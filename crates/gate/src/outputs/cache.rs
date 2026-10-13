//! The PC's hash cache: the hashes of a worktree's output files from its last built outputs manifest, each kept with
//! the stamp it was read at, so the next build reuses the hash of a file whose size and last write still match instead
//! of reading the file again. The cache is a hint checked against each file's stamp: one that cannot be read gives no
//! hash and is replaced by the next save, and one that cannot be written is left as it was, either costing one full
//! hash and nothing else.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::outputs::manifest::{Held, MANIFEST_LIMIT, Manifest};
use crate::outputs::walk::Stamp;

/// How long before a build's walk began a file's last write must be for its hash to be kept: 2 s, in the 100 ns ticks
/// of a Windows file time. A file written closer to the build could be written again within the same tick at the same
/// size, keeping its stamp over other bytes.
pub(crate) const WINDOW: u64 = 20_000_000;

/// The Unix epoch as a Windows file time: the 100 ns ticks from 1601 to 1970.
const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

/// The hashes a build may reuse: each file by its path, found without regard to case, with the stamp its hash was read
/// at. Saved as an outputs manifest line ([`Manifest::line`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HashCache {
    held: Manifest,
}

impl HashCache {
    /// The cache saved at `path`, empty when there is none. `None` when the file is there but cannot be read, is longer
    /// than [`MANIFEST_LIMIT`] or is not an outputs manifest line: it gives no hash, and the next save replaces it.
    pub(crate) fn load(path: &Path) -> Option<HashCache> {
        // A cache that cannot be read costs the build its hashes, never the run its end.
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Some(HashCache::default()),
            Err(_) => return None,
        };
        let held = Manifest::parse(&read_line(file)?).ok()?;
        Some(HashCache { held })
    }

    /// What the cache holds at `path`, found without regard to case: the stamp its hash was read at and that hash, or
    /// nothing when it holds no entry for the path. A caller takes the hash only when that stamp is the one the file's
    /// handle reads.
    pub(crate) fn held(&self, path: &str) -> Option<&Held> {
        self.held.get(path)
    }

    /// Keeps `sha256` as the hash of the file at `path`, whose stamp `stamp` a handle read before the hash, kept by a
    /// build whose walk began at the file time `started`, when the stamp's last write is at least [`WINDOW`] before it;
    /// a last write after it is never kept.
    pub(crate) fn keep(&mut self, path: &str, stamp: Stamp, sha256: &str, started: u64) {
        let settled = stamp.modified.checked_add(WINDOW).is_some_and(|end| end <= started);
        if settled {
            let held = Held {
                stamp,
                sha256: sha256.to_owned(),
            };
            self.held.insert(path.to_owned(), held);
        }
    }

    /// Saves the cache at `path`, its folder made when missing, through a temporary sibling `<path>.<pid>.tmp` moved
    /// over it, so a reader never sees a torn file. A save that fails removes its temporary sibling and is not said.
    pub(crate) fn save(&self, path: &Path) {
        let temp = temp_of(path);
        if write_then_rename(&temp, path, &self.held.line()).is_err() {
            // A cache that cannot be saved costs the next build its hashes, never the run its end; a temporary
            // sibling that cannot be removed either is overwritten by the next save of this process id.
            let _ = fs::remove_file(&temp);
        }
    }
}

/// The line `file` holds, when it can be read and is at most [`MANIFEST_LIMIT`] bytes long.
fn read_line(file: File) -> Option<String> {
    let limit = u64::try_from(MANIFEST_LIMIT).ok()?;
    if file.metadata().ok()?.len() > limit {
        return None;
    }
    let mut line = String::new();
    file.take(limit.saturating_add(1)).read_to_string(&mut line).ok()?;
    (line.len() <= MANIFEST_LIMIT).then_some(line)
}

/// Writes `line` to `temp` and moves it over `path`, making `path`'s folder when missing.
fn write_then_rename(temp: &Path, path: &Path, line: &str) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    fs::write(temp, line)?;
    fs::rename(temp, path)
}

/// The temporary sibling a save of `path` writes first: `<path>.<pid>.tmp`.
fn temp_of(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", std::process::id()));
    PathBuf::from(temp)
}

/// Now, as a Windows file time. 0 when the clock reads before 1970 or past what a file time holds: no file is written
/// before it, so a build then keeps no hash.
pub(crate) fn file_time_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_nanos() / 100).ok())
        .and_then(|ticks| ticks.checked_add(UNIX_EPOCH_TICKS))
        .unwrap_or(0)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::PathBuf;

    use super::{HashCache, WINDOW, file_time_now};
    use crate::outputs::glob::Globs;
    use crate::outputs::manifest::MANIFEST_LIMIT;
    use crate::outputs::walk::{Found, Stamp, walk};

    /// A build's start, as a file time.
    const STARTED: u64 = 133_000_000_000_000_000;

    /// A SHA-256 of `digit` repeated.
    fn sha(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    fn stamp(size: u64, modified: u64) -> Stamp {
        Stamp { size, modified }
    }

    fn found(path: &str, size: u64, modified: u64) -> Found {
        Found {
            path: path.to_owned(),
            full: PathBuf::from(format!(r"C:\work\{path}")),
            stamp: stamp(size, modified),
        }
    }

    /// A cache holding each of `files` with the hash of its digit, kept by a build that began long after each was
    /// written.
    fn cache_of(files: &[(&Found, char)]) -> HashCache {
        let mut cache = HashCache::default();
        for (file, digit) in files {
            cache.keep(&file.path, file.stamp, &sha(*digit), u64::MAX);
        }
        cache
    }

    /// A scratch folder for one test, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-outputs-cache-{}-{name}", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    /// The names of the entries of `folder`, sorted.
    fn names_in(folder: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .expect("the folder can be listed")
            .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The cache holds an entry by its path, found without regard to case, with the stamp and the hash it was kept at,
    /// and holds nothing for a path it lacks: whether an entry is taken is the caller's comparison of that stamp.
    #[test]
    fn an_entry_is_found_by_its_path_without_regard_to_case() {
        let kept = found("Bin/A.dll", 3, STARTED);
        let cache = cache_of(&[(&kept, 'f')]);

        assert_eq!(
            cache.held(&kept.path).map(|held| (held.stamp, held.sha256.as_str())),
            Some((stamp(3, STARTED), sha('f').as_str()))
        );
        assert_eq!(
            cache.held("bin/a.DLL").map(|held| held.stamp),
            Some(stamp(3, STARTED)),
            "a path in another case"
        );
        assert_eq!(cache.held("Bin/B.dll"), None, "a path the cache lacks");
    }

    #[test]
    fn an_entry_inside_the_window_of_the_build_start_is_not_kept() {
        let settled = found("bin/settled.dll", 1, STARTED - WINDOW);
        let racy = found("bin/racy.dll", 1, STARTED - WINDOW + 1);
        let at_start = found("bin/at-start.dll", 1, STARTED);
        let ahead = found("bin/ahead.dll", 1, STARTED + 1);
        let past_any_clock = found("bin/past.dll", 1, u64::MAX);
        let mut cache = HashCache::default();
        for file in [&settled, &racy, &at_start, &ahead, &past_any_clock] {
            cache.keep(&file.path, file.stamp, &sha('a'), STARTED);
        }

        assert_eq!(WINDOW, 20_000_000, "2 s of 100 ns ticks");
        assert_eq!(
            cache.held(&settled.path).map(|held| (held.stamp, held.sha256.as_str())),
            Some((stamp(1, STARTED - WINDOW), sha('a').as_str())),
            "written the whole window before the build"
        );
        assert_eq!(cache.held(&racy.path), None, "written inside the window");
        assert_eq!(cache.held(&at_start.path), None, "written as the build began");
        assert_eq!(cache.held(&ahead.path), None, "written after the build began");
        assert_eq!(cache.held(&past_any_clock.path), None, "a last write no clock reaches");
    }

    #[test]
    fn a_missing_cache_loads_empty_and_a_corrupt_unreadable_or_oversized_one_fails() {
        let folder = scratch("corrupt");
        let path = folder.join("app-1a2b3c4d.outputs-hashes");
        assert_eq!(HashCache::load(&path), Some(HashCache::default()), "a missing cache");
        let bad_entry = r#"{"files":{"bin/a.dll":{"size":1,"modified":1,"sha256":"abc"}}}"#;
        for text in ["not json", "{\"files\":[]}", bad_entry, "\u{feff}"] {
            fs::write(&path, text).expect("the cache can be written");
            assert_eq!(HashCache::load(&path), None, "{text:?}");
        }
        let folder_at_the_name = folder.join("folder.outputs-hashes");
        fs::create_dir_all(&folder_at_the_name).expect("a folder stands at the cache's name");
        assert_eq!(HashCache::load(&folder_at_the_name), None, "a cache that cannot be read");

        let line = format!(
            "{{\"files\":{{\"bin/a.dll\":{{\"size\":3,\"modified\":10,\"sha256\":\"{}\"}}}}}}",
            sha('a')
        );
        let mut file = File::create(&path).expect("the cache can be made");
        file.write_all(line.as_bytes()).expect("the line can be written");
        file.write_all(&vec![b' '; MANIFEST_LIMIT - line.len()])
            .expect("the padding can be written");
        drop(file);
        let loaded = HashCache::load(&path).expect("a cache of the limit's length loads");
        assert_eq!(
            loaded.held("bin/a.dll").map(|held| (held.stamp, held.sha256.as_str())),
            Some((stamp(3, 10), sha('a').as_str())),
            "a cache of the limit's length, its padding blank"
        );
        let mut file = File::options().append(true).open(&path).expect("the cache can be opened");
        file.write_all(b" ").expect("one more byte can be written");
        drop(file);
        assert_eq!(HashCache::load(&path), None, "a cache one byte past the limit");
    }

    #[test]
    fn a_saved_cache_loads_back_equal_and_a_rename_leaves_no_temp() {
        let folder = scratch("save");
        let remote = folder.join("remote");
        let path = remote.join("app-1a2b3c4d.outputs-hashes");
        let first = cache_of(&[(&found("bin/a.dll", 3, 10), 'a'), (&found("bin/Ünïcødé ✓.dll", 0, 1), 'b')]);
        let second = cache_of(&[(&found("bin/b.dll", 4, 20), 'c')]);

        first.save(&path);
        assert_eq!(HashCache::load(&path).as_ref(), Some(&first), "the folder is made and the cache saved");
        second.save(&path);

        assert_eq!(HashCache::load(&path).as_ref(), Some(&second), "a save replaces the cache");
        assert_eq!(names_in(&remote), ["app-1a2b3c4d.outputs-hashes"]);
        let text = fs::read_to_string(&path).expect("the cache can be read");
        assert_eq!(text.matches('\n').count(), 1, "one line: {text}");
    }

    #[test]
    fn a_save_that_fails_leaves_no_temp_file() {
        let folder = scratch("save-fails");
        let path = folder.join("app-1a2b3c4d.outputs-hashes");
        fs::create_dir_all(&path).expect("a folder stands at the cache's name");

        cache_of(&[(&found("bin/a.dll", 3, 10), 'a')]).save(&path);

        assert_eq!(names_in(&folder), ["app-1a2b3c4d.outputs-hashes"], "the temp file is removed");
        assert!(path.is_dir(), "what stood at the name is left");
    }

    /// The clock reads in the unit and from the epoch of a file's last write: a file written before it was read
    /// is not after it, and is less than a day before it.
    #[test]
    fn the_clock_reads_as_a_file_time() {
        let folder = scratch("clock");
        fs::write(folder.join("a.dll"), "abc").expect("the file can be written");
        let files = walk(&folder, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");

        let now = file_time_now();

        let written = files[0].stamp.modified;
        assert!(written <= now, "written {written}, now {now}");
        assert!(now - written < 24 * 3600 * 10_000_000, "written {written}, now {now}");
    }
}
