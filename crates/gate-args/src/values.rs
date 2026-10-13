//! Value validation, shared by the CLI and the compatibility layer.

use crate::problem::{MAX_WHOLE_NUMBER, Problem};
use crate::request::{Ceiling, Slot};

/// How a value reads as a whole number.
enum Whole {
    /// Not a run of ASCII digits: a sign, a space, a fraction, a word or nothing.
    Invalid,
    /// Digits for a number above [`MAX_WHOLE_NUMBER`], however many.
    TooLarge,
    Number(u64),
}

fn whole(value: &str) -> Whole {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Whole::Invalid;
    }
    match value.parse::<u64>() {
        Ok(number) if number <= MAX_WHOLE_NUMBER => Whole::Number(number),
        _ => Whole::TooLarge,
    }
}

fn too_large(option: &str, value: &str) -> Problem {
    Problem::TooLarge {
        option: option.to_owned(),
        value: value.to_owned(),
    }
}

/// The longest `gate burn` may run: the owner's 120 s burn cap plus 60 s of margin.
pub(crate) const MAX_BURN_SECONDS: u32 = 180;

pub(crate) fn above_zero(option: &str, value: &str) -> Result<u64, Problem> {
    above_zero_shown(option, value, value)
}

/// The whole number above 0 that `digits` spell, a problem naming the value as given, `shown`.
fn above_zero_shown(option: &str, digits: &str, shown: &str) -> Result<u64, Problem> {
    match whole(digits) {
        Whole::Number(number) if number > 0 => Ok(number),
        Whole::TooLarge => Err(too_large(option, shown)),
        Whole::Number(_) | Whole::Invalid => Err(Problem::NotAboveZero {
            option: option.to_owned(),
            value: shown.to_owned(),
        }),
    }
}

/// The prefix of an auto ceiling's value, read in any ASCII case.
const AUTO_PREFIX: &str = "auto:";

/// The subcommand CLI's `--timeout`: a whole number above 0, or `auto:<n>` for a floor `n` read the same way, `auto` in
/// any ASCII case. A problem names the whole value as given; a bare `auto` needs its floor.
pub(crate) fn ceiling(option: &str, value: &str) -> Result<Ceiling, Problem> {
    if value.eq_ignore_ascii_case("auto") {
        return Err(Problem::AutoNeedsFloor(option.to_owned()));
    }
    match value.get(..AUTO_PREFIX.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(AUTO_PREFIX) => above_zero_shown(option, &value[AUTO_PREFIX.len()..], value).map(Ceiling::Auto),
        _ => above_zero(option, value).map(Ceiling::Given),
    }
}

pub(crate) fn zero_or_above(option: &str, value: &str) -> Result<u64, Problem> {
    match whole(value) {
        Whole::Number(number) => Ok(number),
        Whole::TooLarge => Err(too_large(option, value)),
        Whole::Invalid => Err(Problem::NotZeroOrAbove {
            option: option.to_owned(),
            value: value.to_owned(),
        }),
    }
}

/// A whole number from `low` to `high`, for an option bounded at both ends such as `gate burn`'s `--seconds`. Every
/// value outside the range takes the one line, one above the whole-number ceiling included.
pub(crate) fn in_range(option: &str, value: &str, low: u32, high: u32) -> Result<u32, Problem> {
    let out_of_range = || Problem::NotInRange {
        option: option.to_owned(),
        value: value.to_owned(),
        low,
        high,
    };
    match whole(value) {
        Whole::Number(number) if u64::from(low) <= number && number <= u64::from(high) => u32::try_from(number).map_err(|_| out_of_range()),
        Whole::Number(_) | Whole::TooLarge | Whole::Invalid => Err(out_of_range()),
    }
}

/// A whole number above 0 as a `u32`, [`above_zero`] for a count no option may set as high as a `u32` holds.
pub(crate) fn above_zero_u32(option: &str, value: &str) -> Result<u32, Problem> {
    let number = above_zero(option, value)?;
    // `above_zero` refuses anything above `MAX_WHOLE_NUMBER`, which is below `u32::MAX`.
    u32::try_from(number).map_err(|_| Problem::NotAboveZero {
        option: option.to_owned(),
        value: value.to_owned(),
    })
}

/// The calls `--test-remote-interrupt-at` may name.
#[cfg(feature = "test-seams")]
pub(crate) const INTERRUPTED_CALLS: &[&str] = &["preflight", "run", "outputs", "install", "sdks", "stop"];
/// The calls `--test-remote-watch-call` may name.
#[cfg(feature = "test-seams")]
pub(crate) const WATCHED_CALLS: &[&str] = &["preflight", "push", "run", "ls-remote", "fetch", "outputs"];

/// A call of the remote client a seam names, one of `calls`, as written.
#[cfg(feature = "test-seams")]
pub(crate) fn remote_call(option: &str, value: &str, calls: &'static [&'static str]) -> Result<String, Problem> {
    if calls.contains(&value) {
        return Ok(value.to_owned());
    }
    Err(Problem::BadRemoteCall {
        option: option.to_owned(),
        value: value.to_owned(),
        calls,
    })
}

pub(crate) fn slot(option: &str, value: &str) -> Result<Slot, Problem> {
    [Slot::Heavy, Slot::Light, Slot::Critical, Slot::None]
        .into_iter()
        .find(|slot| slot.as_str().eq_ignore_ascii_case(value))
        .ok_or_else(|| Problem::BadSlot {
            option: option.to_owned(),
            value: value.to_owned(),
        })
}

/// A switch's colon value: `true`, `$true`, `false` or `$false`, in any ASCII case.
pub(crate) fn true_or_false(option: &str, value: &str) -> Result<bool, Problem> {
    let bare = value.strip_prefix('$').unwrap_or(value);
    if bare.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if bare.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        Err(Problem::NotTrueOrFalse {
            option: option.to_owned(),
            value: value.to_owned(),
        })
    }
}

/// A process id: a run of ASCII digits for a number from 1 to `u32::MAX`.
pub(crate) fn pid(value: &str) -> Result<u32, Problem> {
    let digits = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    digits
        .then(|| value.parse::<u32>().ok())
        .flatten()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| Problem::BadPid(value.to_owned()))
}
