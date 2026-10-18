//! The GPU's 3D use per adapter over a window, from the `GPU Engine` performance counters read through PDH: a query
//! opened and sampled at the window's start ([`Gpu3dWindow::start`]), sampled again at its end
//! ([`Gpu3dWindow::finish`]), and the utilization of every 3D engine instance summed per adapter, at most 100.

use std::collections::BTreeMap;
use std::fmt;
use std::ptr;

use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PDH_NO_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};

/// The counter of every 3D engine of every adapter, by its English path, so a localized system finds it too.
const COUNTER_PATH: &str = r"\GPU Engine(*engtype_3D)\Utilization Percentage";
/// The PDH status of a call that succeeded.
const PDH_OK: u32 = 0;
/// How many times the values are asked for before an array that keeps outgrowing its buffer is an error.
const ARRAY_ATTEMPTS: usize = 4;
/// What an instance name says before its adapter's LUID.
const LUID_TAG: &str = "luid_";
/// The values' buffer is made of `u64` words: an item is a whole number of words and needs no stricter alignment.
const ITEM_FITS_WORDS: () = assert!(
    size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>().is_multiple_of(size_of::<u64>()) && align_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() <= align_of::<u64>()
);

/// One adapter's 3D use over a window.
#[derive(Clone, Debug, PartialEq)]
pub struct AdapterUse {
    /// The adapter's LUID, its two halves in lowercase hex as the instance names give them: `0x00000000_0x00013ec4`.
    pub luid: String,
    /// The utilization of its 3D engines summed, from 0 to 100.
    pub percent: f64,
}

/// A PDH call that failed, or found no 3D engine to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PdhError {
    /// The call, such as `PdhAddEnglishCounterW`.
    pub call: &'static str,
    /// The PDH status it returned, or `PDH_NO_DATA` when it found no 3D engine instance of any adapter.
    pub status: u32,
}

impl fmt::Display for PdhError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} returned 0x{:08X}", self.call, self.status)
    }
}

impl std::error::Error for PdhError {}

/// A PDH query of the 3D engines' utilization, sampled once at the window's start; closed when dropped.
pub struct Gpu3dWindow {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl Gpu3dWindow {
    /// Opens the query, adds the 3D engines' counter and takes the window's first sample.
    ///
    /// # Errors
    ///
    /// The call that failed and its status: no `GPU Engine` counter set on this machine fails the add.
    pub fn start() -> Result<Gpu3dWindow, PdhError> {
        let mut query: PDH_HQUERY = ptr::null_mut();
        // SAFETY: a null data source asks for live data, and `query` is a writable handle that outlives the call.
        check("PdhOpenQueryW", unsafe { PdhOpenQueryW(ptr::null(), 0, &raw mut query) })?;
        let mut window = Gpu3dWindow {
            query,
            counter: ptr::null_mut(),
        };
        let path: Vec<u16> = COUNTER_PATH.encode_utf16().chain([0]).collect();
        // SAFETY: the query is open, `path` is a NUL-terminated UTF-16 string and `counter` a writable handle, both
        // outliving the call.
        let added = unsafe { PdhAddEnglishCounterW(window.query, path.as_ptr(), 0, &raw mut window.counter) };
        check("PdhAddEnglishCounterW", added)?;
        window.collect()?;
        Ok(window)
    }

    /// Takes the window's second sample and reads each adapter's 3D use between the two, adapters in LUID order.
    ///
    /// # Errors
    ///
    /// The call that failed and its status; `PDH_NO_DATA` from `PdhGetFormattedCounterArrayW` when no instance with
    /// an adapter's LUID had a valid value.
    pub fn finish(self) -> Result<Vec<AdapterUse>, PdhError> {
        self.collect()?;
        let values = self.values()?;
        let adapters = adapter_use(values.iter().map(|(name, value)| (name.as_str(), *value)));
        if adapters.is_empty() {
            return Err(PdhError {
                call: "PdhGetFormattedCounterArrayW",
                status: PDH_NO_DATA,
            });
        }
        Ok(adapters)
    }

    /// Samples the query once.
    fn collect(&self) -> Result<(), PdhError> {
        // SAFETY: the query is open for as long as `self` lives.
        check("PdhCollectQueryData", unsafe { PdhCollectQueryData(self.query) })
    }

    /// Every instance of the counter with a valid value, by name, as a double.
    fn values(&self) -> Result<Vec<(String, f64)>, PdhError> {
        const CALL: &str = "PdhGetFormattedCounterArrayW";
        let () = ITEM_FITS_WORDS;
        let item_size = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        for _ in 0..ARRAY_ATTEMPTS {
            let mut bytes = 0u32;
            let mut count = 0u32;
            // SAFETY: a null buffer of zero bytes asks only for the size, written to the two writable counts.
            let sized = unsafe { PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &raw mut bytes, &raw mut count, ptr::null_mut()) };
            match sized {
                PDH_MORE_DATA => {}
                PDH_OK => return Ok(Vec::new()),
                status => return Err(PdhError { call: CALL, status }),
            }
            let wanted = usize::try_from(bytes).unwrap_or(usize::MAX).div_ceil(item_size);
            // Words rather than items, so every byte of the buffer, the items' padding included, starts zeroed.
            let mut buffer = vec![0u64; wanted.saturating_mul(item_size) / size_of::<u64>()];
            let held = buffer.len() * size_of::<u64>();
            bytes = u32::try_from(held).unwrap_or(u32::MAX);
            // SAFETY: `buffer` is a writable array of `bytes` bytes, aligned for the items (`ITEM_FITS_WORDS`), that
            // outlives the call and the reads of the items and the names PDH writes into it.
            let read = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &raw mut bytes,
                    &raw mut count,
                    buffer.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                )
            };
            match read {
                PDH_MORE_DATA => continue,
                PDH_OK => {}
                status => return Err(PdhError { call: CALL, status }),
            }
            let filled = usize::try_from(count).unwrap_or(usize::MAX).min(held / item_size);
            // SAFETY: the buffer is aligned for the items and holds `filled` of them; every byte is initialised (zeroed,
            // then written by PDH), and an item of any bytes is a valid one (a pointer, a status and a plain union).
            let items = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(), filled) };
            let span = Span {
                start: buffer.as_ptr().cast::<u8>() as usize,
                end: buffer.as_ptr().cast::<u8>() as usize + held,
            };
            return Ok(items.iter().filter_map(|item| valid_value(item, span)).collect());
        }
        Err(PdhError {
            call: CALL,
            status: PDH_MORE_DATA,
        })
    }
}

impl Drop for Gpu3dWindow {
    fn drop(&mut self) {
        // SAFETY: the query was opened by `start` and is closed only here, once; closing it also frees its counter.
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// The addresses of the buffer PDH filled, which every name it points to lies inside.
#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

/// An item's instance name and value, when its status says the value is valid and its name lies inside `span`.
fn valid_value(item: &PDH_FMT_COUNTERVALUE_ITEM_W, span: Span) -> Option<(String, f64)> {
    if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA {
        return None;
    }
    let name = item.szName as usize;
    if name < span.start || name >= span.end || !name.is_multiple_of(align_of::<u16>()) {
        return None;
    }
    let units = (span.end - name) / size_of::<u16>();
    // SAFETY: the name starts inside the filled buffer, aligned for UTF-16, and `units` units from it stay inside it.
    let text = unsafe { std::slice::from_raw_parts(item.szName.cast_const(), units) };
    let length = text.iter().position(|&unit| unit == 0)?;
    // SAFETY: the format asked for is `PDH_FMT_DOUBLE`, so the double is the union's field PDH wrote.
    let value = unsafe { item.FmtValue.Anonymous.doubleValue };
    Some((String::from_utf16_lossy(&text[..length]), value))
}

/// `status` as a result naming `call`.
fn check(call: &'static str, status: u32) -> Result<(), PdhError> {
    if status == PDH_OK { Ok(()) } else { Err(PdhError { call, status }) }
}

/// The adapter's LUID an instance name holds, such as `pid_13968_luid_0x00000000_0x00013ec4_phys_0_eng_0_engtype_3D`,
/// its hex in lowercase as `0x00000000_0x00013ec4`; `None` when the name holds none.
fn luid_of(instance: &str) -> Option<String> {
    let lower = instance.to_ascii_lowercase();
    let at = lower.find("luid_0x")?;
    let rest = lower.get(at + LUID_TAG.len()..)?.strip_prefix("0x")?;
    let (high, rest) = hex_run(rest)?;
    let (low, _) = hex_run(rest.strip_prefix("_0x")?)?;
    Some(format!("0x{high}_0x{low}"))
}

/// The hex digits `text` starts with, and the rest; `None` when it starts with none.
fn hex_run(text: &str) -> Option<(&str, &str)> {
    let end = text.find(|unit: char| !unit.is_ascii_hexdigit()).unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

/// The 3D use of each adapter `values` name, each an instance name and its utilization: the values of an adapter's
/// instances summed and held between 0 and 100, adapters in LUID order. A name without a LUID, or a value that is not
/// a number, counts for no adapter.
fn adapter_use<'a>(values: impl IntoIterator<Item = (&'a str, f64)>) -> Vec<AdapterUse> {
    let mut sums: BTreeMap<String, f64> = BTreeMap::new();
    for (name, value) in values {
        if let Some(luid) = luid_of(name).filter(|_| value.is_finite()) {
            *sums.entry(luid).or_default() += value;
        }
    }
    sums.into_iter()
        .map(|(luid, sum)| AdapterUse {
            luid,
            percent: sum.clamp(0.0, 100.0),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_luid_of_a_measured_gpu_instance_name_is_its_two_hex_halves() {
        let name = "pid_13968_luid_0x00000000_0x00013ec4_phys_0_eng_0_engtype_3d";
        assert_eq!(luid_of(name).as_deref(), Some("0x00000000_0x00013ec4"));
    }

    #[test]
    fn a_gpu_instance_name_without_a_luid_has_none() {
        assert_eq!(luid_of("pid_13968_phys_0_eng_0_engtype_3D"), None);
        assert_eq!(luid_of("pid_1_luid_0x_0x1_engtype_3D"), None);
        assert_eq!(luid_of("pid_1_luid_0x00000000_phys_0"), None);
    }

    #[test]
    fn a_gpu_luid_in_upper_or_lower_hex_reads_as_one_adapter() {
        let upper = luid_of("pid_1_luid_0x00000000_0x00013EC4_phys_0_eng_0_engtype_3D");
        let lower = luid_of("pid_2_luid_0x00000000_0x00013ec4_phys_0_eng_1_engtype_3D");
        assert_eq!(upper.as_deref(), Some("0x00000000_0x00013ec4"));
        assert_eq!(upper, lower);
    }

    #[test]
    fn gpu_use_is_summed_per_adapter_and_held_at_100() {
        let values = [
            ("pid_1_luid_0x00000000_0x0000AAAA_phys_0_eng_0_engtype_3D", 70.0),
            ("pid_2_luid_0x00000000_0x0000aaaa_phys_0_eng_0_engtype_3D", 50.0),
            ("pid_3_luid_0x00000000_0x0000bbbb_phys_0_eng_0_engtype_3D", 12.5),
            ("pid_3_luid_0x00000000_0x0000bbbb_phys_0_eng_1_engtype_3D", 0.5),
            ("pid_4_phys_0_eng_0_engtype_3D", 40.0),
            ("pid_5_luid_0x00000000_0x0000cccc_phys_0_eng_0_engtype_3D", f64::NAN),
        ];
        let adapters = adapter_use(values);
        assert_eq!(
            adapters,
            [
                AdapterUse {
                    luid: "0x00000000_0x0000aaaa".to_owned(),
                    percent: 100.0,
                },
                AdapterUse {
                    luid: "0x00000000_0x0000bbbb".to_owned(),
                    percent: 13.0,
                },
            ]
        );
    }

    #[test]
    fn a_pdh_error_names_its_call_and_status_in_hex() {
        let error = PdhError {
            call: "PdhAddEnglishCounterW",
            status: 0xC000_0BB8,
        };
        assert_eq!(error.to_string(), "PdhAddEnglishCounterW returned 0xC0000BB8");
    }

    #[test]
    fn a_live_gpu_window_yields_adapters_or_a_pdh_error() {
        let read = Gpu3dWindow::start().and_then(|window| {
            thread::sleep(Duration::from_millis(100));
            window.finish()
        });
        match read {
            Ok(adapters) => {
                assert_ne!(adapters.len(), 0, "a read that found no adapter is an error");
                assert!(adapters.iter().all(|adapter| (0.0..=100.0).contains(&adapter.percent)));
                assert!(adapters.iter().all(|adapter| adapter.luid.starts_with("0x")));
            }
            Err(error) => assert!(error.to_string().contains(" returned 0x"), "{error}"),
        }
    }
}
