//! `gate remote add|remove`: edits one repo's pattern list in one section of `remote.json` through
//! [`gate_router::edit_remote_json`], which refuses a result the gate would not read or that changes anything else. Before
//! the write the file is read again and a change since the first read refuses the edit; then the original bytes are
//! copied to a backup made new beside it, and the new text replaces the file in one move, so a reader sees the old
//! file or the new one. No line names a `host` the file holds.
//!
//! `gate remote match` reads the file as routing does and prints the pattern a command would match
//! ([`gate_router::pattern_for`]), so nothing outside the gate needs a matcher of its own.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use gate_args::{RemoteMatchRequest, RemoteRequest, RemoteVerb};
use gate_router::{Edit, EditRefusal, EditVerb, Edited, edit_remote_json, pattern_for};
use jiff::Timestamp;

use crate::answer::FAILED_STATUS;
use crate::console::Console;
use crate::resolve::Environment;
use crate::route;

/// The last number a backup's name takes when the names before it are taken.
const LAST_BACKUP_NUMBER: u32 = 99;

/// Runs `gate remote` and returns its exit status: 0 changed or nothing to do, 1 refused or failed, the file untouched.
pub(crate) fn remote(request: &RemoteRequest, environment: &Environment, console: &mut Console) -> u32 {
    let Some(path) = route::remote_json_path(environment.local_app_data.as_deref()) else {
        console.err("remote: no remote.json: LOCALAPPDATA is not an absolute folder");
        return u32::from(FAILED_STATUS);
    };
    match edit_file(request, &path, |path| fs::read(path), Timestamp::now()) {
        Ok(lines) => {
            for line in lines {
                console.out(&line);
            }
            0
        }
        Err(line) => {
            console.err(&line);
            u32::from(FAILED_STATUS)
        }
    }
}

/// Runs `gate remote match` and returns its exit status: 0 with the first pattern of the repo that matches the text on
/// stdout, 1 with nothing on it when none matches or, saying why on stderr, when `remote.json` cannot be read.
pub(crate) fn remote_match(request: &RemoteMatchRequest, environment: &Environment, console: &mut Console) -> u32 {
    let Some(path) = route::remote_json_path(environment.local_app_data.as_deref()) else {
        console.err("remote: no remote.json: LOCALAPPDATA is not an absolute folder");
        return u32::from(FAILED_STATUS);
    };
    let config = match route::read_remote_json(&path) {
        Ok(Some(config)) => config,
        Ok(None) => {
            console.err(&format!("remote: no remote.json at {}", path.display()));
            return u32::from(FAILED_STATUS);
        }
        Err(problem) => {
            console.err(&format!("remote: ignored {}: {}", problem.path.display(), problem.reason));
            return u32::from(FAILED_STATUS);
        }
    };
    match pattern_for(&config, &request.repo, &request.text) {
        Some(pattern) => {
            console.out(&pattern);
            0
        }
        None => u32::from(FAILED_STATUS),
    }
}

/// Makes `request`'s edit on the file at `path`, read a second time through `reread` just before the write, a backup
/// named for `now`: the stdout lines; else the one refusal line, nothing written.
fn edit_file(request: &RemoteRequest, path: &Path, reread: impl FnOnce(&Path) -> io::Result<Vec<u8>>, now: Timestamp) -> Result<Vec<String>, String> {
    let shown = path.display();
    let original = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(format!("remote: no remote.json at {shown}")),
        Err(error) => return Err(format!("remote: cannot read {shown}: {error}")),
    };
    let text = std::str::from_utf8(&original).map_err(|_| format!("remote: refused: {shown} is not UTF-8 text; nothing written"))?;
    let edited = edit_remote_json(text, &edit_of(request)).map_err(|refusal| refusal_line(&refusal, path))?;
    let Edited {
        text: new_text,
        mut lines,
        changed,
    } = edited;
    if !changed {
        return Ok(lines);
    }
    if request.dry_run {
        lines.push("remote: dry run; nothing written".to_owned());
        return Ok(lines);
    }
    match reread(path) {
        Ok(again) if again == original => {}
        Ok(_) => return Err("remote: changed while editing; nothing written".to_owned()),
        Err(error) => return Err(format!("remote: cannot read {shown} again: {error}; nothing written")),
    }
    let first = backup_path(path, request.tag.as_deref(), now);
    let backup = back_up(&first, &original, write_backup)?;
    gate_state::write_text_replacing(path, &new_text).map_err(|error| {
        format!(
            "remote: cannot write {shown}: {error}; it is left as it was, and {} holds it",
            backup.display()
        )
    })?;
    lines.push(format!("remote: backup {}", backup.display()));
    lines.push(format!("remote: wrote {shown}"));
    Ok(lines)
}

/// The router's edit of `request`.
fn edit_of(request: &RemoteRequest) -> Edit<'_> {
    Edit {
        verb: match request.verb {
            RemoteVerb::Add => EditVerb::Add,
            RemoteVerb::Remove => EditVerb::Remove,
        },
        section: &request.section,
        builder: request.builder.as_deref(),
        repo: &request.repo,
        patterns: &request.patterns,
    }
}

/// The line of an edit of the file at `path` that `refusal` refused.
fn refusal_line(refusal: &EditRefusal, path: &Path) -> String {
    match refusal {
        EditRefusal::Unusable(reason) => format!("remote: ignored {}: {reason}", path.display()),
        EditRefusal::UnknownBuilder(_) => format!("remote: {refusal}"),
        _ => format!("remote: refused: {refusal}; nothing written"),
    }
}

/// The backup of the file at `path` made at `now`: `remote.json.bak-[<tag>-]<yyyyMMddTHHmmssZ>` beside it.
fn backup_path(path: &Path, tag: Option<&str>, now: Timestamp) -> PathBuf {
    let name = path.file_name().map_or_else(|| "remote.json".into(), |name| name.to_string_lossy());
    let tag = tag.map_or_else(String::new, |tag| format!("{tag}-"));
    path.with_file_name(format!("{name}.bak-{tag}{}", now.strftime("%Y%m%dT%H%M%SZ")))
}

/// Writes `bytes` to a file made new at `first`, else at `<first>-2`, `-3` and so on up to `-<LAST_BACKUP_NUMBER>` when
/// the names before are taken, as two edits in one second find them, through `stage`, which is handed the file made
/// open: the path made. A file already at a name is left. Else the refusal line, naming the file made when `stage`
/// failed on it.
fn back_up(first: &Path, bytes: &[u8], stage: impl FnOnce(&mut fs::File, &[u8]) -> io::Result<()>) -> Result<PathBuf, String> {
    let refused = |path: &Path, why: &str| format!("remote: cannot make the backup {}: {why}; nothing written", path.display());
    let name = first.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    for number in 1..=LAST_BACKUP_NUMBER {
        let path = if number == 1 {
            first.to_path_buf()
        } else {
            first.with_file_name(format!("{name}-{number}"))
        };
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let staged = stage(&mut file, bytes);
                drop(file);
                return match staged {
                    Ok(()) => Ok(path),
                    Err(error) => Err(refused(&path, &removed_after(&path, &error))),
                };
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(refused(&path, &error.to_string())),
        }
    }
    Err(refused(
        first,
        &format!("it and every numbered name up to -{LAST_BACKUP_NUMBER} are taken"),
    ))
}

/// Why the backup made at `path` could not be written, `error`, once the file is removed; with `; <path> may be left
/// partly written` when the removal fails too.
fn removed_after(path: &Path, error: &io::Error) -> String {
    match fs::remove_file(path) {
        Ok(()) => error.to_string(),
        Err(_) => format!("{error}; {} may be left partly written", path.display()),
    }
}

/// Writes `bytes` to the backup `file` and flushes it to the disk.
fn write_backup(file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot write or read its files must fail loudly")]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    use gate_args::{RemoteRequest, RemoteVerb};
    use jiff::Timestamp;

    use super::{back_up, backup_path, edit_file};
    use crate::test_support::temp_dir;

    const FILE: &str =
        "{\n    \"host\": \"builder@example\",\n    \"repos\": {\n        \"gate\": [\n            \"cargo test*\"\n        ]\n    }\n}\n";

    fn add(pattern: &str) -> RemoteRequest {
        RemoteRequest {
            verb: RemoteVerb::Add,
            section: "repos".to_owned(),
            repo: "gate".to_owned(),
            patterns: vec![pattern.to_owned()],
            builder: None,
            tag: None,
            dry_run: false,
        }
    }

    /// A folder of the test's own holding `remote.json` with [`FILE`]: the file's path.
    fn routing_file(name: &str) -> PathBuf {
        let folder = temp_dir().join(format!("remote-cmd-{name}"));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("an earlier run's folder can be removed");
        }
        fs::create_dir_all(&folder).expect("the folder can be made");
        let path = folder.join("remote.json");
        fs::write(&path, FILE).expect("the file can be written");
        path
    }

    fn entries(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .expect("the folder can be listed")
            .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn now() -> Timestamp {
        "2026-11-02T03:04:05Z".parse().expect("a timestamp")
    }

    #[test]
    fn a_file_changed_between_the_reads_is_left_and_no_backup_made() {
        let path = routing_file("changed");
        let changed = |_: &Path| -> io::Result<Vec<u8>> { Ok(FILE.replace("cargo test*", "cargo nextest*").into_bytes()) };
        assert_eq!(
            edit_file(&add("cargo build*"), &path, changed, now()),
            Err("remote: changed while editing; nothing written".to_owned())
        );
        assert_eq!(fs::read_to_string(&path).expect("the file reads"), FILE);
        assert_eq!(entries(path.parent().expect("a folder")), ["remote.json"]);
    }

    #[test]
    fn an_unchanged_second_read_lets_the_edit_write_after_its_backup() {
        let path = routing_file("unchanged");
        let lines = edit_file(&add("cargo build*"), &path, |path| fs::read(path), now()).expect("the edit is made");
        let backup = path.with_file_name("remote.json.bak-20261102T030405Z");
        assert_eq!(
            lines,
            [
                "remote: repos.gate: + cargo build*".to_owned(),
                format!("remote: backup {}", backup.display()),
                format!("remote: wrote {}", path.display()),
            ]
        );
        assert_eq!(fs::read_to_string(&backup).expect("the backup reads"), FILE);
    }

    /// `name` with `-<n>` after it, beside `path`.
    fn numbered(path: &Path, n: u32) -> PathBuf {
        let name = path.file_name().expect("a name").to_string_lossy().into_owned();
        path.with_file_name(format!("{name}-{n}"))
    }

    #[test]
    fn a_backup_name_already_taken_moves_the_backup_to_the_next_number() {
        let path = routing_file("taken-once");
        let first = backup_path(&path, Some("t"), now());
        fs::write(&first, "earlier").expect("the backup's name can be taken");
        let request = RemoteRequest {
            tag: Some("t".to_owned()),
            ..add("cargo build*")
        };
        let lines = edit_file(&request, &path, |path| fs::read(path), now()).expect("the edit is made");
        let second = numbered(&first, 2);
        assert_eq!(lines[1], format!("remote: backup {}", second.display()));
        assert_eq!(fs::read_to_string(&second).expect("the backup reads"), FILE);
        assert_eq!(fs::read_to_string(&first).expect("the earlier backup reads"), "earlier");
    }

    #[test]
    fn every_backup_name_up_to_99_taken_refuses_the_write() {
        let path = routing_file("taken-all");
        let first = backup_path(&path, None, now());
        fs::write(&first, "earlier").expect("the backup's name can be taken");
        for n in 2..=99 {
            fs::write(numbered(&first, n), "earlier").expect("the backup's name can be taken");
        }
        let refusal = edit_file(&add("cargo build*"), &path, |path| fs::read(path), now()).expect_err("refused");
        assert!(refusal.starts_with("remote: cannot make the backup "), "{refusal}");
        assert_eq!(fs::read_to_string(&path).expect("the file reads"), FILE);
        assert!(!numbered(&first, 100).exists(), "no name past -99 is tried");
    }

    #[test]
    fn a_backup_whose_write_fails_is_removed_and_named_in_the_refusal() {
        let path = routing_file("write-fails");
        let first = backup_path(&path, None, now());
        fs::write(&first, "earlier").expect("the backup's name can be taken");
        let failing = |_: &mut fs::File, _: &[u8]| -> io::Result<()> { Err(io::Error::other("the disk is full")) };
        let refusal = back_up(&first, FILE.as_bytes(), failing).expect_err("refused");
        let made = numbered(&first, 2);
        assert_eq!(
            refusal,
            format!("remote: cannot make the backup {}: the disk is full; nothing written", made.display())
        );
        assert!(!made.exists(), "the backup made is removed");
        assert_eq!(fs::read_to_string(&first).expect("the earlier backup reads"), "earlier");
    }

    #[test]
    fn the_parser_and_the_router_name_the_same_sections() {
        assert_eq!(gate_args::REMOTE_SECTIONS, gate_router::EDIT_SECTIONS);
        assert_eq!(gate_args::REMOTE_BUILDER_SECTIONS, gate_router::BUILDER_SECTIONS);
    }
}
