//! The command's log: readied before the command runs, written while it runs through its [`crate::log_writer`],
//! completed after it with the gate's own lines a run held, and read back in pieces, never whole: the failure markers
//! line by line from the start, the tail from the end. A line ends at CR LF, LF or a lone CR, as .NET's `ReadLine`
//! reads it, and is read as UTF-8 lossily.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use gate_wire::stamp::{Tag, is_process_line, unstamped};
use regex::Regex;

use crate::console::{Console, Note};

/// The failure markers (kept, V2), as the PowerShell gate and design.md write them.
const MARKERS: &str = r"^Build FAILED\.|error CS\d+|: error |Test run summary: Failed!|No test projects were found|^\s*failed: [1-9]|gate: (TIMED OUT|STALLED|BACKSTOP|TRANSFER TIMED OUT)";

/// [`MARKERS`] matched without regard to case, as the PowerShell gate's `Select-String` matches them.
#[expect(clippy::expect_used, reason = "the pattern is a constant, compiled by every unit test that scans a line")]
static MARKER_SCAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("(?i){MARKERS}")).expect("the marker pattern compiles"));

/// The most marker lines a scan keeps, and a failed run prints.
const MARKER_LINES: usize = 40;
/// The bytes of one line the marker scan reads; the rest of a longer line is passed over.
const SCAN_LINE_BYTES: usize = 64 * 1024;
/// The most bytes read from the end of the log for its tail.
const TAIL_BYTES: u64 = 1024 * 1024;
/// The size of each piece the tail is read in, from the end back.
const TAIL_CHUNK: u64 = 64 * 1024;
/// The most characters a printed line shows.
const SHOWN_CHARACTERS: usize = 4096;

/// The log at an absolute path.
pub(crate) struct Log {
    path: PathBuf,
}

impl Log {
    /// The log at `path`, which is absolute.
    pub(crate) fn new(path: &Path) -> Log {
        Log { path: path.to_owned() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the log's folder and removes an earlier log, and the `.err` file beside it the PowerShell gate writes,
    /// so a run that never starts its command leaves only its own lines. A log a running command still writes cannot
    /// be removed.
    ///
    /// # Errors
    ///
    /// What could not be done, and why.
    pub(crate) fn prepare(&self) -> Result<(), String> {
        if let Some(folder) = self.path.parent() {
            fs::create_dir_all(folder).map_err(|error| format!("cannot create the log's folder {}: {error}", folder.display()))?;
        }
        let mut err_path = OsString::from(self.path.as_os_str());
        err_path.push(".err");
        for path in [&self.path, &PathBuf::from(err_path)] {
            match fs::remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(format!("cannot remove the earlier {}: {error}", path.display()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Appends the notes `console` holds, each stamped when it was said ([`Note::line`]), in the order they were said
    /// ([`Log::append`]); kept in `console` for a later `finish` when they cannot be. A console attached to the log
    /// holds none, having written each as it was said.
    ///
    /// # Errors
    ///
    /// The system's error when the notes cannot be appended.
    pub(crate) fn finish(&self, console: &mut Console) -> io::Result<()> {
        let notes = console.take_held();
        let appended = self.append(&notes.iter().map(Note::line).collect::<Vec<_>>());
        if appended.is_err() {
            console.put_back_notes(notes);
        }
        appended
    }

    /// Removes the log, so a run that goes on from here starts from a log holding nothing: the bytes a remote client
    /// wrote to it do not stand twice beside the notes that say them again. A log already gone is no failure.
    ///
    /// # Errors
    ///
    /// The system's error when the log cannot be removed.
    pub(crate) fn discard(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            removed => removed,
        }
    }

    /// Appends `notes`, each as a line, starting on a line of its own when the log does not end in a line feed.
    ///
    /// # Errors
    ///
    /// The system's error when the log cannot be opened, read or written.
    pub(crate) fn append(&self, notes: &[String]) -> io::Result<()> {
        if notes.is_empty() {
            return Ok(());
        }
        let mut log = OpenOptions::new().read(true).append(true).create(true).open(&self.path)?;
        let mut text = Vec::new();
        if !ends_a_line(&mut log)? {
            text.push(b'\n');
        }
        for note in notes {
            text.extend_from_slice(note.as_bytes());
            text.push(b'\n');
        }
        log.write_all(&text)?;
        log.flush()
    }

    /// The first lines holding a failure marker, at most 40, each with its line number counted from 1 and cut for
    /// printing.
    ///
    /// # Errors
    ///
    /// The system's error when the log cannot be read.
    pub(crate) fn markers(&self) -> io::Result<Vec<(usize, String)>> {
        markers_in(BufReader::new(File::open(&self.path)?), MARKER_LINES)
    }

    /// The last `count` lines, read back from the end of the log, at most 1 MiB of it, each shown as its unstamped
    /// text and cut for printing; the gate's process lines are passed over.
    ///
    /// # Errors
    ///
    /// The system's error when the log cannot be read.
    pub(crate) fn tail(&self, count: u64) -> io::Result<Vec<String>> {
        tail_in(&mut File::open(&self.path)?, count, TAIL_CHUNK, TAIL_BYTES)
    }

    /// The log's last line for which `wanted` holds, read line by line from the start and never whole, only the last
    /// such line held; `None` when none holds. `wanted` is given, and the line is returned as, its unstamped text
    /// ([`unstamped`]): a plain line whole. A line ends at CR LF, LF or a lone CR, so a line written after a progress
    /// line's lone CR is a line of its own.
    ///
    /// # Errors
    ///
    /// The system's error when the log cannot be read.
    pub(crate) fn last_line(&self, wanted: impl Fn(&str) -> bool) -> io::Result<Option<String>> {
        let mut lines = LineReader::new(BufReader::new(File::open(&self.path)?), SCAN_LINE_BYTES);
        let mut last = None;
        while let Some(line) = lines.next_line()? {
            let line = String::from_utf8_lossy(&line);
            let text = unstamped(&line).text;
            if wanted(text) {
                last = Some(text.to_owned());
            }
        }
        Ok(last)
    }
}

/// Whether `log` is empty or its last byte is a line feed.
fn ends_a_line(log: &mut File) -> io::Result<bool> {
    if log.metadata()?.len() == 0 {
        return Ok(true);
    }
    let mut last = [0_u8];
    log.seek(SeekFrom::End(-1))?;
    log.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// Reads lines as .NET's `ReadLine` splits them, keeping at most `cap` bytes of each.
struct LineReader<R> {
    reader: R,
    state: LineState,
}

/// Where the reader stands between bytes: the bytes a line keeps, and whether the last byte ended a line with a CR.
struct LineState {
    cap: usize,
    after_cr: bool,
}

impl<R: BufRead> LineReader<R> {
    fn new(reader: R, cap: usize) -> LineReader<R> {
        LineReader {
            reader,
            state: LineState { cap, after_cr: false },
        }
    }

    /// The next line's bytes, without its line end; `None` at the end, where no empty line follows a final line end.
    fn next_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        let mut started = false;
        loop {
            let buffer = self.reader.fill_buf()?;
            if buffer.is_empty() {
                return Ok(started.then_some(line));
            }
            let (used, ended) = self.state.take(buffer, &mut line, &mut started);
            self.reader.consume(used);
            if ended {
                return Ok(Some(line));
            }
        }
    }
}

impl LineState {
    /// Takes `buffer`'s bytes into `line` until a line ends; how many bytes were used, and whether the line ended.
    fn take(&mut self, buffer: &[u8], line: &mut Vec<u8>, started: &mut bool) -> (usize, bool) {
        for (index, &byte) in buffer.iter().enumerate() {
            let after_cr = std::mem::replace(&mut self.after_cr, false);
            match byte {
                b'\n' if after_cr => {}
                b'\n' => return (index + 1, true),
                b'\r' => {
                    self.after_cr = true;
                    return (index + 1, true);
                }
                _ => {
                    *started = true;
                    if line.len() < self.cap {
                        line.push(byte);
                    }
                }
            }
        }
        (buffer.len(), false)
    }
}

/// The first `limit` lines of `reader` holding a failure marker in their unstamped text ([`unstamped`]; a plain line
/// whole), numbered from 1, each kept as that text cut for printing; the scan stops once it has them.
fn markers_in(reader: impl BufRead, limit: usize) -> io::Result<Vec<(usize, String)>> {
    let mut lines = LineReader::new(reader, SCAN_LINE_BYTES);
    let mut found = Vec::new();
    let mut number = 0;
    while found.len() < limit
        && let Some(line) = lines.next_line()?
    {
        number += 1;
        let line = String::from_utf8_lossy(&line);
        let text = unstamped(&line).text;
        if MARKER_SCAN.is_match(text) {
            found.push((number, shown(text)));
        }
    }
    Ok(found)
}

/// Whether `line` is one of the gate's process lines ([`is_process_line`] under the `gat` tag), which a tail passes
/// over.
fn is_process_entry(line: &[u8]) -> bool {
    let line = String::from_utf8_lossy(line);
    let parts = unstamped(&line);
    parts.tag == Some(Tag::Gat) && is_process_line(parts.text)
}

/// The last `count` lines of `file` that are not the gate's process lines ([`is_process_entry`]), read back from its
/// end in pieces of `chunk` bytes until they hold that many or `most` bytes were read, each shown as its unstamped text
/// and cut for printing. A tail that needs more than `most` bytes shows the whole lines that fit, or, for a last line
/// longer than that, its end ([`line_end`]).
fn tail_in(file: &mut (impl Read + Seek), count: u64, chunk: u64, most: u64) -> io::Result<Vec<String>> {
    let wanted = usize::try_from(count).unwrap_or(usize::MAX);
    if wanted == 0 {
        return Ok(Vec::new());
    }
    let end = file.seek(SeekFrom::End(0))?;
    let mut start = end;
    let mut buffer = Vec::new();
    loop {
        let step = chunk.min(start).min(most - (end - start));
        start -= step;
        let mut piece = vec![0; usize::try_from(step).map_err(io::Error::other)?];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut piece)?;
        piece.extend_from_slice(&buffer);
        buffer = piece;
        let lines = split(&buffer)?;
        // The first line may have begun before the bytes read, so only the lines after it are surely whole.
        let whole = lines.iter().skip(1).filter(|line| !is_process_entry(line)).count();
        if start == 0 || end - start >= most || whole >= wanted {
            return Ok(last_lines(lines, wanted, start == 0));
        }
    }
}

/// The last `wanted` of `lines` that are not the gate's process lines, each shown as its unstamped text and cut for
/// printing. Unless they start at the file's start, the first may have begun before the bytes read: it is dropped when
/// another follows, and shown by its end ([`line_end`]) when it is the only one.
fn last_lines(mut lines: Vec<Vec<u8>>, wanted: usize, at_start: bool) -> Vec<String> {
    if !at_start {
        match lines.as_slice() {
            [only] => return vec![line_end(only)],
            [_, _, ..] => {
                lines.remove(0);
            }
            [] => {}
        }
    }
    let kept: Vec<&Vec<u8>> = lines.iter().filter(|line| !is_process_entry(line)).collect();
    let skipped = kept.len().saturating_sub(wanted);
    kept[skipped..]
        .iter()
        .map(|line| shown(unstamped(&String::from_utf8_lossy(line)).text))
        .collect()
}

/// The end of a line whose start was not read: `… ` and its last 4096 characters, from its first whole character (the
/// bytes of a character begun before the line's read bytes are dropped).
fn line_end(bytes: &[u8]) -> String {
    let partial = bytes.iter().take(3).take_while(|&&byte| byte & 0xc0 == 0x80).count();
    let text = String::from_utf8_lossy(&bytes[partial..]);
    let characters = text.chars().count();
    let from = text
        .char_indices()
        .nth(characters.saturating_sub(SHOWN_CHARACTERS))
        .map_or(text.len(), |(at, _)| at);
    format!("\u{2026} {}", &text[from..])
}

/// The lines of `bytes`.
fn split(bytes: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let mut reader = LineReader::new(bytes, usize::MAX);
    let mut lines = Vec::new();
    while let Some(line) = reader.next_line()? {
        lines.push(line);
    }
    Ok(lines)
}

/// `text` as a printed line shows it: cut after 4096 characters, ending ` …` when cut.
fn shown(text: &str) -> String {
    match text.char_indices().nth(SHOWN_CHARACTERS) {
        Some((cut, _)) => format!("{} \u{2026}", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot write or read its files must fail loudly")]
mod tests {
    use std::fs;
    use std::io::{BufReader, Cursor};
    use std::path::PathBuf;

    use gate_wire::stamp::{Tag, unstamped};

    use super::{Log, MARKER_LINES, markers_in, tail_in};
    use crate::console::Console;

    /// A fresh folder for one test, under the system's temporary folder.
    fn folder(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-log-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        fs::create_dir_all(&folder).expect("the test folder can be made");
        folder
    }

    fn markers(text: &str) -> Vec<(usize, String)> {
        markers_in(text.as_bytes(), MARKER_LINES).expect("a string reads")
    }

    fn matched(line: &str) -> bool {
        !markers(line).is_empty()
    }

    fn tail(text: &str, count: u64, chunk: u64, most: u64) -> Vec<String> {
        tail_in(&mut Cursor::new(text.as_bytes()), count, chunk, most).expect("a string reads")
    }

    fn numbered(lines: &[(usize, &str)]) -> Vec<(usize, String)> {
        lines.iter().map(|(number, line)| (*number, (*line).to_owned())).collect()
    }

    #[test]
    fn each_marker_alternative_matches_a_line() {
        let lines = [
            "Build FAILED.",
            "Program.cs(3,1): error CS1002: ; expected",
            "src/main.rs: error : something",
            "Test run summary: Failed!",
            "No test projects were found.",
            "failed: 1",
            "gate: TIMED OUT after 900 s",
            "gate: STALLED: no output",
            "inner gate: BACKSTOP: 5 times the ceiling",
            "gate: TRANSFER TIMED OUT after 1800 s fetching the outputs",
        ];
        for line in lines {
            assert!(matched(line), "{line}");
        }
    }

    #[test]
    fn the_no_test_projects_line_is_a_marker_anywhere_in_its_line() {
        assert!(matched("  No test projects were found."), "leading text");
        assert!(matched("NO TEST PROJECTS WERE FOUND."), "case is ignored");
        assert!(!matched("Found 3 test projects"), "a run that found test projects is not a failure");
    }

    #[test]
    fn markers_anchor_per_line_ignore_case_and_need_a_failed_count_above_0() {
        assert!(!matched("the Build FAILED. line"), "^ anchors at the line start");
        assert_eq!(
            markers("first line\nBuild FAILED.\n"),
            numbered(&[(2, "Build FAILED.")]),
            "^ anchors at each line"
        );
        assert!(matched("build failed."), "case is ignored");
        assert!(matched("ERROR cs0103"), "case is ignored");
        assert!(matched("GATE: stalled: x"), "case is ignored");
        assert!(!matched("    failed: 0"));
        assert!(matched("    failed: 3"), "leading spaces before failed");
        assert!(!matched("Build FAILED"), "the period is part of the marker");
    }

    #[test]
    fn lines_end_at_crlf_lf_or_a_lone_cr() {
        assert_eq!(
            markers("ok\r\nBuild FAILED.\r\nTest run summary: Failed!\r\n"),
            numbered(&[(2, "Build FAILED."), (3, "Test run summary: Failed!")])
        );
        assert_eq!(
            markers("50%\rBuild FAILED.\n"),
            numbered(&[(2, "Build FAILED.")]),
            "a progress line ends at CR"
        );
        assert_eq!(tail("a\r\nb\rc\n\nd", 4, 64, 1024), ["b", "c", "", "d"]);
    }

    #[test]
    fn the_marker_scan_keeps_the_first_40() {
        let text = (1..=45).map(|number| format!("error CS{number}")).collect::<Vec<_>>().join("\n");

        let found = markers(&text);

        assert_eq!(found.len(), 40);
        assert_eq!(found[0], (1, "error CS1".to_owned()));
        assert_eq!(found[39], (40, "error CS40".to_owned()));
    }

    #[test]
    fn the_tail_is_read_back_across_pieces() {
        let text = "one\r\ntwo\nthree\rfour\r\nfive\n";

        for chunk in 1..=8 {
            assert_eq!(tail(text, 3, chunk, 1024), ["three", "four", "five"], "pieces of {chunk}");
            assert_eq!(tail(text, 9, chunk, 1024), ["one", "two", "three", "four", "five"], "pieces of {chunk}");
        }
        assert_eq!(tail(text, 0, 4, 1024), Vec::<String>::new());
    }

    #[test]
    fn a_tail_past_its_byte_cap_shows_what_fits() {
        assert_eq!(tail("a\nbb\nccc\n", 3, 2, 6), ["ccc"], "bb\\n is cut short, so only ccc is whole");
        assert_eq!(tail("abcdefgh\n", 1, 2, 4), ["\u{2026} fgh"], "the end of a line longer than the cap");
    }

    #[test]
    fn a_last_line_longer_than_the_cap_shows_its_last_4096_characters() {
        let long = format!("{}END\n", "a".repeat(9000));
        let split = format!("{}x\n", "\u{e9}".repeat(5000));

        assert_eq!(tail(&long, 1, 1024, 8000), [format!("\u{2026} {}END", "a".repeat(4093))]);
        assert_eq!(
            tail(&split, 1, 1024, 4097),
            [format!("\u{2026} {}x", "\u{e9}".repeat(2047))],
            "the half of a character the window starts in is dropped"
        );
    }

    #[test]
    fn a_crlf_split_across_reads_ends_one_line() {
        let text = "ok\r\nBuild FAILED.\r\nerror CS1\r\n";

        let found = markers_in(BufReader::with_capacity(1, text.as_bytes()), MARKER_LINES).expect("a string reads");

        assert_eq!(found, numbered(&[(2, "Build FAILED."), (3, "error CS1")]));
    }

    #[test]
    fn a_printed_line_is_cut_after_4096_characters() {
        let long = format!("{}\n", "\u{e9}".repeat(5000));

        let lines = tail(&long, 1, 1024, 1 << 20);
        let found = markers(&format!("error CS1 {long}"));

        assert_eq!(lines, [format!("{} \u{2026}", "\u{e9}".repeat(4096))]);
        assert!(found[0].1.ends_with(" \u{2026}"), "a marker line is cut too");
        assert_eq!(found[0].1.chars().count(), 4096 + 2);
    }

    #[test]
    fn the_tail_of_invalid_utf8_reads_as_replacement_characters() {
        let folder = folder("tail");
        let log = Log::new(&folder.join("out.log"));
        fs::write(log.path(), b"first\nok \xff\xfe bad\nlast").expect("the log can be written");

        assert_eq!(log.tail(2).expect("the log reads"), ["ok \u{fffd}\u{fffd} bad", "last"]);
        assert_eq!(log.tail(20).expect("the log reads"), ["first", "ok \u{fffd}\u{fffd} bad", "last"]);
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }

    /// A stamp as a run's log writes it.
    const STAMP: &str = "2026-10-22T09:31:38.120001-07:00";

    #[test]
    fn a_stamped_marker_line_is_caught_and_kept_as_its_text() {
        let text = format!("{STAMP} out ok\n{STAMP} out Build FAILED.\n{STAMP} err    failed: 3\n{STAMP} out failed: 0\n");

        assert_eq!(
            markers(&text),
            numbered(&[(2, "Build FAILED."), (3, "   failed: 3")]),
            "each anchored marker matches the text after the tag"
        );
    }

    #[test]
    fn the_tail_passes_over_a_burst_of_process_lines_at_the_end() {
        let outs = ["one", "two", "three"].iter().map(|line| format!("{STAMP} out {line}"));
        let exits = (1..=50).map(|pid| format!("{STAMP} gat exit pid={pid} code=0 ran=0.010000s"));
        let text = outs.chain(exits).collect::<Vec<_>>().join("\n") + "\n";

        for chunk in [64, 1024, 64 * 1024] {
            assert_eq!(tail(&text, 3, chunk, 1024 * 1024), ["one", "two", "three"], "pieces of {chunk}");
        }
        assert_eq!(
            tail(&text, 20, 64, 1024 * 1024),
            ["one", "two", "three"],
            "no process line fills a longer tail"
        );
    }

    #[test]
    fn the_last_line_of_a_stamped_log_is_its_text() {
        let folder = folder("stamped-last");
        let log = Log::new(&folder.join("out.log"));
        let text = format!("{STAMP} out working\n{STAMP} gat gate: STALLED: no output for 600 s\n{STAMP} out after\n");
        fs::write(log.path(), text).expect("the log can be written");

        let last = log.last_line(|text| text.starts_with("gate: STALLED")).expect("the log reads");

        assert_eq!(last.as_deref(), Some("gate: STALLED: no output for 600 s"));
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }

    #[test]
    fn finish_appends_the_held_notes_stamped_in_order_on_a_line_of_their_own() {
        let folder = folder("finish");
        let log = Log::new(&folder.join("logs").join("out.log"));
        log.prepare().expect("the log's folder can be made");
        fs::write(log.path(), b"no line feed").expect("the log can be written");
        let mut console = Console::new();
        console.err("gate: first");
        console.err("gate: second");

        log.finish(&mut console).expect("the log completes");
        log.finish(&mut console).expect("a second finish has nothing to append");

        let text = String::from_utf8(fs::read(log.path()).expect("the log reads")).expect("the log is UTF-8");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "no line feed");
        let notes: Vec<_> = lines[1..].iter().map(|line| unstamped(line)).collect();
        assert_eq!(
            notes.iter().map(|note| note.text).collect::<Vec<_>>(),
            ["gate: first", "gate: second"],
            "{text}"
        );
        assert!(notes.iter().all(|note| note.tag == Some(Tag::Gat)), "{text}");
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }

    #[test]
    fn prepare_removes_an_earlier_log_and_the_err_file_the_powershell_gate_wrote() {
        let folder = folder("prepare");
        let log = Log::new(&folder.join("out.log"));
        let err = folder.join("out.log.err");
        fs::write(log.path(), b"earlier").expect("the log can be written");
        fs::write(&err, b"err").expect("the err file can be written");

        log.prepare().expect("the log is readied");

        assert!(!log.path().exists() && !err.exists());
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }
}
