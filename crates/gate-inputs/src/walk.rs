//! What a walk of a run's outputs finds below a folder: each file the output globs match, and each they would match but
//! for its name. Each box walks its own folders and lists what it finds here, so both judge a found file alike.

use std::ffi::OsStr;
use std::path::PathBuf;

use crate::glob::Globs;
use crate::manifest::Stamp;

/// A file the walk found: its path relative to the walk's root, `/`-separated, its full path and its stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub full: PathBuf,
    pub stamp: Stamp,
}

/// What a walk found below a folder: the files its globs match, and the files they would match but for a name that is
/// not plain ([`Globs::refuses_name`]), each ordered by relative path.
#[derive(Debug, Default)]
pub struct Walked {
    pub found: Vec<Found>,
    pub refused: Vec<Found>,
}

impl Walked {
    /// The list the file at the relative `path` goes on under `globs`; `None` when it goes on neither. An `unfit` file,
    /// below a folder read only to count or with a name that is not Unicode, goes on `refused` when the globs select it.
    pub fn list_for(&mut self, globs: &Globs, path: &str, unfit: bool) -> Option<&mut Vec<Found>> {
        if unfit {
            globs.selects(path).then_some(&mut self.refused)
        } else if globs.matches(path) {
            Some(&mut self.found)
        } else {
            globs.refuses_name(path).then_some(&mut self.refused)
        }
    }
}

/// The path of the entry `name` of the folder at the relative path `folder`, `/`-separated; a name that is not Unicode
/// is read lossily.
#[must_use]
pub fn relative_path(folder: &str, name: &OsStr) -> String {
    let name = name.to_string_lossy();
    if folder.is_empty() {
        name.into_owned()
    } else {
        format!("{folder}/{name}")
    }
}
