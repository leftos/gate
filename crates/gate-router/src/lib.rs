//! Which heavy commands run on the builder, over strings: the routing file `remote.json` ([`Config`]), PowerShell's
//! `-like` wildcard match its patterns use ([`Pattern`]), the in-worktree path rewrite ([`rewrite_words`]), the
//! builder's work-folder key ([`work_folder_key`]) and the decision itself ([`decide`]), which sends a first word naming
//! a program by a path outside the worktree as the program's name, drops `--no-build` from a dotnet command's words for
//! the builder and turns a bare first word naming a file of the gate's folder into `./name`, never a word it sent as a
//! program's name, and the builder warm-up's file, hash, judge and texts ([`warm`]), with the TOML key spelling and
//! parse failure text its readers share with the gate ([`spelled`], [`parse_failure`]), and the repo's desktop
//! declaration ([`desktop`]). It holds too the caller's
//! environment a routed run does not send ([`dropped_env`]) and the repo's declaration of the prefixes its commands
//! read ([`env`]) and of the commands that stay on this PC ([`local`]).
//!
//! The crate is pure: it reads no file, runs no git or other process and reads no environment or clock. The decision
//! asks the caller for each fact it needs through [`Facts`], and only at the rule that needs it, so it builds and tests
//! everywhere.

mod config;
mod decide;
pub mod desktop;
pub mod env;
mod escape;
mod key;
mod like;
mod list;
pub mod local;
mod rewrite;
pub mod warm;
mod watch;

pub use config::{CARGO_OUTPUTS, Config, ConfigProblem, DEFAULT_OUTPUTS, ignored_line, is_usable_host};
pub use decide::{
    BuildSettings, BuildTarget, BuilderWords, CarriedSibling, DROPPED_NO_BUILD_LINE, DotnetTarget, Facts, FolderId, Inputs, LocalReason,
    NO_OUTPUTS_LINE, OUTPUT_PROPERTIES, PROJECT_EXTENSIONS, Place, RemoteRun, Route, SendPlan, SiblingFolder, Slot, Target, build_settings,
    builder_words, decide, dotnet_builds, dotnet_target, is_cargo, is_dotnet, sdk_build_target, sibling_root, stays_local, target, without_no_build,
};
pub use escape::{escaped, is_reordering_mark};
pub use key::{is_layout_char, is_layout_name, repo_name, work_folder_key};
pub use like::{InvalidPattern, Pattern};
pub use rewrite::{RewritePlace, Rewritten, SiblingRoot, rewrite_words};
pub use warm::{parse_failure, spelled};
pub use watch::{dropped_env, dropped_env_line};
