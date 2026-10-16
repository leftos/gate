//! The repo name and the builder's work-folder key, as `remote.ps1`'s `Get-RemoteName` and `Get-RemoteKey` compute
//! them.

use sha2::{Digest, Sha256};

use crate::like::lower_char;

/// A folder name cut to what a git ref component and a remote path both take: leading dots dropped, then every
/// character other than an ASCII letter, digit, `-` or `_` turned into `-` (one for each UTF-16 unit it takes, as
/// .NET counts it); `repo` when nothing is left.
#[must_use]
pub fn repo_name(folder: &str) -> String {
    let mut name = String::with_capacity(folder.len());
    for character in folder.trim_start_matches('.').chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            name.push(character);
        } else {
            name.extend(std::iter::repeat_n('-', character.len_utf16()));
        }
    }
    if name.is_empty() { "repo".to_owned() } else { name }
}

/// The work folder a worktree root has on the builder: the root's last segment as a [`repo_name`], `-`, and the first
/// 8 lower-case hex digits of the SHA-256 of the root in lower case, as UTF-8. `root` is the worktree root as git
/// gives it (`X:/dev/repo.wt/a`).
#[must_use]
pub fn work_folder_key(root: &str) -> String {
    let lowered: String = root.chars().map(lower_char).collect();
    let digest = Sha256::digest(lowered.as_bytes());
    let mut key = repo_name(&leaf(root));
    key.push('-');
    key.extend(digest.iter().take(4).map(|byte| format!("{byte:02x}")));
    key
}

/// The device names Windows reserves in every folder, in upper case.
const DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
    "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Whether `character` is one a folder name the builder lays out is made of ([`is_layout_name`]): an ASCII letter or
/// digit, `.`, `_` or `-`.
#[must_use]
pub fn is_layout_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

/// Whether the builder can lay out a folder named `name` beside another: one or more ASCII letters, digits, `.`, `_`
/// and `-` ([`is_layout_char`]), neither starting nor ending with a dot (so never `.` or `..`, and never a name Windows would cut), and not
/// a reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`) in any ASCII case, whatever
/// follows its first dot (`nul.txt`).
#[must_use]
pub fn is_layout_name(name: &str) -> bool {
    let characters = !name.is_empty() && name.chars().all(is_layout_char);
    let device = name
        .split('.')
        .next()
        .is_some_and(|stem| DEVICE_NAMES.iter().any(|device| device.eq_ignore_ascii_case(stem)));
    characters && !name.starts_with('.') && !name.ends_with('.') && !device
}

/// The last segment of a path, as `Split-Path -Leaf` gives it: separators at the end are ignored, and a drive root
/// (`X:/`) is its own leaf, written `X:\`.
fn leaf(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let drive = matches!(trimmed.as_bytes(), [letter, b':'] if letter.is_ascii_alphabetic());
    if drive && trimmed.len() < path.len() {
        return format!("{trimmed}\\");
    }
    trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed).to_owned()
}
