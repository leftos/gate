//! The usage text, printed on stdout for `--help` and on stderr after a usage error.

/// The usage, without a final line feed.
pub const USAGE: &str = "\
usage: gate run --slot heavy|light|critical --timeout <seconds> --log <path> [options] -- <command> [args...]
       gate stop-tree <pid>
       gate cutover
       gate push
       gate burn [--cores <n>] --seconds <s>
       gate --version | --help

run options:
  --stall <seconds>    stall window, default 120
  --tail <lines>       log lines printed at the end, default 20
  --no-markers         the command's status is the verdict
  --local              never route this run to a builder
  --state-dir <path>   use another state root (also GATE_STATE_DIR)

--slot, --timeout, --log and the command are all required.
The PowerShell form (gate -Log <path> -TimeoutSeconds <n> -Slot <kind> [...] -- <command>) is also accepted.";
