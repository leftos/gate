#!/usr/bin/env bash
# Tests install-macos.sh without downloading anything or touching the real HOME: the argument handling, the System
# step's dry run, the Login PATH block in a temporary HOME and the root refusal. Runs on macOS and in Git Bash.
# Usage: bash tools/builder/test-install-macos.sh
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
script="$here/install-macos.sh"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/test-install-macos.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
home="$scratch/home"
mkdir -p "$home"

# Runs the installer with the temporary HOME and CARGO_HOME, as the uid in $1 (the script's INSTALL_MACOS_TEST_UID
# hook); leaves its exit code in $rc and its stdout and stderr in $scratch/out.
rc=0
run_as() {
    local uid="$1"
    shift
    set +e
    env HOME="$home" CARGO_HOME="$scratch/cargo" INSTALL_MACOS_TEST_UID="$uid" bash "$script" "$@" >"$scratch/out" 2>&1
    rc=$?
    set -e
}
run() { run_as 501 "$@"; }

# Each expectation runs its arguments as a command and, when that fails, prints it and fails the case.
expect() {
    if ! "$@"; then
        echo "expected: $*"
        return 1
    fi
}
has() { grep -qF -- "$1" "$scratch/out"; }
absent() { [ ! -e "$1" ]; }
rc_is() { [ "$rc" = "$1" ]; }
lacks() { ! grep -qF -- "$1" "$2"; }
line_count() { grep -cxF -- "$1" "$2" || true; }

passes=0
fails=0
check() {
    local why
    if why=$("$2" 2>&1); then
        passes=$((passes + 1))
        printf 'ok   %s\n' "$1"
    else
        fails=$((fails + 1))
        printf 'FAIL %s: %s\n' "$1" "$why"
        sed 's/^/     | /' "$scratch/out"
    fi
}

expected_steps='.NET SDK 10.0.401
.NET SDK 8.0.425
.NET workload wasm-tools
PowerShell 7.6.6
Node.js 24.20.0
pnpm 12.5.1
uv 0.12.18 and prek 0.5.0
Rust (stable, 1.98.1, 1.96.1, 1.89.0)
cargo-nextest 0.9.148
Git LFS 3.8.0
Godot 4.7.2 mono
Bun 1.4.2
FFmpeg 9.0.2
Login PATH
Git config
Gate work root
System'

case_list() {
    run --list
    expect rc_is 0 && expect [ "$(cat "$scratch/out")" = "$expected_steps" ]
}

case_only_bogus() {
    run --prefix "$scratch/p-bogus" --root "$scratch/r-bogus" --only 'uv,Bogus'
    expect rc_is 2 && expect has "--only 'Bogus' matches no step. Steps: .NET SDK 10.0.401, " &&
        expect absent "$scratch/p-bogus" && expect absent "$scratch/r-bogus"
}

case_unknown_flag() {
    run --prefix "$scratch/p-unknown" --frobnicate
    expect rc_is 2 && expect has "unknown argument '--frobnicate'" && expect absent "$scratch/p-unknown"
}

case_relative_prefix() {
    run --prefix relative/dir --only Summary
    expect rc_is 2 && expect has "--prefix 'relative/dir' is not an absolute path"
}

case_system_dry_run() {
    local p="$scratch/p-system"
    run --prefix "$p" --root "$scratch/r-system" --only System
    expect rc_is 0 &&
        expect has 'would run: pmset -a sleep 0 disksleep 0 autorestart 1 womp 1' &&
        expect has 'would write /Library/LaunchDaemons/limit.maxfiles.plist (root:wheel, 0644)' &&
        expect has 'would run: launchctl bootstrap system /Library/LaunchDaemons/limit.maxfiles.plist' &&
        expect has 'would run: sysctl -w kern.maxfiles=524288' &&
        expect has 'would run: sysctl -w kern.maxfilesperproc=524288' &&
        expect has 'would run: systemsetup -setremotelogin on' &&
        expect has 'SKIP System: run with sudo and --system' &&
        expect grep -qF 'SKIP System: run with sudo and --system' "$p/install.log" &&
        expect [ "$(ls -A "$p")" = install.log ] &&
        expect absent "$scratch/r-system" && expect [ -z "$(ls -A "$home")" ]
}

case_system_needs_root() {
    run --prefix "$scratch/p-sysuser" --system --user nobody --only System
    expect rc_is 2 && expect has '--system needs root' && expect absent "$scratch/p-sysuser"
}

# The step's tool check needs the installed tools, so it may FAIL here; what is tested is the block itself.
case_login_path_block() {
    local zp="$home/.zprofile" second="$scratch/p-second"
    printf '%s\n' 'export KEEP_ME=1' >"$zp"
    run --prefix "$scratch/p-first" --only 'Login PATH'
    expect [ "$rc" -le 1 ] && expect grep -qF "$scratch/p-first/bin" "$zp" || return 1
    run --prefix "$second" --only 'Login PATH'
    expect [ "$rc" -le 1 ] &&
        expect [ "$(line_count '# >>> gate builder >>>' "$zp")" = 1 ] &&
        expect [ "$(line_count '# <<< gate builder <<<' "$zp")" = 1 ] &&
        expect lacks "$scratch/p-first" "$zp" &&
        expect [ "$(line_count "export PATH=\"$second/bin:$second/dotnet:$second/node/bin:$scratch/cargo/bin:\$PATH\"" "$zp")" = 1 ] &&
        expect [ "$(line_count "export DOTNET_ROOT=\"$second/dotnet\"" "$zp")" = 1 ] &&
        expect [ "$(line_count 'export DOTNET_CLI_TELEMETRY_OPTOUT=1' "$zp")" = 1 ] &&
        expect [ "$(line_count "export GATE_GODOT_ROOT=\"$second/godot\"" "$zp")" = 1 ] &&
        expect [ "$(head -n 1 "$zp")" = 'export KEEP_ME=1' ]
}

case_root_refused() {
    run_as 0 --prefix "$scratch/p-root" --only cargo-nextest
    expect rc_is 2 && expect has "running as root: the toolchain steps must land in the builder user's home" &&
        expect absent "$scratch/p-root"
}

case_root_step_beside_system_refused() {
    run_as 0 --prefix "$scratch/p-root-system" --system --user nobody --only 'cargo-nextest,System'
    expect rc_is 2 && expect has "step 'cargo-nextest 0.9.148' refuses to run as root" &&
        expect absent "$scratch/p-root-system"
}

check '--list prints every step and exits 0' case_list
check '--only with a prefix matching no step exits 2 and creates nothing' case_only_bogus
check 'an unknown flag exits 2' case_unknown_flag
check 'a relative --prefix exits 2' case_relative_prefix
check '--only System without --system prints the commands, logs SKIP and changes nothing' case_system_dry_run
check '--system without root exits 2' case_system_needs_root
check 'the Login PATH block is written once and replaced on rerun' case_login_path_block
check 'a toolchain step as root is refused' case_root_refused
check 'a toolchain step beside System under --system as root is refused' case_root_step_beside_system_refused

printf '%s passed, %s failed\n' "$passes" "$fails"
[ "$fails" = 0 ]
