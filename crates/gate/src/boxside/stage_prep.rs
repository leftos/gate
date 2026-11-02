//! A commit stage's preparation: the commands a repo declares in its tracked `.gate/stage.toml`, the schema of
//! `.gate/warmup.toml` ([`read_warmup`]) with [`DEFAULT_SECONDS`] when it sets no `timeout`, run in the stage's checkout
//! after its tools and packages and before its hooks, so a hook that needs restored output finds it. Any other run never
//! reads the file, and a repo without it prepares nothing.
//!
//! The commands run on every commit stage, in order, as a warm-up's do ([`each_command`]), under one budget in
//! load-adjusted seconds: the file's, held to a third of the run's ceiling so the commit's hooks keep real time
//! ([`budget_of`]). No start marker is said for them, so a refusal still falls back.
//!
//! The first command that fails, cannot start or runs past the budget refuses the run with status 75, as a failed tool
//! restore does: the commit's hooks do not run on this builder. So does a file naming `git` or `prek`, which a routed run
//! never sends, a process left holding the job's name, and a preparation that changed a tree of the run, since its edits
//! would pass for the hooks' fixes; ignored files, the point of a preparation, do not count. A stop or a client gone ends
//! the run instead, whatever else went wrong ([`Stage::failed`]). A file with problems or a command that exits clears
//! nothing in the work folder; a killed command leaves the interrupted-command record like any killed command, so the
//! next run clears what it left.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::PoisonError;
use std::time::Duration;

use gate_router::escaped;
use gate_router::warm::{WarmupFile, never_sent_problems, read_warmup};

use crate::boxside::lfs::RunLfs;
use crate::boxside::run::{
    At, Beat, CommandLines, Commands, Of, REFUSED_STATUS, Ran, Request, WorkGit, each_command, ended_clean, job_name_freed, stopped_status,
};
use crate::boxside::stamping;
use crate::console::Console;
use crate::remote::{Refusal, refused_line};

/// The file's budget for all of its commands together when it sets none, in seconds.
const DEFAULT_SECONDS: u64 = 600;
/// The share of the run's ceiling a preparation may spend, as a divisor.
const CEILING_SHARE: u64 = 3;
/// How long the hooks' start may wait for the name of the last command's job, in wall time.
const PREP_JOB_FREED_LIMIT: Duration = Duration::from_secs(30);
/// The line after a refusal's tag. A `tools` refusal can move to another builder before the PC.
const REFUSAL_LINE: &str = "gate: the stage's preparation failed on this builder; the commit's hooks will not run here";

/// What a `.gate/stage.toml` declares.
#[derive(Debug)]
struct Declared {
    /// The commands, in file order, each its words.
    commands: Vec<Vec<String>>,
    /// The budget it declares for all of them together, in seconds.
    seconds: u64,
}

/// What `<place>/.gate/stage.toml` declares, `None` when there is no such file; else its problems, among them a command
/// a routed run never sends ([`never_sent_problems`]).
fn declared(place: &Path) -> Result<Option<Declared>, Vec<String>> {
    let bytes = match fs::read(place.join(".gate").join("stage.toml")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(vec![format!("cannot read it: {error}")]),
    };
    let WarmupFile { commands, timeout_seconds } = read_warmup(&bytes)?;
    let problems = never_sent_problems(&commands);
    if !problems.is_empty() {
        return Err(problems);
    }
    let seconds = timeout_seconds.unwrap_or(DEFAULT_SECONDS);
    Ok(Some(Declared { commands, seconds }))
}

/// The budget in force: the file's `seconds`, held to a third of the run's `ceiling`, at least 1 s, when it has one.
fn budget_of(seconds: u64, ceiling: Option<u64>) -> u64 {
    ceiling.map_or(seconds, |ceiling| seconds.min((ceiling / CEILING_SHARE).max(1)))
}

/// A command's words as its lines show them.
fn spoken(words: &[String]) -> String {
    escaped(&words.join(" "))
}

/// A preparation's lines, under its `budget` in seconds.
struct PrepLines {
    budget: u64,
}

impl CommandLines for PrepLines {
    fn running(&self, at: &At<'_>) -> Option<String> {
        Some(format!("gate: remote stage prep: running {}", spoken(at.words)))
    }

    fn passed(&self, at: &At<'_>, took: Duration) -> Option<String> {
        Some(format!(
            "gate: remote stage prep: {} passed in {:.1} s",
            spoken(at.words),
            took.as_secs_f64()
        ))
    }

    fn ran_out(&self, budget: u64, at: &At<'_>) -> String {
        format!(
            "gate: remote stage prep failed: its budget of {budget} s ran out before {}",
            spoken(at.words)
        )
    }

    /// Nothing for a command a stop or a closed connection ended: it did not fail, the run did.
    fn failed(&self, ran: &Ran, at: &At<'_>, clean: bool) -> Option<String> {
        let ending = match ran {
            Ran::Exited(status) => format!("exited {status}"),
            Ran::NotStarted(status) => format!("could not start (status {status})"),
            Ran::Killed => format!("ran past its budget of {} s", self.budget),
        };
        clean.then(|| format!("gate: remote stage prep failed: {} {ending}", spoken(at.words)))
    }
}

/// Refuses the run, its failure already said: under the box's output lock, so no line lands between them, the `tools`
/// tag and the line saying the hooks do not run here; 75.
fn refused(request: &Request<'_>, beat: &Beat<'_>, console: &mut Console) -> u32 {
    let _held = beat.lock.lock().unwrap_or_else(PoisonError::into_inner);
    stamping::protocol(console, &refused_line(request.nonce, Refusal::Tools));
    stamping::err(console, REFUSAL_LINE);
    REFUSED_STATUS
}

/// How a preparation that did not pass ends.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// It failed, its run still clean: its lines, then the refusal.
    Refused,
    /// A stop or a closed connection ended it with this command's status.
    Status(u32),
    /// A stop or a closed connection ended it, and no status says which: [`stopped_status`].
    Stopped,
}

/// How a preparation ends whose run is `clean` ([`ended_clean`]), with `status` when a command gave one. A status of 75
/// on a stopped run is no refusal: it is the command's own, so it says nothing of the stop.
fn ending(clean: bool, status: Option<u32>) -> Ending {
    match status.filter(|status| *status != REFUSED_STATUS) {
        _ if clean => Ending::Refused,
        Some(status) => Ending::Status(status),
        None => Ending::Stopped,
    }
}

/// One commit stage's preparation.
struct Stage<'a, 'b> {
    request: &'a Request<'b>,
    beat: &'a Beat<'b>,
}

impl Stage<'_, '_> {
    /// The one way a preparation fails: `failures` said and the run refused ([`refused`]), or, once a stop or a closed
    /// connection ended it, what that ends on, with no line of the failure and no tag. `status` is a command's.
    fn failed(&self, failures: &[String], status: Option<u32>, console: &mut Console) -> u32 {
        match ending(ended_clean(self.beat), status) {
            Ending::Refused => {
                for failure in failures {
                    self.beat.say(console, &format!("gate: remote stage prep failed: {failure}"));
                }
                refused(self.request, self.beat, console)
            }
            Ending::Status(status) => status,
            Ending::Stopped => stopped_status(self.request, self.beat, console),
        }
    }

    /// Runs the file's commands under their budget; `Err` with the status the run ends on.
    fn commands(&self, file: &Declared, place: &Path, console: &mut Console) -> Result<(), u32> {
        let budget = budget_of(file.seconds, self.request.timeout);
        let list = Commands {
            list: &file.commands,
            place,
            budget: Some(budget),
            of: Of::StagePrep,
        };
        match each_command(self.request, &list, &PrepLines { budget }, self.beat, console) {
            Ok(0) => Ok(()),
            // Its line said, or none for a stop or a closed connection.
            Ok(status) => Err(self.failed(&[], Some(status), console)),
            Err(status) => Err(self.failed(&[format!("a command ended the run with status {status}")], Some(status), console)),
        }
    }

    /// Waits for the last command's job name, which the hooks' job takes; `Err` with the status the run ends on when a
    /// process it left still holds the name.
    fn job_freed(&self, console: &mut Console) -> Result<(), u32> {
        if job_name_freed(self.request.job, self.beat, PREP_JOB_FREED_LIMIT, console) {
            return Ok(());
        }
        Err(self.failed(&["a process it left still holds its job".to_owned()], None, console))
    }

    /// Checks the run's trees are as their snapshots had them; `Err` with the status the run ends on.
    fn trees_unchanged(&self, (git, lfs): (&WorkGit, &RunLfs), console: &mut Console) -> Result<(), u32> {
        let failure = match git.tree_changes(self.request, lfs, console) {
            Ok(paths) => match paths.first() {
                None => return Ok(()),
                Some(first) => format!("it changed {} file(s) in the tree, first {}", paths.len(), escaped(first)),
            },
            Err(_) => "its change to the tree could not be read".to_owned(),
        };
        Err(self.failed(&[failure], None, console))
    }
}

/// Runs the preparation `<place>/.gate/stage.toml` declares when the run is a commit stage's. `Ok` when the run is
/// another's, there is no file, or every command passed and left the trees as they were; else `Err` with the status the
/// run ends on, its lines already said ([`Stage::failed`]).
pub(crate) fn update(request: &Request<'_>, held: (&WorkGit, &RunLfs), place: &Path, beat: &Beat<'_>, console: &mut Console) -> Result<(), u32> {
    if !request.commit_stage() {
        return Ok(());
    }
    let stage = Stage { request, beat };
    let file = match declared(place) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(()),
        Err(problems) => {
            let failures: Vec<String> = problems.iter().map(|problem| format!(".gate/stage.toml: {}", escaped(problem))).collect();
            return Err(stage.failed(&failures, None, console));
        }
    };
    stage.commands(&file, place, console)?;
    stage.job_freed(console)?;
    stage.trees_unchanged(held, console)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{DEFAULT_SECONDS, Ending, REFUSED_STATUS, budget_of, declared, ending};

    /// A place folder holding `.gate/stage.toml` with `content`, removed by hand at the test's end.
    fn place(test: &str, content: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-stage-prep-{}-{test}", std::process::id()));
        fs::create_dir_all(folder.join(".gate")).expect("the folder can be made");
        fs::write(folder.join(".gate").join("stage.toml"), content).expect("the file can be written");
        folder
    }

    /// The seconds the file `content` declares.
    fn seconds(test: &str, content: &str) -> u64 {
        let folder = place(test, content);
        let read = declared(&folder);
        fs::remove_dir_all(&folder).expect("the folder can be removed");
        read.expect("the file reads").expect("the file is there").seconds
    }

    #[test]
    fn a_file_with_no_timeout_declares_the_default() {
        assert_eq!(seconds("no_timeout", "commands = [['a']]\n"), DEFAULT_SECONDS);
        assert_eq!(DEFAULT_SECONDS, 600);
    }

    #[test]
    fn a_file_with_a_timeout_declares_its_value() {
        assert_eq!(seconds("timeout", "commands = [['a']]\ntimeout = 1800\n"), 1800);
    }

    #[test]
    fn the_budget_is_the_file_s_with_no_ceiling() {
        assert_eq!(budget_of(600, None), 600);
    }

    #[test]
    fn a_small_ceiling_holds_the_budget_to_its_third() {
        assert_eq!(budget_of(600, Some(90)), 30);
    }

    #[test]
    fn a_large_ceiling_leaves_the_file_s_budget() {
        assert_eq!(budget_of(600, Some(3600)), 600);
    }

    #[test]
    fn a_failure_on_a_clean_run_is_refused() {
        assert_eq!(ending(true, None), Ending::Refused);
        assert_eq!(ending(true, Some(3)), Ending::Refused);
        assert_eq!(ending(true, Some(REFUSED_STATUS)), Ending::Refused);
    }

    #[test]
    fn a_stop_before_a_bad_file_or_with_a_changed_tree_ends_as_the_stop() {
        assert_eq!(ending(false, None), Ending::Stopped);
    }

    #[test]
    fn a_stopped_command_ends_on_its_status() {
        assert_eq!(ending(false, Some(124)), Ending::Status(124));
    }

    #[test]
    fn a_command_exiting_75_on_a_stopped_run_is_no_refusal() {
        assert_eq!(ending(false, Some(REFUSED_STATUS)), Ending::Stopped);
    }

    #[test]
    fn a_ceiling_under_three_gives_one_second() {
        assert_eq!(budget_of(600, Some(2)), 1);
    }
}
