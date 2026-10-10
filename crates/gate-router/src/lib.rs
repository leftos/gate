//! Which heavy commands run on the builder, over strings: the routing file `remote.json` ([`Config`]), PowerShell's
//! `-like` wildcard match its patterns use ([`Pattern`]), the in-worktree path rewrite ([`rewrite_words`]), the
//! builder's work-folder key ([`work_folder_key`]) and the decision itself ([`decide`]), which drops `--no-build` from a
//! dotnet command's words for the builder and turns a bare first word naming a file of the gate's folder into `./name`.
//!
//! The crate is pure: it reads no file, runs no git or other process and reads no environment or clock. The decision
//! asks the caller for each fact it needs through [`Facts`], and only at the rule that needs it, so it builds and tests
//! everywhere.

mod config;
mod decide;
mod key;
mod like;
mod rewrite;

pub use config::{Config, ConfigProblem, DEFAULT_OUTPUTS, ignored_line};
pub use decide::{DROPPED_NO_BUILD_LINE, Facts, Inputs, LocalReason, NO_OUTPUTS_LINE, Place, RemoteRun, Route, Slot, decide};
pub use key::{repo_name, work_folder_key};
pub use like::{InvalidPattern, Pattern};
pub use rewrite::{Rewritten, rewrite_words};
