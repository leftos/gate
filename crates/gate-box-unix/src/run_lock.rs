//! A run's `--lock <name>` (`RunLock`): a builder-wide exclusive lock held from just before the command starts to the
//! end of the run's command, back commit, cross build and pack. The lock is an `flock` on `<root>/locks/<name>.lock`,
//! a file never removed, beneath the box user's own folder `<root>/locks` at mode 0700. Its holder writes a record
//! beside it, `<root>/locks/<name>.holder.json`, once it holds the lock, naming its job, work folder, repo, PC, box
//! and start; a waiter names the holder from it. The kernel lets the `flock` go when the holder's box dies, while the
//! command's process group may live on: a taker that finds the record's work folder's `.pgid` still naming the
//! record's job and judged current keeps the `flock` and waits on, so no two runs' commands under one name are alive
//! at once. The holder removes its record before it lets the lock go, unless its own group is still there.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gate_wire::escaped;
use serde_json::{Value, json};

use crate::console::{GAT, LineSink, Output, Stamp, Stream};
use crate::drain;
use crate::files::{self, HeldLock, Locked};
use crate::lines::{JobRecord, job_record};
use crate::prune::group_current;

/// The folder beneath the work root holding the run locks and their holder records.
pub const LOCKS: &str = "locks";
/// The longest one turn of a lock wait pauses.
const STEP: Duration = Duration::from_millis(100);
/// How long a waiter goes before naming the same holder again.
const SAY_AGAIN: Duration = Duration::from_secs(30);
/// The line a wait with no heartbeat pump says in its place: the pump's idle beat.
const BEAT_LINE: &str = "gate-remote-heartbeat: 0";
/// The holder record's form.
const RECORD_VERSION: u64 = 1;

/// Whether the process group a work folder's `.pgid` records for a job is still there: [`group_live`], or a test's.
pub type GroupLive = fn(&Path, &str) -> bool;

/// Who holds a lock, as a waiter names it: `id` changes whenever the holder does, `line` is what the waiter says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    /// The holder's identity: its job, and whether it is an orphan.
    pub id: String,
    /// The waiting line naming it.
    pub line: String,
}

/// What one look at the lock found.
#[derive(Debug)]
pub enum Look<T> {
    /// The lock is taken and its record written: the command may start.
    Taken(T),
    /// Another holds it, or an orphan's group still runs.
    Held(Holder),
}

/// How a wait for the lock ended.
#[derive(Debug)]
pub enum Waited<T> {
    /// The lock is held.
    Took(T),
    /// The client went during the wait: nothing started.
    Gone,
    /// The lock stayed held past the wait's total: nothing started.
    RanOut,
    /// The lock could not be looked at or its record written: why.
    Failed(String),
}

/// What a lock wait reads and does: the run's ([`held`]) or a test's.
pub trait Waiting<T> {
    /// How long the wait has gone.
    fn elapsed(&self) -> Duration;
    /// Pauses for `step`.
    fn pause(&mut self, step: Duration);
    /// Whether the client is gone.
    fn gone(&self) -> bool;
    /// One look at the lock.
    ///
    /// # Errors
    ///
    /// Why the lock could not be looked at or its record written.
    fn look(&mut self) -> Result<Look<T>, String>;
    /// Says `line`.
    fn say(&self, line: &str);
    /// Says a heartbeat.
    fn beat(&self);
}

/// How long a wait may go, and how often it beats itself: `None` while the heartbeat pump beats.
#[derive(Clone, Copy, Debug)]
pub struct Bound {
    /// The wait's whole bound.
    pub total: Duration,
    /// The period of the wait's own beats.
    pub beat: Option<Duration>,
}

/// What a wait has said so far.
#[derive(Default)]
struct Said {
    /// The identity of the holder last named, and when.
    named: Option<(String, Duration)>,
    /// When the wait last beat itself.
    beaten: Duration,
}

impl Said {
    /// Names `holder` when no holder was named yet, another was, or [`SAY_AGAIN`] has passed since.
    fn named<T>(&mut self, holder: Holder, wait: &impl Waiting<T>) {
        let now = wait.elapsed();
        let again = match &self.named {
            Some((id, at)) => *id != holder.id || now.saturating_sub(*at) >= SAY_AGAIN,
            None => true,
        };
        if again {
            wait.say(&holder.line);
            self.named = Some((holder.id, now));
        }
    }

    /// Beats once `period` has passed since the last beat, when the wait beats itself.
    fn beaten<T>(&mut self, period: Option<Duration>, wait: &impl Waiting<T>) {
        let now = wait.elapsed();
        if let Some(period) = period
            && now.saturating_sub(self.beaten) >= period
        {
            wait.beat();
            self.beaten = now;
        }
    }
}

/// Waits for the lock `name` within `bound`, through `wait`. Each turn probes the client first, then looks at the
/// lock; a lock still held at the bound's end runs out, else the holder is named ([`Said::named`]), the wait beats
/// itself when it has no pump, and it pauses for at most [`STEP`], never past the bound. A lock taken after a waiting
/// line was said is said taken ([`took_line`]).
#[must_use]
pub fn waited<T>(name: &str, bound: Bound, wait: &mut impl Waiting<T>) -> Waited<T> {
    let mut said = Said::default();
    loop {
        if wait.gone() {
            return Waited::Gone;
        }
        let holder = match wait.look() {
            Ok(Look::Taken(taken)) => {
                if said.named.is_some() {
                    wait.say(&took_line(name, wait.elapsed()));
                }
                return Waited::Took(taken);
            }
            Ok(Look::Held(holder)) => holder,
            Err(why) => return Waited::Failed(why),
        };
        let left = bound.total.saturating_sub(wait.elapsed());
        if left.is_zero() {
            return Waited::RanOut;
        }
        said.named(holder, &*wait);
        said.beaten(bound.beat, &*wait);
        wait.pause(left.min(STEP));
    }
}

/// `gate: remote waiting for lock <name>: held by <repo> from <pc> for <n> s`, `repo` and `pc` escaped.
#[must_use]
pub fn held_line(name: &str, (repo, pc): (&str, &str), held: Duration) -> String {
    format!(
        "gate: remote waiting for lock {name}: held by {} from {} for {} s",
        escaped(repo),
        escaped(pc),
        held.as_secs()
    )
}

/// `gate: remote waiting for lock <name>: held by another run`: no record names the holder.
#[must_use]
pub fn another_line(name: &str) -> String {
    format!("gate: remote waiting for lock {name}: held by another run")
}

/// `gate: remote waiting for lock <name>: held by <repo> from <pc>, whose box ended; its command still runs`.
#[must_use]
pub fn orphan_line(name: &str, (repo, pc): (&str, &str)) -> String {
    format!(
        "gate: remote waiting for lock {name}: held by {} from {}, whose box ended; its command still runs",
        escaped(repo),
        escaped(pc)
    )
}

/// `gate: remote took lock <name> after <n> s`.
#[must_use]
pub fn took_line(name: &str, waited: Duration) -> String {
    format!("gate: remote took lock {name} after {} s", waited.as_secs())
}

/// `gate: remote lock <name> stayed held past <n> s; nothing ran`.
#[must_use]
pub fn ran_out_line(name: &str, total: Duration) -> String {
    format!("gate: remote lock {name} stayed held past {} s; nothing ran", total.as_secs())
}

/// `gate: remote could not take lock <name>: <why>`.
#[must_use]
pub fn failed_line(name: &str, why: &str) -> String {
    format!("gate: remote could not take lock {name}: {why}")
}

/// What a holder record names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HolderRecord {
    /// The holder's job.
    pub job: String,
    /// The holder's work folder, whose `.pgid` records its command's group.
    pub work: String,
    /// The holder's repo, from its payload.
    pub repo: String,
    /// The holder's PC, from its payload.
    pub pc: String,
    /// The holder's box.
    pub pid: u32,
    /// When it took the lock, in milliseconds since the Unix epoch.
    pub started_ms: u64,
}

impl HolderRecord {
    /// The record's JSON text.
    #[must_use]
    pub fn text(&self) -> String {
        json!({
            "v": RECORD_VERSION,
            "job": self.job,
            "work": self.work,
            "repo": clipped(&self.repo),
            "pc": clipped(&self.pc),
            "pid": self.pid,
            "startedMs": self.started_ms,
        })
        .to_string()
    }

    /// The record `bytes` hold; `None` for anything but a whole record of this form, which counts as stale.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Option<HolderRecord> {
        let value: Value = serde_json::from_slice(bytes).ok()?;
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        if value.get("v").and_then(Value::as_u64) != Some(RECORD_VERSION) {
            return None;
        }
        Some(HolderRecord {
            job: text("job")?,
            work: text("work")?,
            repo: text("repo")?,
            pc: text("pc")?,
            pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
            started_ms: value.get("startedMs")?.as_u64()?,
        })
    }

    /// The group record of the holder's work folder, `<work>.pgid`.
    fn group_record(&self) -> PathBuf {
        PathBuf::from(format!("{}.pgid", self.work))
    }
}

/// The most characters of a holder's repo and of its PC a record keeps: they only name the holder in a waiting line.
const SHOWN_CHARS: usize = 256;

/// `text`'s first [`SHOWN_CHARS`] characters.
fn clipped(text: &str) -> &str {
    text.char_indices()
        .nth(SHOWN_CHARS)
        .map_or(text, |(at, _)| text.get(..at).unwrap_or(text))
}

/// Milliseconds since the Unix epoch now.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// Whether the work folder record `pgid` names `job`'s group and judges it current, as `box stop` judges a record
/// ([`group_current`]), failing closed: a record missing or another job's is not live; one that cannot be read, or
/// names `job` but is malformed, is.
#[must_use]
pub fn group_live(pgid: &Path, job: &str) -> bool {
    let (bytes, written) = match files::read_plain_dated(pgid, files::RECORD_LIMIT) {
        Ok(read) => read,
        Err(error) => return error.kind() != io::ErrorKind::NotFound,
    };
    match job_record(&bytes, job) {
        JobRecord::Group(recorded) => group_current(recorded, written),
        JobRecord::Malformed => true,
        JobRecord::Other => false,
    }
}

/// What a read of a holder record found.
enum Read {
    /// No record, or one that does not parse: written whole by a rename, so no live holder's.
    Stale,
    /// The record could not be read: a holder is taken to be there.
    Unread,
    /// The record names this holder.
    Holder(HolderRecord),
}

/// Reads the holder record `path` ([`Read`]).
fn record_read(path: &Path) -> Read {
    // Whole or not at all: a record over the limit is never read cut short and taken for a malformed one.
    match files::read_whole(path, files::RECORD_LIMIT) {
        Ok(bytes) => HolderRecord::of(&bytes).map_or(Read::Stale, Read::Holder),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Read::Stale,
        Err(_) => Read::Unread,
    }
}

/// Whether no process has the pid `pid`: `kill(pid, 0)` fails with `ESRCH`.
fn box_gone(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` with signal 0 sends nothing and touches no memory.
    unsafe { libc::kill(pid, 0) != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) }
}

/// The waiting line naming `holder` of the lock `name` as an orphan: its box ended, its command still runs.
fn orphan(name: &str, holder: &HolderRecord) -> Holder {
    Holder {
        id: format!("orphan {}", holder.job),
        line: orphan_line(name, (&holder.repo, &holder.pc)),
    }
}

/// The waiting line naming another run that holds the lock `name`, no record naming it.
fn another(name: &str) -> Holder {
    Holder {
        id: "another".to_owned(),
        line: another_line(name),
    }
}

/// The lock file and holder record of the lock `name` under the work root `root`.
fn paths(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let folder = root.join(LOCKS);
    (folder.join(format!("{name}.lock")), folder.join(format!("{name}.holder.json")))
}

/// The lock `name` a run holds: its `flock`, let go when this is dropped, after its holder record is removed unless
/// the run's own command group is still there ([`GroupLive`]).
#[derive(Debug)]
pub struct RunLock {
    record: PathBuf,
    pgid: PathBuf,
    job: String,
    live: GroupLive,
    // Dropped after `drop` has run, so the record goes before the lock.
    _lock: HeldLock,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if !(self.live)(&self.pgid, &self.job) {
            // A record left behind names a run whose group is gone: the next taker judges it stale.
            let _ = std::fs::remove_file(&self.record);
        }
    }
}

/// What a run asks of its lock.
pub struct Asked<'a> {
    /// The lock's name.
    pub name: &'a str,
    /// The work root.
    pub root: &'a Path,
    /// What the run's holder record names, its start the take's.
    pub own: HolderRecord,
    /// How a group is judged live.
    pub live: GroupLive,
}

/// The run's lock wait: the real clock, the run's output and the lock's files.
struct RunWait<'a, S: LineSink> {
    asked: &'a Asked<'a>,
    output: &'a Output<S>,
    began: Instant,
    lock: PathBuf,
    record: PathBuf,
    held: Option<HeldLock>,
}

impl<S: LineSink> RunWait<'_, S> {
    /// The holder the record names to a waiter the `flock` turned away: an orphan when the record's box is gone (a
    /// waiter ahead holds the `flock` behind it), else named; another run when no record reads.
    fn busy(&self) -> Holder {
        let name = self.asked.name;
        match record_read(&self.record) {
            Read::Holder(holder) if box_gone(holder.pid) => orphan(name, &holder),
            Read::Holder(holder) => Holder {
                line: held_line(
                    name,
                    (&holder.repo, &holder.pc),
                    Duration::from_millis(now_ms().saturating_sub(holder.started_ms)),
                ),
                id: format!("held {}", holder.job),
            },
            Read::Stale | Read::Unread => another(name),
        }
    }

    /// With the `flock` held: the orphan the record names whose group still runs, or another run when the record
    /// cannot be read; else the run's own record written over a stale one.
    fn orphan_or_written(&mut self) -> Result<Option<Holder>, String> {
        match record_read(&self.record) {
            Read::Unread => return Ok(Some(another(self.asked.name))),
            Read::Holder(holder) if (self.asked.live)(&holder.group_record(), &holder.job) => {
                return Ok(Some(orphan(self.asked.name, &holder)));
            }
            Read::Holder(_) | Read::Stale => {}
        }
        let own = HolderRecord {
            started_ms: now_ms(),
            ..self.asked.own.clone()
        };
        files::replace_plain(&self.record, own.text().as_bytes()).map_err(|failure| failure.to_string())?;
        Ok(None)
    }
}

impl<S: LineSink> Waiting<HeldLock> for RunWait<'_, S> {
    fn elapsed(&self) -> Duration {
        self.began.elapsed()
    }

    fn pause(&mut self, step: Duration) {
        std::thread::sleep(step);
    }

    fn gone(&self) -> bool {
        self.output.client_gone() || self.output.client_gone_probe()
    }

    fn look(&mut self) -> Result<Look<HeldLock>, String> {
        if self.held.is_none() {
            match files::take_lock(&self.lock) {
                Ok(Locked::Held(held)) => self.held = Some(held),
                Ok(Locked::Busy) => return Ok(Look::Held(self.busy())),
                Err(error) => return Err(error.to_string()),
            }
        }
        match self.orphan_or_written()? {
            Some(orphan) => Ok(Look::Held(orphan)),
            None => self.held.take().map(Look::Taken).ok_or_else(|| "the lock was let go".to_owned()),
        }
    }

    fn say(&self, line: &str) {
        // A note: the wait goes on, and a client gone ends it at the next turn.
        let _ = self.output.say(Stream::Err, GAT, line);
    }

    fn beat(&self) {
        // As the pump's beat: a client gone ends the wait at the next turn.
        let _ = self.output.say(Stream::Err, Stamp::Bare, BEAT_LINE);
    }
}

/// Takes the lock `asked` names for a run, through `output`, waiting within `bound` ([`waited`]): `<root>/locks` readied
/// as the box user's own folder at mode 0700 first ([`drain::private_folder`]), a folder that is not refusing the wait.
#[must_use]
pub fn held<S: LineSink>(asked: &Asked<'_>, output: &Output<S>, bound: Bound) -> Waited<RunLock> {
    if let Err(why) = drain::private_folder(&asked.root.join(LOCKS)) {
        return Waited::Failed(why);
    }
    let (lock, record) = paths(asked.root, asked.name);
    let mut wait = RunWait {
        asked,
        output,
        began: Instant::now(),
        lock,
        record: record.clone(),
        held: None,
    };
    match waited(asked.name, bound, &mut wait) {
        Waited::Took(held) => Waited::Took(RunLock {
            record,
            pgid: asked.own.group_record(),
            job: asked.own.job.clone(),
            live: asked.live,
            _lock: held,
        }),
        Waited::Gone => Waited::Gone,
        Waited::RanOut => Waited::RanOut,
        Waited::Failed(why) => Waited::Failed(why),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, clippy::panic, reason = "a test whose case cannot be readied must fail loudly")]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        Asked, Bound, Holder, HolderRecord, Look, RunWait, Waited, Waiting, another_line, held, held_line, orphan_line, paths, ran_out_line,
        took_line, waited,
    };
    use crate::console::{Output, RecordingSink};
    use crate::files::{self, Locked};

    /// A wait on a fake clock: each look gives the next of `looks`, the last one again once they run out.
    struct Fake {
        now: Cell<Duration>,
        pauses: RefCell<Vec<Duration>>,
        looks: RefCell<Vec<Look<()>>>,
        looked: Cell<usize>,
        said: RefCell<Vec<String>>,
        beats: Cell<usize>,
    }

    impl Fake {
        fn new(looks: Vec<Look<()>>) -> Fake {
            Fake {
                now: Cell::new(Duration::ZERO),
                pauses: RefCell::new(Vec::new()),
                looks: RefCell::new(looks),
                looked: Cell::new(0),
                said: RefCell::new(Vec::new()),
                beats: Cell::new(0),
            }
        }
    }

    impl Waiting<()> for Fake {
        fn elapsed(&self) -> Duration {
            self.now.get()
        }

        fn pause(&mut self, step: Duration) {
            self.pauses.borrow_mut().push(step);
            self.now.set(self.now.get() + step);
        }

        fn gone(&self) -> bool {
            false
        }

        fn look(&mut self) -> Result<Look<()>, String> {
            self.looked.set(self.looked.get() + 1);
            let mut looks = self.looks.borrow_mut();
            let look = if looks.len() > 1 {
                looks.remove(0)
            } else {
                looks.first().map(copied).expect("a look is given")
            };
            Ok(look)
        }

        fn say(&self, line: &str) {
            self.said.borrow_mut().push(line.to_owned());
        }

        fn beat(&self) {
            self.beats.set(self.beats.get() + 1);
        }
    }

    /// A copy of `look`.
    fn copied(look: &Look<()>) -> Look<()> {
        match look {
            Look::Taken(()) => Look::Taken(()),
            Look::Held(holder) => Look::Held(holder.clone()),
        }
    }

    fn holder(id: &str) -> Look<()> {
        Look::Held(Holder {
            id: id.to_owned(),
            line: format!("held by {id}"),
        })
    }

    fn bound(total_ms: u64) -> Bound {
        Bound {
            total: Duration::from_millis(total_ms),
            beat: None,
        }
    }

    #[test]
    fn a_zero_lock_wait_is_one_try() {
        let mut fake = Fake::new(vec![holder("a")]);
        let waited = waited("signing", bound(0), &mut fake);
        assert!(matches!(waited, Waited::RanOut), "{waited:?}");
        assert_eq!(fake.looked.get(), 1, "one look");
        assert!(fake.said.borrow().is_empty(), "no waiting line: {:?}", fake.said.borrow());
        assert!(fake.pauses.borrow().is_empty(), "no pause");
    }

    #[test]
    fn a_lock_wait_never_runs_past_its_total() {
        let mut fake = Fake::new(vec![holder("a")]);
        let waited = waited("signing", bound(1_050), &mut fake);
        assert!(matches!(waited, Waited::RanOut), "{waited:?}");
        assert_eq!(fake.now.get(), Duration::from_millis(1_050), "the wait ends at its total");
        assert!(
            fake.pauses.borrow().iter().all(|step| *step <= Duration::from_millis(100)),
            "{:?}",
            fake.pauses.borrow()
        );
        assert_eq!(
            fake.pauses.borrow().last(),
            Some(&Duration::from_millis(50)),
            "the last pause stops at the total"
        );
    }

    #[test]
    fn a_waiter_names_the_holder_and_names_it_again_when_it_changes() {
        let mut looks = vec![holder("a"), holder("a"), holder("b"), holder("b"), holder("a")];
        // Held by `a` for 31 s of turns: named once more when 30 s have passed.
        looks.extend((0..310).map(|_| holder("a")));
        looks.push(Look::Taken(()));
        let mut fake = Fake::new(looks);
        let waited = waited("signing", bound(3_600_000), &mut fake);
        assert!(matches!(waited, Waited::Took(())), "{waited:?}");
        assert_eq!(
            *fake.said.borrow(),
            [
                "held by a",
                "held by b",
                "held by a",
                "held by a",
                took_line("signing", Duration::from_millis(31_500)).as_str()
            ],
            "named first, on each change, again after 30 s, and taken"
        );
        assert_eq!(
            took_line("signing", Duration::from_millis(31_500)),
            "gate: remote took lock signing after 31 s"
        );
    }

    #[test]
    fn a_wait_with_no_pump_beats_itself() {
        let mut fake = Fake::new(vec![holder("a")]);
        let bound = Bound {
            total: Duration::from_secs(16),
            beat: Some(Duration::from_secs(5)),
        };
        assert!(matches!(waited("signing", bound, &mut fake), Waited::RanOut));
        assert_eq!(fake.beats.get(), 3, "a beat at 5, 10 and 15 s");
        let mut pumped = Fake::new(vec![holder("a")]);
        assert!(matches!(
            waited("signing", super::Bound { beat: None, ..bound }, &mut pumped),
            Waited::RanOut
        ));
        assert_eq!(pumped.beats.get(), 0, "the pump beats instead");
    }

    #[test]
    fn the_holder_line_escapes_pc_and_repo() {
        assert_eq!(
            held_line("signing", ("re\npo", "p\tc"), Duration::from_millis(12_900)),
            "gate: remote waiting for lock signing: held by re\\npo from p\\tc for 12 s"
        );
        assert_eq!(
            orphan_line("signing", ("re\npo", "p\tc")),
            "gate: remote waiting for lock signing: held by re\\npo from p\\tc, whose box ended; its command still runs"
        );
        assert_eq!(another_line("signing"), "gate: remote waiting for lock signing: held by another run");
        assert_eq!(
            ran_out_line("signing", Duration::from_millis(2_500)),
            "gate: remote lock signing stayed held past 2 s; nothing ran"
        );
    }

    /// A work root private to this test, removed when it ends.
    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Root {
            let path = std::env::temp_dir().join(format!("gate-box-unix-lock-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::DirBuilder::new().recursive(true).mode(0o700).create(&path).expect("the root is made");
            Root(path)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn live(_: &Path, _: &str) -> bool {
        true
    }

    fn dead(_: &Path, _: &str) -> bool {
        false
    }

    fn own(job: &str) -> HolderRecord {
        HolderRecord {
            job: job.to_owned(),
            work: format!("/r/work/{job}"),
            repo: "repo".to_owned(),
            pc: "pc".to_owned(),
            pid: 7,
            started_ms: 0,
        }
    }

    fn asked<'a>(root: &'a Root, job: &str, judge: super::GroupLive) -> Asked<'a> {
        Asked {
            name: "signing",
            root: &root.0,
            own: own(job),
            live: judge,
        }
    }

    fn record_of(root: &Root) -> Option<HolderRecord> {
        let (_, record) = paths(&root.0, "signing");
        fs::read(record).ok().and_then(|bytes| HolderRecord::of(&bytes))
    }

    fn one_try() -> Bound {
        Bound {
            total: Duration::ZERO,
            beat: None,
        }
    }

    #[test]
    fn a_stale_holder_record_is_replaced_by_the_next_taker() {
        let root = Root::new("stale");
        let output = Output::new(RecordingSink::default());
        // A box that died holding the lock: its flock let go by the kernel, its record left, its group judged gone.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.0.join(super::LOCKS))
            .expect("the locks folder is made");
        let (_, record) = paths(&root.0, "signing");
        fs::write(&record, own("first").text()).expect("the dead holder's record is written");
        let second = held(&asked(&root, "second", dead), &output, one_try());
        assert!(matches!(second, Waited::Took(_)), "{second:?}");
        assert_eq!(
            record_of(&root).map(|record| record.job),
            Some("second".to_owned()),
            "the stale record is replaced"
        );
        drop(second);
        fs::write(&record, "{\"v\":1,\"job\":").expect("a malformed record is written");
        let third = held(&asked(&root, "third", dead), &output, one_try());
        assert!(matches!(third, Waited::Took(_)), "a malformed record is replaced too: {third:?}");
    }

    /// The locks folder of `root` made, and the work folder `<root>/<name>` whose `.pgid` is a folder, which no read
    /// can take for a record.
    fn unreadable_pgid(root: &Root, name: &str) -> String {
        let locks = root.0.join(super::LOCKS);
        if !locks.exists() {
            fs::DirBuilder::new().mode(0o700).create(&locks).expect("the locks folder is made");
        }
        let work = root.0.join(name).to_str().expect("the temp folder is UTF-8").to_owned();
        fs::create_dir(format!("{work}.pgid")).expect("a folder is put at the .pgid");
        work
    }

    #[test]
    fn an_unreadable_pgid_keeps_the_taker_waiting() {
        let root = Root::new("pgid-taker");
        let work = unreadable_pgid(&root, "holder");
        let (_, record) = paths(&root.0, "signing");
        fs::write(&record, HolderRecord { work, ..own("holder") }.text()).expect("the holder's record is written");
        let output = Output::new(RecordingSink::default());
        let waited = held(&asked(&root, "next", super::group_live), &output, one_try());
        assert!(matches!(waited, Waited::RanOut), "{waited:?}");
        assert_eq!(record_of(&root).map(|record| record.job), Some("holder".to_owned()), "the record stays");
    }

    #[test]
    fn an_unreadable_pgid_keeps_the_holder_s_record() {
        let root = Root::new("pgid-holder");
        let work = unreadable_pgid(&root, "own");
        let asking = Asked {
            own: HolderRecord { work, ..own("own") },
            ..asked(&root, "own", super::group_live)
        };
        let output = Output::new(RecordingSink::default());
        let took = held(&asking, &output, one_try());
        assert!(matches!(took, Waited::Took(_)), "{took:?}");
        drop(took);
        assert_eq!(record_of(&root).map(|record| record.job), Some("own".to_owned()), "the record is kept");
    }

    #[test]
    fn a_holder_record_that_cannot_be_read_keeps_the_taker_waiting() {
        let root = Root::new("record-unread");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.0.join(super::LOCKS))
            .expect("the locks folder is made");
        let (lock, record) = paths(&root.0, "signing");
        fs::create_dir(&record).expect("a folder is put at the record");
        let output = Output::new(RecordingSink::default());
        let asking = asked(&root, "next", dead);
        let mut wait = RunWait {
            asked: &asking,
            output: &output,
            began: std::time::Instant::now(),
            lock: lock.clone(),
            record,
            held: None,
        };
        let look = wait.look().expect("the lock is looked at");
        assert!(matches!(&look, Look::Held(holder) if holder.line == another_line("signing")), "{look:?}");
        assert!(
            matches!(files::take_lock(&lock), Ok(Locked::Busy)),
            "the taker holds the flock while it waits"
        );
        drop(wait);
        let waited = held(&asking, &output, one_try());
        assert!(matches!(waited, Waited::RanOut), "{waited:?}");
        assert!(
            matches!(files::take_lock(&lock), Ok(Locked::Held(_))),
            "the flock is let go with the wait"
        );
    }

    #[test]
    fn an_oversized_holder_record_keeps_the_taker_waiting() {
        let root = Root::new("record-oversized");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.0.join(super::LOCKS))
            .expect("the locks folder is made");
        let (_, record) = paths(&root.0, "signing");
        let long = "r".repeat(70 * 1024);
        let oversized =
            format!("{{\"v\":1,\"job\":\"orphan\",\"work\":\"/r/work/orphan\",\"repo\":\"{long}\",\"pc\":\"pc\",\"pid\":7,\"startedMs\":0}}");
        assert!(oversized.len() as u64 > files::RECORD_LIMIT, "the record is over the limit");
        fs::write(&record, &oversized).expect("the oversized record is written");
        let output = Output::new(RecordingSink::default());
        let waited = held(&asked(&root, "next", dead), &output, one_try());
        assert!(matches!(waited, Waited::RanOut), "{waited:?}");
        assert_eq!(
            fs::read_to_string(&record).expect("the record reads"),
            oversized,
            "the record is not replaced"
        );
        let clipped = HolderRecord {
            repo: long.clone(),
            pc: long,
            ..own("own")
        }
        .text();
        assert!(clipped.len() < 1024, "a record clips its repo and pc: {} bytes", clipped.len());
    }

    #[test]
    fn a_malformed_pgid_of_the_holder_s_job_keeps_the_taker_waiting() {
        let root = Root::new("pgid-malformed");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.0.join(super::LOCKS))
            .expect("the locks folder is made");
        let work = root.0.join("holder").to_str().expect("the temp folder is UTF-8").to_owned();
        fs::write(format!("{work}.pgid"), "holder\nnot a group\n").expect("a malformed .pgid is written");
        let (_, record) = paths(&root.0, "signing");
        fs::write(&record, HolderRecord { work, ..own("holder") }.text()).expect("the holder's record is written");
        let output = Output::new(RecordingSink::default());
        let waited = held(&asked(&root, "next", super::group_live), &output, one_try());
        assert!(matches!(waited, Waited::RanOut), "{waited:?}");
        assert_eq!(record_of(&root).map(|record| record.job), Some("holder".to_owned()), "the record stays");
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this starts a child to reap for its pid"
    )]
    fn a_waiter_behind_another_waiter_names_an_orphan_holder_as_such() {
        let root = Root::new("behind");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.0.join(super::LOCKS))
            .expect("the locks folder is made");
        let (lock, record) = paths(&root.0, "signing");
        let mut reaped = std::process::Command::new("/usr/bin/true").spawn().expect("a child starts");
        reaped.wait().expect("the child is reaped");
        let dead_box = HolderRecord {
            pid: reaped.id(),
            ..own("orphan")
        };
        fs::write(&record, dead_box.text()).expect("the dead box's record is written");
        // The waiter ahead, sitting on the orphan, holds the flock.
        let ahead = files::held_for_test(&lock);
        let asking = asked(&root, "behind", live);
        let output = Output::new(RecordingSink::default());
        let mut wait = RunWait {
            asked: &asking,
            output: &output,
            began: std::time::Instant::now(),
            lock: lock.clone(),
            record,
            held: None,
        };
        let look = wait.look().expect("the lock is looked at");
        let Look::Held(holder) = look else { panic!("held: {look:?}") };
        assert_eq!(holder.line, orphan_line("signing", ("repo", "pc")));
        drop(ahead);
    }

    #[test]
    fn a_holder_whose_group_is_still_there_keeps_its_record() {
        let root = Root::new("kept");
        let output = Output::new(RecordingSink::default());
        let kept = held(&asked(&root, "kept", live), &output, one_try());
        assert!(matches!(kept, Waited::Took(_)), "{kept:?}");
        drop(kept);
        assert_eq!(
            record_of(&root).map(|record| record.job),
            Some("kept".to_owned()),
            "a group still there keeps the record"
        );
        let (lock, _) = paths(&root.0, "signing");
        assert!(matches!(files::take_lock(&lock), Ok(Locked::Held(_))), "the flock is let go");

        let ended = held(&asked(&root, "ended", dead), &output, one_try());
        assert!(matches!(ended, Waited::Took(_)), "{ended:?}");
        drop(ended);
        assert_eq!(record_of(&root), None, "a group that ended takes the record with it");
        let (lock, _) = paths(&root.0, "signing");
        assert!(lock.exists(), "the lock file is never removed");
    }

    #[test]
    fn an_orphan_holder_is_named_as_such() {
        let root = Root::new("orphan");
        let output = Output::new(RecordingSink::default());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.0.join(super::LOCKS))
            .expect("the locks folder is made");
        let (lock, record) = paths(&root.0, "signing");
        let orphan = HolderRecord {
            repo: "re\npo".to_owned(),
            ..own("orphan")
        };
        fs::write(&record, orphan.text()).expect("the orphan's record is written");
        let asking = asked(&root, "next", live);
        let mut wait = RunWait {
            asked: &asking,
            output: &output,
            began: std::time::Instant::now(),
            lock: lock.clone(),
            record,
            held: None,
        };
        let look = wait.look().expect("the lock is looked at");
        let Look::Held(holder) = look else {
            panic!("an orphan holds it: {look:?}")
        };
        assert_eq!(holder.line, orphan_line("signing", ("re\npo", "pc")));
        assert_eq!(
            holder.line,
            "gate: remote waiting for lock signing: held by re\\npo from pc, whose box ended; its command still runs"
        );
        assert!(
            matches!(files::take_lock(&lock), Ok(Locked::Busy)),
            "the taker keeps the flock while it waits"
        );
        assert_eq!(
            record_of(&root).map(|record| record.job),
            Some("orphan".to_owned()),
            "the orphan's record stays"
        );
    }

    #[test]
    fn a_locks_folder_open_to_others_refuses_the_run() {
        let root = Root::new("open");
        let folder = root.0.join(super::LOCKS);
        fs::DirBuilder::new().mode(0o700).create(&folder).expect("the locks folder is made");
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).expect("the folder is opened to others");
        let output = Output::new(RecordingSink::default());
        let waited = held(&asked(&root, "open", dead), &output, one_try());
        let Waited::Failed(why) = waited else { panic!("refused: {waited:?}") };
        assert!(why.contains("is not the box user's own folder at mode 0700"), "{why}");
        assert!(!paths(&root.0, "signing").0.exists(), "no lock file is made");
    }
}
