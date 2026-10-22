//! The diagnostics block a kill writes before it terminates the job: the facts gathered about the job's processes, the
//! processes outside it and the last samples, turned into the PowerShell gate's lines for them, in its order and
//! wording. The facts are given; nothing here reads a process.

use std::collections::VecDeque;

use crate::share::Ticks;

/// The heading of the block, its first line.
pub const HEADING: &str = "gate: diagnostics";

/// The job processes the block gives a line each; the rest are counted on one line. A gatherer reads at most this many
/// of them and counts the rest unread.
pub const JOB_LINES: usize = 40;

/// The processes outside the job the block gives a line each; the rest are counted on one line. A gatherer reads at
/// most this many of them and counts the rest unread.
pub const OUTSIDE_LINES: usize = 20;

/// The samples the block lists and a [`SampleHistory`] keeps, the last ones.
const SAMPLES: usize = 6;

/// The characters of a command line a process line keeps before it is cut with `...`.
const COMMAND_CHARS: usize = 200;

/// The bytes of a mebibyte, the unit a working set prints in.
const MEBIBYTE: u64 = 1_048_576;

/// The magnitude from which .NET's `Math.Round` to a number of decimals returns the value unchanged.
const ROUND_LIMIT: f64 = 1e16;

/// What was read of one process, each part `None` when it could not be read: a process that exited since the
/// snapshot, or another user's.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessFacts {
    /// The process's id.
    pub pid: u32,
    /// The id of the process that started it.
    pub parent: Option<u32>,
    /// The name of its image, such as `cargo.exe`.
    pub name: String,
    /// Its user and kernel CPU time.
    pub cpu: Option<Ticks>,
    /// Its working set, in bytes.
    pub working_set_bytes: Option<u64>,
    /// The command line it was started with.
    pub command_line: Option<String>,
}

/// What the WSL VM's CPU was at a sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WslVm {
    /// The command is not watched for the WSL VM.
    NotWatched,
    /// The command is watched for the WSL VM, but its CPU could not be read.
    Unavailable,
    /// The WSL VM's CPU in percent of one core (vmmemWSL's `% Processor Time`), so above 100 when it used more than
    /// one core.
    Percent(f64),
}

/// The figures one sample of the watch found.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleFacts {
    /// The wall seconds the command had run.
    pub wall_seconds: f64,
    /// The load-adjusted seconds counted so far.
    pub adjusted_seconds: f64,
    /// The share of the machine free over the sample's interval, as a whole percent.
    pub free_percent: u32,
    /// The bytes the command wrote to its stdout so far.
    pub out_bytes: u64,
    /// The bytes the command wrote to its stderr so far.
    pub err_bytes: u64,
    /// The job's total CPU time.
    pub job_cpu: Ticks,
    /// The WSL VM's CPU, or that it is not watched.
    pub wsl_vm: WslVm,
    /// The processes in the job.
    pub processes: u32,
    /// The CPU time of the processes outside the job under its members.
    pub escaped_cpu: Ticks,
}

/// Everything the gathering found, uncapped: [`lines`] sorts the job by pid and keeps to the block's limits itself.
///
/// A gathering that ran out of time says what it did not read: the processes it found but left unread are counted on
/// the line after the process lines. The gatherer reads the job's processes in pid order, so the ones it read are the
/// lowest pids.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The processes in the job that were read.
    pub job: Vec<ProcessFacts>,
    /// The processes in the job found but not read before the time ran out.
    pub job_unread: usize,
    /// The live descendants of the command and of the job's processes that are not in the job and were read, in the
    /// order found.
    pub outside: Vec<ProcessFacts>,
    /// The processes outside the job found but not read before the time ran out.
    pub outside_unread: usize,
    /// The samples, oldest first.
    pub samples: Vec<SampleFacts>,
    /// The milliseconds after which the gathering ran out of time, or `None` when it finished.
    pub cut_at_ms: Option<u64>,
}

/// The last samples of a watch, as many as the block lists, oldest first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SampleHistory {
    samples: VecDeque<SampleFacts>,
}

impl SampleHistory {
    /// Keeps `sample`, dropping the oldest once the history is full.
    pub fn push(&mut self, sample: SampleFacts) {
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// The samples kept, oldest first.
    #[must_use]
    pub fn to_vec(&self) -> Vec<SampleFacts> {
        self.samples.iter().cloned().collect()
    }
}

/// The diagnostics block for `report`: the heading, the job's processes by pid, the processes outside the job, the
/// last samples, and when the gathering was cut.
#[must_use]
pub fn lines(report: &Report) -> Vec<String> {
    let mut lines = vec![HEADING.to_owned()];
    let mut job: Vec<&ProcessFacts> = report.job.iter().collect();
    job.sort_by_key(|process| process.pid);
    push_processes(&mut lines, "job", job.into_iter(), report.job_unread, JOB_LINES);
    push_processes(&mut lines, "outside job", report.outside.iter(), report.outside_unread, OUTSIDE_LINES);
    push_samples(&mut lines, &report.samples);
    if let Some(ms) = report.cut_at_ms {
        lines.push(format!("gate:   ... diagnostics cut at {ms} ms"));
    }
    lines
}

/// The line that ends the block when gathering it failed, with `message` made one line: its trailing CR and LF are
/// dropped and every other run of them is one space.
#[must_use]
pub fn failed_line(message: &str) -> String {
    format!("gate: diagnostics failed: {}", one_line(message.trim_end_matches(['\r', '\n'])))
}

/// The whole block when the gathering failed: the heading, then [`failed_line`].
#[must_use]
pub fn failed_block(message: &str) -> Vec<String> {
    vec![HEADING.to_owned(), failed_line(message)]
}

/// Up to `limit` lines for the processes `read` under `label`, then a line counting the rest, the `unread` ones with
/// them.
fn push_processes<'a>(lines: &mut Vec<String>, label: &str, read: impl ExactSizeIterator<Item = &'a ProcessFacts>, unread: usize, limit: usize) {
    let found = read.len() + unread;
    let written = read.len().min(limit);
    lines.extend(read.take(limit).map(|process| process_line(label, process)));
    if found > written {
        lines.push(format!("gate:   ... and {} more {label} processes", found - written));
    }
}

/// One process line: its pid, parent, name, CPU time, working set and command line, each part that could not be
/// read given as `?`.
fn process_line(label: &str, process: &ProcessFacts) -> String {
    let parent = process.parent.map_or_else(|| "?".to_owned(), |parent| parent.to_string());
    let set = process
        .working_set_bytes
        .map_or_else(|| "?".to_owned(), |bytes| format!("{} MB", whole_mebibytes(bytes)));
    let command = process.command_line.as_deref().map_or_else(|| "?".to_owned(), command_text);
    format!(
        "gate:   {label} {} parent {parent} {}: cpu {}, working set {set}: {command}",
        process.pid,
        process.name,
        cpu_text(process.cpu)
    )
}

/// The last samples, oldest first, or a line saying there are none.
fn push_samples(lines: &mut Vec<String>, samples: &[SampleFacts]) {
    if samples.is_empty() {
        lines.push("gate:   samples: none".to_owned());
        return;
    }
    let first = samples.len().saturating_sub(SAMPLES);
    lines.extend(samples[first..].iter().map(sample_line));
}

/// One sample's line.
fn sample_line(sample: &SampleFacts) -> String {
    let vm = match sample.wsl_vm {
        WslVm::NotWatched => String::new(),
        WslVm::Unavailable => "wsl vm cpu unavailable, ".to_owned(),
        WslVm::Percent(percent) => format!("wsl vm cpu {}%, ", round_half_even(percent, 1)),
    };
    format!(
        "gate:   sample at {} s (load-adjusted {} s, free {}%): out {} B, err {} B, job cpu {} s, {vm}{}, \
         escaped cpu {} s",
        round_half_even(sample.wall_seconds, 1),
        round_half_even(sample.adjusted_seconds, 1),
        sample.free_percent,
        sample.out_bytes,
        sample.err_bytes,
        seconds(sample.job_cpu),
        process_count(sample.processes),
        seconds(sample.escaped_cpu),
    )
}

/// `1 process` or `<n> processes`.
fn process_count(count: u32) -> String {
    if count == 1 {
        return "1 process".to_owned();
    }
    format!("{count} processes")
}

/// CPU time in seconds with ` s`, or `?` when it could not be read.
fn cpu_text(cpu: Option<Ticks>) -> String {
    cpu.map_or_else(|| "?".to_owned(), |cpu| format!("{} s", seconds(cpu)))
}

/// CPU time in seconds to two decimals, as PowerShell's `ConvertTo-Second` gives it.
#[expect(
    clippy::cast_precision_loss,
    reason = "the PowerShell gate divides the ticks as a double, so the same loss prints the same seconds"
)]
fn seconds(ticks: Ticks) -> f64 {
    round_half_even(ticks.0 as f64 / 1e7, 2)
}

/// A command line made one line, cut at [`COMMAND_CHARS`] characters with `...`.
fn command_text(command: &str) -> String {
    let text = one_line(command);
    if text.chars().count() <= COMMAND_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(COMMAND_CHARS).collect();
    cut.push_str("...");
    cut
}

/// `text` with every run of CR and LF made one space and nothing else changed, as PowerShell's
/// `-replace '[\r\n]+', ' '` does.
fn one_line(text: &str) -> String {
    let mut line = String::with_capacity(text.len());
    let mut after_break = false;
    for ch in text.chars() {
        let is_break = ch == '\r' || ch == '\n';
        if !is_break {
            line.push(ch);
        } else if !after_break {
            line.push(' ');
        }
        after_break = is_break;
    }
    line
}

/// `bytes` in whole mebibytes, halves to even.
fn whole_mebibytes(bytes: u64) -> u64 {
    let (whole, rest) = (bytes / MEBIBYTE, bytes % MEBIBYTE);
    let twice = rest * 2;
    if twice > MEBIBYTE || (twice == MEBIBYTE && whole % 2 == 1) {
        whole + 1
    } else {
        whole
    }
}

/// `value` rounded to `digits` decimals with halves to even, as PowerShell's `[math]::Round` does: like .NET, a value
/// that is not a number or is at least [`ROUND_LIMIT`] in magnitude comes back unchanged.
fn round_half_even(value: f64, digits: i32) -> f64 {
    if value.is_nan() || value.abs() >= ROUND_LIMIT {
        return value;
    }
    let scale = 10_f64.powi(digits);
    (value * scale).round_ties_even() / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mebibyte, the unit the working set prints in.
    const MB: u64 = 1_048_576;

    /// The ticks in a millisecond of CPU time.
    const TICKS_PER_MS: u64 = 10_000;

    /// `ms` milliseconds of CPU time, in ticks.
    fn cpu(ms: u64) -> Ticks {
        Ticks(ms * TICKS_PER_MS)
    }

    /// A job process `pid` whose every part could be read.
    fn process(pid: u32) -> ProcessFacts {
        ProcessFacts {
            pid,
            parent: Some(4),
            name: "cargo.exe".to_owned(),
            cpu: Some(cpu(1500)),
            working_set_bytes: Some(10 * MB),
            command_line: Some("cargo test".to_owned()),
        }
    }

    /// A sample at `wall` seconds, outside WSL, with three processes in the job.
    fn sample(wall: f64) -> SampleFacts {
        SampleFacts {
            wall_seconds: wall,
            adjusted_seconds: 6.06,
            free_percent: 49,
            out_bytes: 1024,
            err_bytes: 0,
            job_cpu: cpu(3456),
            wsl_vm: WslVm::NotWatched,
            processes: 3,
            escaped_cpu: cpu(2500),
        }
    }

    /// A report that gathered nothing and was not cut.
    fn empty() -> Report {
        Report {
            job: Vec::new(),
            job_unread: 0,
            outside: Vec::new(),
            outside_unread: 0,
            samples: Vec::new(),
            cut_at_ms: None,
        }
    }

    #[test]
    fn the_block_keeps_the_powershell_order_and_wording() {
        let mut report = empty();
        report.job = vec![process(1)];
        report.outside = vec![process(2)];
        report.samples = vec![sample(12.34)];
        report.cut_at_ms = Some(2003);
        assert_eq!(
            lines(&report),
            [
                "gate: diagnostics",
                "gate:   job 1 parent 4 cargo.exe: cpu 1.5 s, working set 10 MB: cargo test",
                "gate:   outside job 2 parent 4 cargo.exe: cpu 1.5 s, working set 10 MB: cargo test",
                "gate:   sample at 12.3 s (load-adjusted 6.1 s, free 49%): out 1024 B, err 0 B, job cpu 3.46 s, 3 processes, \
                 escaped cpu 2.5 s",
                "gate:   ... diagnostics cut at 2003 ms",
            ]
        );
    }

    #[test]
    fn job_lines_are_sorted_by_pid_and_capped_at_forty() {
        let mut report = empty();
        report.job = (1..=45).rev().map(process).collect();
        report.job.swap(3, 30);
        let lines = lines(&report);
        assert_eq!(lines.len(), 43, "the heading, 40 job lines, the count and no samples");
        for (index, line) in lines[1..41].iter().enumerate() {
            assert!(
                line.starts_with(&format!("gate:   job {} parent 4 ", index + 1)),
                "line {index} is pid {}, got {line}",
                index + 1
            );
        }
        assert_eq!(lines[41], "gate:   ... and 5 more job processes");
    }

    #[test]
    fn unread_job_processes_are_counted_with_the_rest() {
        let mut report = empty();
        report.job = (1..=10).map(process).collect();
        report.job_unread = 20;
        let lines = lines(&report);
        assert_eq!(lines.len(), 13, "the heading, 10 job lines, the count and no samples");
        assert!(lines[10].starts_with("gate:   job 10 parent 4 "), "got {}", lines[10]);
        assert_eq!(lines[11], "gate:   ... and 20 more job processes");

        let mut report = empty();
        report.job = (1..=45).map(process).collect();
        report.job_unread = 3;
        assert_eq!(
            super::lines(&report)[41],
            "gate:   ... and 8 more job processes",
            "the 5 read past the cap and the 3 unread"
        );
    }

    #[test]
    fn unread_outside_processes_alone_print_only_the_count() {
        let mut report = empty();
        report.outside_unread = 5;
        assert_eq!(
            lines(&report),
            [
                "gate: diagnostics",
                "gate:   ... and 5 more outside job processes",
                "gate:   samples: none",
            ]
        );
    }

    #[test]
    fn outside_lines_keep_their_order_and_are_capped_at_twenty() {
        let mut report = empty();
        report.outside = (1..=25).rev().map(process).collect();
        let lines = lines(&report);
        assert_eq!(lines.len(), 23, "the heading, 20 outside lines, the count and no samples");
        assert!(lines[1].starts_with("gate:   outside job 25 parent 4 "), "got {}", lines[1]);
        assert!(lines[20].starts_with("gate:   outside job 6 parent 4 "), "got {}", lines[20]);
        assert_eq!(lines[21], "gate:   ... and 5 more outside job processes");
    }

    #[test]
    fn a_command_line_is_made_one_line_and_cut_at_two_hundred() {
        let mut long = process(1);
        long.command_line = Some(format!("{}\r\n{}", "a".repeat(100), "b".repeat(148)));
        let mut breaks = process(2);
        breaks.command_line = Some("cargo\r\n\n\rtest\nrun".to_owned());
        let mut exact = process(3);
        exact.command_line = Some("c".repeat(200));
        let mut report = empty();
        report.job = vec![long, breaks, exact];
        let lines = lines(&report);
        let cut = format!("{} {}...", "a".repeat(100), "b".repeat(99));
        assert!(lines[1].ends_with(&format!("MB: {cut}")), "got {}", lines[1]);
        assert!(
            lines[2].ends_with("MB: cargo test run"),
            "a run of CR and LF is one space, got {}",
            lines[2]
        );
        assert!(lines[3].ends_with(&format!("MB: {}", "c".repeat(200))), "200 characters are kept whole");
    }

    #[test]
    fn unreadable_parts_print_a_question_mark() {
        let mut report = empty();
        report.job = vec![ProcessFacts {
            pid: 7,
            parent: None,
            name: "svchost.exe".to_owned(),
            cpu: None,
            working_set_bytes: None,
            command_line: None,
        }];
        let lines = lines(&report);
        assert_eq!(lines[1], "gate:   job 7 parent ? svchost.exe: cpu ?, working set ?: ?");
    }

    #[test]
    fn nothing_gathered_says_none() {
        assert_eq!(lines(&empty()), ["gate: diagnostics", "gate:   samples: none"]);
    }

    #[test]
    fn nine_samples_print_the_last_six_oldest_first() {
        let mut report = empty();
        report.samples = (1..=9).map(|wall| sample(f64::from(wall))).collect();
        let lines = lines(&report);
        assert_eq!(lines.len(), 7, "the heading and six samples");
        for (index, line) in lines[1..].iter().enumerate() {
            assert!(
                line.starts_with(&format!("gate:   sample at {} s ", index + 4)),
                "sample {index} is the one at {} s, got {line}",
                index + 4
            );
        }
    }

    #[test]
    fn each_wsl_vm_form_and_the_process_count() {
        let mut unavailable = sample(1.0);
        unavailable.wsl_vm = WslVm::Unavailable;
        unavailable.processes = 1;
        let mut percent = sample(2.0);
        percent.wsl_vm = WslVm::Percent(312.25);
        let mut report = empty();
        report.samples = vec![sample(0.0), unavailable, percent];
        let lines = lines(&report);
        assert!(lines[1].contains("job cpu 3.46 s, 3 processes, escaped"), "got {}", lines[1]);
        assert!(
            lines[2].contains("job cpu 3.46 s, wsl vm cpu unavailable, 1 process, escaped"),
            "got {}",
            lines[2]
        );
        assert!(
            lines[3].contains("job cpu 3.46 s, wsl vm cpu 312.2%, 3 processes, escaped"),
            "got {}",
            lines[3]
        );
    }

    #[test]
    fn a_cut_gathering_says_when() {
        let mut report = empty();
        report.cut_at_ms = Some(0);
        assert_eq!(lines(&report).last().map(String::as_str), Some("gate:   ... diagnostics cut at 0 ms"));
    }

    #[test]
    fn a_failed_block_is_the_heading_and_the_failed_line() {
        assert_eq!(
            failed_block("no snapshot"),
            ["gate: diagnostics", "gate: diagnostics failed: no snapshot"]
        );
    }

    #[test]
    fn the_failed_line_is_one_line() {
        assert_eq!(
            failed_line("the snapshot failed\r\n  at Snapshot,  line 3\r\n"),
            "gate: diagnostics failed: the snapshot failed   at Snapshot,  line 3",
            "an inner CRLF is one space, every space is kept, and the trailing CRLF is dropped"
        );
        assert_eq!(
            failed_line("no snapshot "),
            "gate: diagnostics failed: no snapshot ",
            "a trailing space of the message's own is kept"
        );
    }

    #[test]
    fn halves_round_to_even() {
        let mut down = process(1);
        down.cpu = Some(cpu(125));
        down.working_set_bytes = Some(5 * MB / 2);
        let mut up = process(2);
        up.cpu = Some(cpu(375));
        up.working_set_bytes = Some(7 * MB / 2);
        let mut halves = sample(0.25);
        halves.adjusted_seconds = 0.75;
        halves.job_cpu = cpu(125);
        let mut report = empty();
        report.job = vec![down, up];
        report.samples = vec![halves];
        let lines = lines(&report);
        assert!(lines[1].contains("cpu 0.12 s, working set 2 MB:"), "got {}", lines[1]);
        assert!(lines[2].contains("cpu 0.38 s, working set 4 MB:"), "got {}", lines[2]);
        assert!(
            lines[3].starts_with("gate:   sample at 0.2 s (load-adjusted 0.8 s, free 49%)"),
            "got {}",
            lines[3]
        );
        assert!(lines[3].contains("job cpu 0.12 s,"), "got {}", lines[3]);
    }

    #[test]
    fn rounding_leaves_what_dotnet_leaves() {
        assert!(round_half_even(f64::NAN, 2).is_nan());
        assert_eq!(round_half_even(f64::INFINITY, 1).to_bits(), f64::INFINITY.to_bits());
        let huge = 1.25e16_f64;
        assert_eq!(round_half_even(huge, 2).to_bits(), huge.to_bits());
    }

    #[test]
    fn sample_history_keeps_the_last_six_oldest_first() {
        let mut history = SampleHistory::default();
        assert_eq!(history.to_vec(), Vec::<SampleFacts>::new(), "a new history keeps nothing");
        for wall in 1..=9 {
            history.push(sample(f64::from(wall)));
        }
        let walls: Vec<f64> = history.to_vec().iter().map(|kept| kept.wall_seconds).collect();
        assert_eq!(walls, [4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
    }
}
