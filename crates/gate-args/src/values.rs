//! Value validation, shared by the CLI and the compatibility layer.

use crate::problem::{MAX_WHOLE_NUMBER, Problem};
use crate::request::Slot;

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

pub(crate) fn above_zero(option: &str, value: &str) -> Result<u64, Problem> {
    match whole(value) {
        Whole::Number(number) if number > 0 => Ok(number),
        Whole::TooLarge => Err(too_large(option, value)),
        Whole::Number(_) | Whole::Invalid => Err(Problem::NotAboveZero {
            option: option.to_owned(),
            value: value.to_owned(),
        }),
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

pub(crate) fn slot(option: &str, value: &str) -> Result<Slot, Problem> {
    [Slot::Heavy, Slot::Light, Slot::Critical]
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
