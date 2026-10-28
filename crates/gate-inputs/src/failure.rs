//! What a failed run brings back, judged alike by the PC and both boxes: the set of globs it narrows its outputs to
//! ([`failed_run_globs`]), and the caps its failure outputs are held to as the box chooses what to pack. A file only a
//! failure output glob brings back is left on the builder past a size of its own, and such files together carry no
//! more than a total, taken in walk order. A file a test reports glob brings back is never capped here. Both boxes
//! choose through [`Capping`] and [`capped`]: the Windows box with [`Caps::BUILT_IN`] or the smaller caps of its test
//! seam `--test-box-failure-cap-bytes`, the Unix box always with [`Caps::BUILT_IN`].

use gate_router::escaped;

use crate::glob::Globs;
use crate::manifest::Chosen;

/// The bytes in one mebibyte.
const MIB: u64 = 1024 * 1024;
/// The bytes in one gibibyte.
const GIB: u64 = 1024 * MIB;

/// The caps on a failed run's failure outputs, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// The most one file only a failure output glob brings back may hold.
    pub file: u64,
    /// The most such files may hold together.
    pub total: u64,
}

impl Caps {
    /// The caps a box holds a failed run's failure outputs to: 256 MiB a file, 1 GiB in all.
    pub const BUILT_IN: Caps = Caps { file: 256 * MIB, total: GIB };

    /// The caps of the Windows box's test seam `--test-box-failure-cap-bytes <n>`: `n` bytes a file, four times that in
    /// all. The Unix box reads no such seam.
    #[must_use]
    pub fn of_seam(file: u64) -> Caps {
        Caps {
            file,
            total: file.saturating_mul(4),
        }
    }
}

/// The globs a failed run brings back: those of `outputs`, the run's output globs, that name a `TestResults` folder
/// ([`Globs::test_results_only`]), its test reports, followed by the failure output globs `failure`; and the capping its
/// pack is held to: [`Capping::BesideReports`] with `caps` when the set holds a failure output glob, else
/// [`Capping::Uncapped`]. `None` when no output glob names a `TestResults` folder and there are no failure output
/// globs.
#[must_use]
pub fn failed_run_globs(outputs: &Globs, failure: Option<&Globs>, caps: Caps) -> Option<(Globs, Capping)> {
    let reports = outputs.test_results_only();
    let globs = failed_run_set(reports.as_ref(), failure)?;
    let capping = if failure.is_some() {
        Capping::BesideReports { reports, caps }
    } else {
        Capping::Uncapped
    };
    Some((globs, capping))
}

/// The set of globs a failed run brings back: its test reports' globs `reports` ([`Globs::test_results_only`] of its
/// output globs) followed by its failure output globs `failure`; `None` when it has neither. The PC, which packs
/// nothing and so holds nothing to caps, narrows a failed run's install to it directly.
#[must_use]
pub fn failed_run_set(reports: Option<&Globs>, failure: Option<&Globs>) -> Option<Globs> {
    match (reports, failure) {
        (None, None) => None,
        (Some(reports), None) => Some(reports.clone()),
        (None, Some(failure)) => Some(failure.clone()),
        (Some(reports), Some(failure)) => Some(reports.joined(failure)),
    }
}

/// Whether a pack holds the files it chooses to the failure output caps.
pub enum Capping {
    /// No cap: a passing command's outputs, or a failed one's test reports alone.
    Uncapped,
    /// A failed command's set holding failure output globs: every file but those its test reports' globs `reports`
    /// bring back is held to `caps`.
    BesideReports { reports: Option<Globs>, caps: Caps },
}

impl Capping {
    /// The files of `chosen` a pack under this capping packs, and the lines saying each it leaves on the builder for a
    /// cap: all of them, and no line, unless it holds them [`Capping::BesideReports`] ([`capped`]).
    #[must_use]
    pub fn capped<'found>(&self, chosen: Vec<Chosen<'found>>) -> (Vec<Chosen<'found>>, Vec<String>) {
        match self {
            Capping::Uncapped => (chosen, Vec::new()),
            Capping::BesideReports { reports, caps } => {
                let capped = capped(chosen, reports.as_ref(), *caps);
                (capped.kept, capped.left)
            }
        }
    }
}

/// What [`capped`] keeps of a failed run's chosen files, and the line saying each it left on the builder.
#[derive(Debug, PartialEq, Eq)]
pub struct Capped<'found> {
    pub kept: Vec<Chosen<'found>>,
    pub left: Vec<String>,
}

/// The files of `chosen`, a failed run's choice in walk order, held to `caps`: a file `reports`, the run's test reports
/// globs, brings back is kept whatever its size and counts toward no cap; every other file, one only a failure output
/// glob brings back, is left past [`Caps::file`], and the rest are taken in walk order until the next would carry them
/// past [`Caps::total`], when it and every later one is left. Each file left is named in a line of its own
/// ([`left_line`]), in walk order.
#[must_use]
pub fn capped<'found>(chosen: Vec<Chosen<'found>>, reports: Option<&Globs>, caps: Caps) -> Capped<'found> {
    let mut kept = Vec::with_capacity(chosen.len());
    let mut left = Vec::new();
    let mut carried: u64 = 0;
    let mut full = false;
    for item in chosen {
        if reports.is_some_and(|reports| reports.matches(&item.file.path)) {
            kept.push(item);
            continue;
        }
        let size = item.file.stamp.size;
        if size > caps.file {
            left.push(left_line(&item.file.path, size, &size_text(caps.file)));
            continue;
        }
        full = full || carried.saturating_add(size) > caps.total;
        if full {
            left.push(left_line(&item.file.path, size, &format!("{} in all", size_text(caps.total))));
            continue;
        }
        carried += size;
        kept.push(item);
    }
    Capped { kept, left }
}

/// The line saying the failure output at the relative `path`, `size` bytes, was left on the builder past the cap `cap`:
/// its size in whole MiB, rounded up, so a file just past a cap never reads as the cap's own size.
fn left_line(path: &str, size: u64, cap: &str) -> String {
    format!(
        "gate: remote failure output left on the builder: {} ({} MiB, past the {cap} cap)",
        escaped(path),
        size.div_ceil(MIB)
    )
}

/// A cap of `bytes` as its line names it: whole GiB, else whole MiB, else bytes.
fn size_text(bytes: u64) -> String {
    if bytes != 0 && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else if bytes != 0 && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a glob that cannot be matched must fail the test loudly")]
mod tests {
    use std::path::PathBuf;

    use super::{Capped, Capping, Caps, GIB, MIB, capped, failed_run_globs};
    use crate::glob::Globs;
    use crate::manifest::{Chosen, Stamp};
    use crate::walk::Found;

    /// A walked file at `path` of `size` bytes.
    fn found(path: &str, size: u64) -> Found {
        Found {
            path: path.to_owned(),
            full: PathBuf::from(path),
            stamp: Stamp { size, modified: 0 },
        }
    }

    /// The paths `capped` keeps of `files` under `reports`' globs and `caps`, and its lines.
    fn kept(files: &[Found], reports: Option<&[&str]>, caps: Caps) -> (Vec<String>, Vec<String>) {
        let reports = reports.map(|globs| {
            let owned: Vec<String> = globs.iter().map(|glob| (*glob).to_owned()).collect();
            Globs::new(&owned).expect("the globs can be matched")
        });
        let Capped { kept, left } = capped(files.iter().map(Chosen::from).collect(), reports.as_ref(), caps);
        (kept.iter().map(|item| item.file.path.clone()).collect(), left)
    }

    #[test]
    fn a_failed_run_brings_back_a_tmp_dump_its_failure_glob_names() {
        let outputs = Globs::new(&["**/bin/**".to_owned(), "out/TestResults/*.trx".to_owned()]).expect("the globs can be matched");
        let failure = Globs::new(&[".tmp/dumps/**".to_owned()]).expect("the glob can be matched");
        let (globs, capping) = failed_run_globs(&outputs, Some(&failure), Caps::BUILT_IN).expect("the run has failure globs");
        assert!(globs.matches(".tmp/dumps/r1/a.dmp"), "the failure glob naming .tmp brings the dump back");
        assert!(globs.matches("out/TestResults/r.trx"), "beside the test reports");
        for never in [".tmp/other/a.dmp", "bin/a.dll", ".tmp/dumps/x/.tmp/a.dmp"] {
            assert!(!globs.matches(never), "{never} is not in the failed run's set");
        }
        assert!(matches!(capping, Capping::BesideReports { .. }), "the dump is held to the failure caps");
    }

    #[test]
    fn a_file_over_the_per_file_cap_is_left_and_named() {
        let files = [found("dumps/a.dmp", 10), found("dumps/b.dmp", 11)];
        let (kept, left) = kept(&files, None, Caps::of_seam(10));
        assert_eq!(kept, ["dumps/a.dmp"], "a file at the cap is kept");
        assert_eq!(
            left,
            ["gate: remote failure output left on the builder: dumps/b.dmp (1 MiB, past the 10 bytes cap)"]
        );
    }

    #[test]
    fn the_files_past_the_total_are_left_in_walk_order() {
        let files = [
            found("d/1.dmp", 4),
            found("d/2.dmp", 4),
            found("d/3.dmp", 4),
            found("d/4.dmp", 4),
            found("d/5.dmp", 1),
        ];
        let (kept, left) = kept(&files, None, Caps { file: 4, total: 10 });
        assert_eq!(kept, ["d/1.dmp", "d/2.dmp"]);
        assert_eq!(
            left,
            [
                "gate: remote failure output left on the builder: d/3.dmp (1 MiB, past the 10 bytes in all cap)",
                "gate: remote failure output left on the builder: d/4.dmp (1 MiB, past the 10 bytes in all cap)",
                "gate: remote failure output left on the builder: d/5.dmp (1 MiB, past the 10 bytes in all cap)",
            ],
            "once a file is past the total, every later one is left, a smaller one included"
        );
    }

    #[test]
    fn a_file_over_the_per_file_cap_counts_toward_no_total() {
        let files = [found("d/big.dmp", 50), found("d/a.dmp", 4), found("d/b.dmp", 4)];
        let (kept, left) = kept(&files, None, Caps { file: 4, total: 8 });
        assert_eq!(kept, ["d/a.dmp", "d/b.dmp"]);
        assert_eq!(left.len(), 1, "{left:?}");
    }

    #[test]
    fn a_test_reports_file_is_never_capped_nor_counted() {
        let files = [
            found("out/TestResults/big.trx", 500),
            found("d/a.dmp", 4),
            found("out/TestResults/more.trx", 500),
            found("d/b.dmp", 4),
        ];
        let (kept, left) = kept(&files, Some(&["out/TestResults/*.trx"]), Caps { file: 4, total: 8 });
        assert_eq!(kept, ["out/TestResults/big.trx", "d/a.dmp", "out/TestResults/more.trx", "d/b.dmp"]);
        assert_eq!(left, Vec::<String>::new());
    }

    #[test]
    fn the_built_in_caps_are_named_as_256_mib_and_1_gib_in_all() {
        let files = [found("d/huge.dmp", 300 * MIB), found("d/a.dmp", 200 * MIB)];
        let caps = Caps {
            total: 100 * MIB,
            ..Caps::BUILT_IN
        };
        let (_, left) = kept(&files, None, caps);
        assert_eq!(
            left,
            [
                "gate: remote failure output left on the builder: d/huge.dmp (300 MiB, past the 256 MiB cap)",
                "gate: remote failure output left on the builder: d/a.dmp (200 MiB, past the 100 MiB in all cap)",
            ]
        );
        assert_eq!(Caps::BUILT_IN, Caps { file: 256 * MIB, total: GIB });
        let (_, left) = kept(&[found("d/x.dmp", GIB)], None, Caps { file: GIB, total: GIB - 1 });
        assert_eq!(
            left,
            ["gate: remote failure output left on the builder: d/x.dmp (1024 MiB, past the 1073741823 bytes in all cap)"]
        );
        let (_, left) = kept(&[found("d/y.dmp", 2 * GIB)], None, Caps::BUILT_IN);
        assert_eq!(
            left,
            ["gate: remote failure output left on the builder: d/y.dmp (2048 MiB, past the 256 MiB cap)"]
        );
    }

    #[test]
    fn a_path_is_named_with_its_control_characters_escaped() {
        let (_, left) = kept(&[found("d/a\u{1b}.dmp", 11)], None, Caps::of_seam(10));
        assert_eq!(
            left,
            [r"gate: remote failure output left on the builder: d/a\u{1b}.dmp (1 MiB, past the 10 bytes cap)"]
        );
    }

    #[test]
    fn the_seam_s_total_is_four_times_its_file_cap() {
        assert_eq!(Caps::of_seam(10), Caps { file: 10, total: 40 });
    }
}
