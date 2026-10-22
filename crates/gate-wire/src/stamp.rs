//! The stamped line of a run's log: `<stamp> <tag> <text>`, where the stamp is the local time to the microsecond with
//! its numeric UTC offset (`2026-10-22T09:31:38.120001-07:00`) and the tag says whose line it is (`out`, `err`, `gat`);
//! and the texts of the lines that say a process joined or left the command's job, which are worded here alone.

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, Zoned};

/// The stamp's shape, one byte per byte of a stamp: `0` a digit, `+` the offset's sign, any other byte itself.
const STAMP_SHAPE: &[u8; 32] = b"0000-00-00T00:00:00.000000+00:00";

/// The bytes of a stamp.
const STAMP_LEN: usize = STAMP_SHAPE.len();

/// Whose line a stamped line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    /// The command's stdout, written `out`.
    Out,
    /// The command's stderr, written `err`.
    Err,
    /// The gate's own line, written `gat`.
    Gat,
}

impl Tag {
    /// The tag as a line writes it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Tag::Out => "out",
            Tag::Err => "err",
            Tag::Gat => "gat",
        }
    }

    /// The tag a line writes as `word`; `None` for any other word.
    fn of_word(word: &str) -> Option<Tag> {
        match word {
            "out" => Some(Tag::Out),
            "err" => Some(Tag::Err),
            "gat" => Some(Tag::Gat),
            _ => None,
        }
    }
}

/// Now, in the system's time zone. This and [`local`] are the only reads of the clock or the time zone in this crate.
#[must_use]
pub fn now() -> Zoned {
    local(Timestamp::now())
}

/// The moment `at`, such as a process's kernel create time, in the system's time zone, as [`now`] gives it.
#[must_use]
pub fn local(at: Timestamp) -> Zoned {
    at.to_zoned(TimeZone::system())
}

/// The line `<stamp> <tag> <text>`, or `<stamp> <tag>` for an empty `text`, so it never ends in a space. The stamp is
/// `YYYY-MM-DDTHH:MM:SS.ffffff±HH:MM`: always six fraction digits, truncated, and always a numeric offset, `+00:00` for
/// UTC. An offset with seconds, as some historical zones had, is written to its whole minutes.
#[must_use]
pub fn stamped(at: &Zoned, tag: Tag, text: &str) -> String {
    let offset = at.offset().seconds();
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    let stamp = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}{sign}{:02}:{:02}",
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.subsec_nanosecond() / 1_000,
        minutes / 60,
        minutes % 60,
    );
    if text.is_empty() {
        format!("{stamp} {}", tag.word())
    } else {
        format!("{stamp} {} {text}", tag.word())
    }
}

/// A log line read back: its stamp and tag when it has them, and its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unstamped<'a> {
    /// The stamp, as written; `None` for a line that is not stamped.
    pub stamp: Option<&'a str>,
    /// The tag; `None` for a line that is not stamped.
    pub tag: Option<Tag>,
    /// The text after the tag, or the whole line when it is not stamped.
    pub text: &'a str,
}

/// The parts of `line` as [`stamped`] wrote them. The parse is strict: a stamp of exactly the shape [`stamped`] writes,
/// one space, a known tag, then the end of the line or one space and the text. Any other line is untagged text,
/// whole; a text that itself starts with a stamp is kept whole as the text.
#[must_use]
pub fn unstamped(line: &str) -> Unstamped<'_> {
    parts(line).unwrap_or(Unstamped {
        stamp: None,
        tag: None,
        text: line,
    })
}

/// The parts of a stamped `line`; `None` when it is not one.
fn parts(line: &str) -> Option<Unstamped<'_>> {
    let stamp = line.get(..STAMP_LEN)?;
    if !is_stamp(stamp.as_bytes()) {
        return None;
    }
    let rest = line.get(STAMP_LEN..)?.strip_prefix(' ')?;
    let tag = Tag::of_word(rest.get(..3)?)?;
    let after = rest.get(3..)?;
    let text = if after.is_empty() { after } else { after.strip_prefix(' ')? };
    Some(Unstamped {
        stamp: Some(stamp),
        tag: Some(tag),
        text,
    })
}

/// Whether `bytes` have the stamp's shape, [`STAMP_SHAPE`].
fn is_stamp(bytes: &[u8]) -> bool {
    bytes.len() == STAMP_LEN
        && bytes.iter().zip(STAMP_SHAPE).all(|(&byte, &shape)| match shape {
            b'0' => byte.is_ascii_digit(),
            b'+' => byte == b'+' || byte == b'-',
            literal => byte == literal,
        })
}

/// How a process ended, as an exit line says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It exited with this code.
    Code(i64),
    /// A signal of this number ended it.
    Signal(i32),
    /// How it ended could not be read.
    Unknown,
}

/// `spawn pid=<pid> ppid=<ppid> <image>`: the process `pid` joined the command's job, started by `ppid`, running the
/// program file named `image`; `?` for a parent or an image that could not be read. An image of more than
/// [`IMAGE_CHARACTERS`] characters is cut to its first `IMAGE_CHARACTERS - 1` and `…`, so a process line stays short.
#[must_use]
pub fn spawn_text(pid: u32, ppid: Option<u32>, image: Option<&str>) -> String {
    let image = image.unwrap_or("?");
    let image = match image.char_indices().nth(IMAGE_CHARACTERS) {
        Some(_) => {
            let cut = image.char_indices().nth(IMAGE_CHARACTERS - 1).map_or(image.len(), |(at, _)| at);
            format!("{}\u{2026}", &image[..cut])
        }
        None => image.to_owned(),
    };
    format!("spawn pid={pid} ppid={} {image}", or_unknown(ppid))
}

/// The most characters of an image a spawn line shows, its `…` included.
pub const IMAGE_CHARACTERS: usize = 260;

/// `exit pid=<pid> code=<code> ran=<seconds>s`: the process `pid` left the command's job as `ended` says, `ran` after
/// it was created. A signal is written `signal=<n>` in place of the code, and an unknown end `code=?`. A code at or
/// above `0x80000000`, as a Windows status is, is written in decimal followed by its hex, ` (0xC0000005)`. `ran` has
/// six fraction digits, truncated, and is `ran=?` when unknown.
#[must_use]
pub fn exit_text(pid: u32, ended: Ended, ran: Option<SignedDuration>) -> String {
    let end = match ended {
        Ended::Code(code) if (0x8000_0000..=0xFFFF_FFFF).contains(&code) => format!("code={code} (0x{code:08X})"),
        Ended::Code(code) => format!("code={code}"),
        Ended::Signal(signal) => format!("signal={signal}"),
        Ended::Unknown => "code=?".to_owned(),
    };
    let ran = ran.map_or_else(|| "?".to_owned(), seconds);
    format!("exit pid={pid} {end} ran={ran}")
}

/// `spawn pid=<pid> missed (<why>)`: the process `pid` joined the command's job, but nothing more of it could be read.
#[must_use]
pub fn missed_text(pid: u32, why: &str) -> String {
    format!("spawn pid={pid} missed ({why})")
}

/// `left pid=<pid> (still running at the end of the watch)`: the process `pid` was still in the command's job when
/// the gate stopped watching it.
#[must_use]
pub fn left_text(pid: u32) -> String {
    format!("left pid={pid} (still running at the end of the watch)")
}

/// `gate pid=<pid> ppid=<ppid>`: the gate itself, the process `pid`, started by `ppid`; `?` for a parent that could
/// not be read.
#[must_use]
pub fn root_text(pid: u32, ppid: Option<u32>) -> String {
    format!("gate pid={pid} ppid={}", or_unknown(ppid))
}

/// `process lines stopped: <why>`: the job's process lines ended early, for the reason `why`, so a process still
/// running then has no exit or left line.
#[must_use]
pub fn stopped_text(why: &str) -> String {
    format!("process lines stopped: {why}")
}

/// Whether `text` is one of the process lines above: it starts `spawn pid=`, `exit pid=`, `left pid=`, `gate pid=` or
/// `process lines stopped:`.
#[must_use]
pub fn is_process_line(text: &str) -> bool {
    ["spawn pid=", "exit pid=", "left pid=", "gate pid=", "process lines stopped:"]
        .iter()
        .any(|start| text.starts_with(start))
}

/// `value` as a line writes it, `?` for `None`.
fn or_unknown(value: Option<u32>) -> String {
    value.map_or_else(|| "?".to_owned(), |value| value.to_string())
}

/// `duration` as `<seconds>.<six digits>s`, a minus before a negative one.
fn seconds(duration: SignedDuration) -> String {
    let sign = if duration.is_negative() { "-" } else { "" };
    let whole = duration.unsigned_abs();
    format!("{sign}{}.{:06}s", whole.as_secs(), whole.subsec_micros())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test's fixed time that cannot be built must fail loudly")]
mod tests {
    use jiff::civil::date;
    use jiff::tz::{Offset, TimeZone};
    use jiff::{SignedDuration, Zoned};
    use proptest::prelude::*;

    use super::{Ended, Tag, Unstamped, exit_text, is_process_line, left_text, missed_text, root_text, spawn_text, stamped, stopped_text, unstamped};

    /// 2026-10-22 09:31:38.000042 at the offset of `seconds` east of UTC.
    fn at_offset(seconds: i32) -> Zoned {
        let offset = Offset::from_seconds(seconds).expect("a test offset is in range");
        date(2026, 10, 22)
            .at(9, 31, 38, 42_000)
            .to_zoned(TimeZone::fixed(offset))
            .expect("a test time is in range")
    }

    #[test]
    fn a_stamp_has_six_fraction_digits_and_a_numeric_offset_west_east_and_at_utc() {
        assert_eq!(
            stamped(&at_offset(-7 * 3600), Tag::Out, "   Compiling foo"),
            "2026-10-22T09:31:38.000042-07:00 out    Compiling foo"
        );
        assert_eq!(
            stamped(&at_offset(5 * 3600 + 1800), Tag::Err, "warning: unused variable"),
            "2026-10-22T09:31:38.000042+05:30 err warning: unused variable"
        );
        assert_eq!(
            stamped(&at_offset(0), Tag::Gat, "gate: passed in 4 s"),
            "2026-10-22T09:31:38.000042+00:00 gat gate: passed in 4 s"
        );
    }

    #[test]
    fn a_stamp_truncates_below_the_microsecond_and_an_empty_text_leaves_no_trailing_space() {
        let zone = TimeZone::fixed(Offset::from_seconds(0).expect("UTC is in range"));
        let at = date(2026, 1, 2).at(3, 4, 5, 999_999_999).to_zoned(zone).expect("in range");

        assert_eq!(stamped(&at, Tag::Out, ""), "2026-01-02T03:04:05.999999+00:00 out");
    }

    #[test]
    fn a_plain_line_is_untagged_text_whole() {
        assert_eq!(
            unstamped("   Compiling foo v0.1.0"),
            Unstamped {
                stamp: None,
                tag: None,
                text: "   Compiling foo v0.1.0"
            }
        );
        assert_eq!(
            unstamped(""),
            Unstamped {
                stamp: None,
                tag: None,
                text: ""
            }
        );
    }

    #[test]
    fn a_stamped_line_with_an_empty_text_gives_its_stamp_and_tag() {
        assert_eq!(
            unstamped("2026-10-22T09:31:38.120001-07:00 gat"),
            Unstamped {
                stamp: Some("2026-10-22T09:31:38.120001-07:00"),
                tag: Some(Tag::Gat),
                text: ""
            }
        );
    }

    #[test]
    fn a_line_with_an_unknown_tag_or_a_short_stamp_or_no_space_after_the_tag_is_untagged() {
        for line in [
            "2026-10-22T09:31:38.120001-07:00 gab text",
            "2026-10-22T09:31:38.12000-07:00 out text",
            "2026-10-22T09:31:38.120001-07:0 out text",
            "2026-10-22T09:31:38.120001Z out text",
            "2026-10-22T09:31:38.120001-07:00 outtext",
            "2026-10-22T09:31:38.120001-07:00  out text",
            "2026-1a-22T09:31:38.120001-07:00 out text",
        ] {
            assert_eq!(
                unstamped(line),
                Unstamped {
                    stamp: None,
                    tag: None,
                    text: line
                },
                "{line}"
            );
        }
    }

    #[test]
    fn a_text_that_looks_like_a_stamp_is_kept_whole() {
        let line = "2026-10-22T09:31:38.120001-07:00 out 2026-10-22T09:31:38.120001-07:00 err inner";

        assert_eq!(
            unstamped(line),
            Unstamped {
                stamp: Some("2026-10-22T09:31:38.120001-07:00"),
                tag: Some(Tag::Out),
                text: "2026-10-22T09:31:38.120001-07:00 err inner"
            }
        );
        let bare = "2026-10-22T09:31:38.120001-07:00 err inner";
        assert_eq!(unstamped(bare).text, "inner", "a stamped text read on its own is its own line");
    }

    fn tags() -> impl Strategy<Value = Tag> {
        prop_oneof![Just(Tag::Out), Just(Tag::Err), Just(Tag::Gat)]
    }

    proptest! {
        #[test]
        fn a_stamped_line_reads_back_as_its_tag_and_text(
            seconds in -50_000_i32..=50_000,
            tag in tags(),
            text in "[^\r\n]*",
        ) {
            let at = at_offset(seconds / 60 * 60);
            let line = stamped(&at, tag, &text);
            let read = unstamped(&line);

            prop_assert_eq!(read.tag, Some(tag));
            prop_assert_eq!(read.text, text.as_str());
        }
    }

    #[test]
    fn a_spawn_names_its_parent_and_image_or_a_question_mark() {
        assert_eq!(spawn_text(4120, Some(3300), Some("cargo.exe")), "spawn pid=4120 ppid=3300 cargo.exe");
        assert_eq!(spawn_text(4120, None, None), "spawn pid=4120 ppid=? ?");
    }

    #[test]
    fn a_spawn_cuts_a_long_image_at_a_character_and_marks_the_cut() {
        let fits = "é".repeat(260);
        assert_eq!(spawn_text(1, Some(2), Some(&fits)), format!("spawn pid=1 ppid=2 {fits}"), "260 fit whole");

        let long = format!("{}.exe", "é".repeat(300));
        let line = spawn_text(1, Some(2), Some(&long));

        let image = line.strip_prefix("spawn pid=1 ppid=2 ").expect("the line starts as a spawn");
        assert_eq!(image.chars().count(), 260, "{image}");
        assert_eq!(
            image,
            format!("{}\u{2026}", "é".repeat(259)),
            "cut between two characters, ending in an ellipsis"
        );
    }

    #[test]
    fn an_exit_says_its_code_signal_or_unknown_and_its_run_time() {
        let ran = SignedDuration::new(2, 414_126_999);

        assert_eq!(exit_text(5200, Ended::Code(0), Some(ran)), "exit pid=5200 code=0 ran=2.414126s");
        assert_eq!(
            exit_text(5200, Ended::Signal(9), Some(SignedDuration::from_micros(42))),
            "exit pid=5200 signal=9 ran=0.000042s"
        );
        assert_eq!(exit_text(5200, Ended::Unknown, None), "exit pid=5200 code=? ran=?");
        assert_eq!(
            exit_text(5200, Ended::Code(0xC000_0005), Some(ran)),
            "exit pid=5200 code=3221225477 (0xC0000005) ran=2.414126s"
        );
        assert_eq!(exit_text(5200, Ended::Code(0x7FFF_FFFF), None), "exit pid=5200 code=2147483647 ran=?");
        assert_eq!(
            exit_text(5200, Ended::Code(-1), Some(SignedDuration::from_micros(-1_500_000))),
            "exit pid=5200 code=-1 ran=-1.500000s"
        );
    }

    #[test]
    fn a_missed_spawn_a_leftover_and_the_gate_itself_are_worded_exactly() {
        assert_eq!(
            missed_text(7, "ended before it could be opened"),
            "spawn pid=7 missed (ended before it could be opened)"
        );
        assert_eq!(left_text(7), "left pid=7 (still running at the end of the watch)");
        assert_eq!(root_text(3300, Some(2900)), "gate pid=3300 ppid=2900");
        assert_eq!(root_text(3300, None), "gate pid=3300 ppid=?");
        assert_eq!(
            stopped_text("the job was dropped before its events were finished"),
            "process lines stopped: the job was dropped before its events were finished"
        );
    }

    #[test]
    fn the_process_lines_are_told_from_other_text() {
        for text in [
            spawn_text(1, Some(2), Some("a.exe")),
            missed_text(1, "why"),
            exit_text(1, Ended::Code(0), None),
            left_text(1),
            root_text(1, None),
            stopped_text("why"),
        ] {
            assert!(is_process_line(&text), "{text}");
        }
        for text in ["spawned pid=1", "exit code=0", "", "gate: passed in 4 s", " spawn pid=1"] {
            assert!(!is_process_line(text), "{text}");
        }
    }
}
