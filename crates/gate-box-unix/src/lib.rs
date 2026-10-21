//! The Unix gate box: the builder side of a routed run on a macOS builder, `gate-box box <verb> --stdin`, saying the
//! lines and statuses a Windows box's `gate box` says, so the PC's remote client reads both alike. The request is read
//! as the Windows box reads it and parsed by `gate-wire` under [`gate_wire::PathForm::Unix`].
//!
//! The modules that read no operating system state (the request, the CLI words, the lines and the console) build on
//! every target, so their tests run on Windows too; the verbs and the process helpers are Unix only.

pub mod cli;
pub mod console;
pub mod lines;
pub mod request;

#[cfg(unix)]
pub mod boot;
#[cfg(unix)]
pub mod files;
#[cfg(unix)]
pub mod preflight;
#[cfg(unix)]
pub mod probe;
#[cfg(unix)]
pub mod process;
#[cfg(unix)]
pub mod stop;
