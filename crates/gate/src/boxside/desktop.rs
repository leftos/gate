//! The builder's desktop: the decisions a run that hands its command to the builder's logged-on session makes on its
//! own, with no machine of its own.
//!
//! [`lines`] holds who may take the command, what a task start or the helper's end means, how long each wait lasts, and
//! every line the box says of them, each escaped into one line; [`verify`] holds what the box checks of the command the
//! helper reported before it takes it, and which processes a failed hand-off leaves to end. Nothing here reads a clock,
//! a handle or a process.

pub(crate) mod lines;
pub(crate) mod verify;
