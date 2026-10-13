//! A `.cmd` or `.bat` target: its words quoted for cmd.exe (C5), its command line through cmd.exe, and the words the
//! gate refuses because cmd.exe would split or expand them whatever the quoting.

use std::fmt;

use crate::cmdline::{QUOTE, SPACE, is_plain, quote_arg, wide, wrapped};

/// What cmd.exe reads as its own syntax or splits a batch file's parameters at.
const CMD_SPECIALS: [char; 10] = ['&', '|', '<', '>', '^', '(', ')', ',', ';', '='];

/// The names cmd.exe gives a value of its own, so it expands them though the block it is started with sets no such
/// variable: `COMSPEC`, `PATHEXT` and `PROMPT`, which it sets for itself when the block lacks them, and its dynamic
/// names.
const SELF_SET_NAMES: [&str; 13] = [
    "COMSPEC",
    "PATHEXT",
    "PROMPT",
    "CD",
    "DATE",
    "TIME",
    "RANDOM",
    "ERRORLEVEL",
    "CMDCMDLINE",
    "CMDEXTVERSION",
    "HIGHESTNUMANODENUMBER",
    "__CD__",
    "__APPDIR__",
];

const PERCENT: u16 = 0x25;
const CR: u16 = 0x0D;
const LF: u16 = 0x0A;
const NUL: u16 = 0x00;

/// Whether Windows starts `path` through cmd.exe: the extension of its file name, read after the trailing dots and
/// spaces Windows drops from a file name, is `.cmd` or `.bat` in any case.
#[must_use]
pub fn is_batch_target(path: &str) -> bool {
    let file_name = path.rsplit(['\\', '/']).next().unwrap_or(path).trim_end_matches(['.', ' ']);
    file_name
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat"))
}

/// One word of a batch target's line, as the PowerShell gate's `Format-BatchArgument` writes it.
///
/// A non-empty word holding no whitespace, no `"` and none of `& | < > ^ ( ) , ; =` is written as it is. Any other word
/// is wrapped in quotes, `"` inside becomes `""`, and a backslash run before a quote or at the end is doubled, for the
/// C-runtime program the batch file hands `%*` to. `/v:off` keeps `!` plain.
#[must_use]
pub fn quote_batch_arg(word: &[u16]) -> Vec<u16> {
    if is_plain(word, &CMD_SPECIALS) {
        word.to_vec()
    } else {
        wrapped(word, &[QUOTE, QUOTE])
    }
}

/// [`quote_batch_arg`] over a string.
#[must_use]
pub fn quote_batch_arg_str(word: &str) -> String {
    // Quoting adds only ASCII to valid UTF-16, so the lossy decode loses nothing.
    String::from_utf16_lossy(&quote_batch_arg(&wide(word)))
}

/// The command line of a batch target, `words` holding the batch file first: `<shell> /d /v:off /s /c "<line>"`, the
/// shell (the caller's `ComSpec`) through the C-runtime quoting and each word through [`quote_batch_arg`], joined by
/// single spaces.
#[must_use]
pub fn batch_command_line<W: AsRef<[u16]>>(shell: &[u16], words: &[W]) -> Vec<u16> {
    let quoted: Vec<Vec<u16>> = words.iter().map(|word| quote_batch_arg(word.as_ref())).collect();
    let mut line = quote_arg(shell);
    line.extend(wide(" /d /v:off /s /c \""));
    line.extend(quoted.join(&SPACE));
    line.push(QUOTE);
    line
}

/// A word a batch target cannot take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRefusal {
    /// The word, as given.
    pub word: Vec<u16>,
}

impl fmt::Display for BatchRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gate: a .cmd or .bat command cannot take a word holding a line break or a %NAME% cmd.exe would expand: {}",
            String::from_utf16_lossy(&self.word)
        )
    }
}

/// The first of a batch target's words, the batch file's own path included, that cmd.exe would split, cut or expand.
///
/// That is a word holding a CR, an LF or a U+0000, or a `%…%` pair whose name cmd.exe expands: a name starting with
/// `=` (cmd's hidden `=ExitCode`, `=C:` variables, which no environment listing shows), a name `is_set` answers for, a
/// name cmd.exe expands unset (`%CD%`, `%RANDOM%`, `%PROMPT%`, ...), or the `%NAME:~a,b%` or `%NAME:a=b%` form of a set or
/// dynamic `NAME` (both the whole name and the part before its first `:` are looked up). A lone `%` and an unset name
/// pass. An unset `%A%` stays as written and its closing `%` can open the next name, so the scan resumes there: `%A%B%`
/// with `A` unset and `B` set is refused.
///
/// `is_set` must answer for the exact environment block the command is started with, comparing names
/// case-insensitively over all of Unicode, as Windows does; a name it misses is passed to cmd.exe to expand.
#[must_use]
pub fn batch_refusal<W: AsRef<[u16]>>(words: &[W], is_set: impl Fn(&str) -> bool) -> Option<BatchRefusal> {
    words
        .iter()
        .map(AsRef::as_ref)
        .find(|word| word.iter().any(|&unit| matches!(unit, CR | LF | NUL)) || expands_a_name(word, &is_set))
        .map(|word| BatchRefusal { word: word.to_vec() })
}

fn expands_a_name(word: &[u16], is_set: &impl Fn(&str) -> bool) -> bool {
    let mut at = 0;
    while let Some(open) = position_from(word, at) {
        let Some(close) = position_from(word, open + 1) else {
            return false;
        };
        if cmd_expands(&String::from_utf16_lossy(&word[open + 1..close]), is_set) {
            return true;
        }
        at = close;
    }
    false
}

/// Whether cmd.exe expands `%name%`: the name starts with `=`, or it is set, or the variable before any `:` (the
/// substring and substitution forms) is set or dynamic.
fn cmd_expands(name: &str, is_set: &impl Fn(&str) -> bool) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.starts_with('=') || is_set(name) {
        return true;
    }
    let variable = name.split_once(':').map_or(name, |(variable, _)| variable);
    !variable.is_empty() && (is_set(variable) || SELF_SET_NAMES.iter().any(|own| own.eq_ignore_ascii_case(variable)))
}

fn position_from(word: &[u16], from: usize) -> Option<usize> {
    word.get(from..)?.iter().position(|&unit| unit == PERCENT).map(|found| from + found)
}
