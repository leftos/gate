#!/usr/bin/env bash
# Tests install-macos.sh without downloading anything or touching the real HOME: the argument handling, the Homebrew
# and FFmpeg steps' dry run, their --admin refusals, the System step's dry run with its prune-job plist (linted where
# plutil exists), the builder-home parse of dscl's output, the Login PATH block in a temporary HOME, the Private home
# step and the root refusal. Runs on macOS and in Git Bash.
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

# Runs the installer as uid $1 with INSTALL_MACOS_TEST_GROUPS in $2, the script's stand-in for `id -Gn`'s output.
run_groups_as() {
    local uid="$1" groups="$2"
    shift 2
    set +e
    env HOME="$home" CARGO_HOME="$scratch/cargo" INSTALL_MACOS_TEST_UID="$uid" INSTALL_MACOS_TEST_GROUPS="$groups" \
        bash "$script" "$@" >"$scratch/out" 2>&1
    rc=$?
    set -e
}
run_admin_as() {
    local groups="$1"
    shift
    run_groups_as 501 "$groups" "$@"
}

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
# A folder's mode, as stat spells it on the box this runs on.
mode_of() {
    if [ "$(uname -s)" = Darwin ]; then stat -f '%Lp' "$1"; else stat -c '%a' "$1"; fi
}
# Whether the run's output holds a line with each of the texts given, in the order given.
in_order() {
    local at=0 now text
    for text in "$@"; do
        now=$(grep -nF -- "$text" "$scratch/out" | head -n 1 | cut -d : -f 1)
        if [ -z "$now" ] || [ "$now" -le "$at" ]; then
            echo "'$text' is not after line $at"
            return 1
        fi
        at=$now
    done
}

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

expected_steps='Private home
.NET SDK 10.0.401
.NET SDK 8.0.425
.NET workload wasm-tools
PowerShell 7.6.6
Node.js 24.20.0
pnpm 12.5.1
uv 0.12.18 and prek 0.5.0
Rust (stable, 1.98.1, 1.96.1, 1.89.0)
cargo-nextest 0.9.148
just 1.58.0
Git LFS 3.8.0
Godot 4.7.2 mono
Bun 1.4.2
Homebrew
FFmpeg
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
    expect rc_is 2 && expect has "--only 'Bogus' matches no step. Steps: Private home, .NET SDK 10.0.401, " &&
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
    local p="$scratch/p-system" r="$scratch/r-system"
    local args="<string>--root</string><string>$r</string></array>"
    run --prefix "$p" --root "$r" --only System
    expect rc_is 0 &&
        expect has 'would run: pmset -a sleep 0 disksleep 0 autorestart 1 womp 1' &&
        expect has 'would write /Library/LaunchDaemons/limit.maxfiles.plist (root:wheel, 0644)' &&
        expect has 'would run: launchctl bootstrap system /Library/LaunchDaemons/limit.maxfiles.plist' &&
        expect has 'would run: sysctl -w kern.maxfiles=524288' &&
        expect has 'would run: sysctl -w kern.maxfilesperproc=524288' &&
        expect has 'would run: systemsetup -setremotelogin on' &&
        expect has 'would run: mdutil -a -i off' &&
        expect has 'would run: softwareupdate --install-rosetta --agree-to-license' &&
        expect has 'would write /Library/LaunchDaemons/dev.gate.prune-work.plist (root:wheel, 0644): <?xml' &&
        expect has '<key>Label</key><string>dev.gate.prune-work</string>' &&
        expect has "<array><string>$r/bin/gate-box</string><string>box</string><string>prune</string>$args" &&
        expect has '<key>UserName</key><string>{builder user}</string>' &&
        expect has '<key>StartInterval</key><integer>7200</integer>' &&
        expect has '<key>RunAtLoad</key><true/>' &&
        expect has '<key>LowPriorityIO</key><true/>' &&
        expect has '<key>ProcessType</key><string>Background</string>' &&
        expect has "<key>WorkingDirectory</key><string>$r</string>" &&
        expect has '<key>EnvironmentVariables</key>  <dict><key>HOME</key><string>{builder home}</string></dict>' &&
        expect has '<key>StandardOutPath</key><string>/Library/Logs/gate/prune-work.log</string>' &&
        expect has '<key>StandardErrorPath</key><string>/Library/Logs/gate/prune-work.log</string>' &&
        expect lacks "<string>$r/prune" "$scratch/out" &&
        expect has "would check that $r and $r/bin are folders of {builder user}'s, mode 700, not links" &&
        expect has 'would check that /Library/Logs is a folder of root'"'"'s no one else can write' &&
        expect has 'would make /Library/Logs/gate (root:wheel, 0755) when it is missing, and check that it is one too' &&
        expect has 'would create /Library/Logs/gate/prune-work.log (owned by {builder user}, 0600) when it is missing' &&
        expect has "gate-box is not in $r/bin yet: the prune job fails until gate push installs it" &&
        expect has 'would run: launchctl bootout system/dev.gate.prune-work (only when it is loaded), then wait up to 10 s for it to go' &&
        expect has 'would run: launchctl bootstrap system /Library/LaunchDaemons/dev.gate.prune-work.plist' &&
        expect in_order 'would check that /Library/Logs ' 'would make /Library/Logs/gate ' \
            'would create /Library/Logs/gate/prune-work.log ' \
            "would check that $r and $r/bin " "gate-box is not in $r/bin yet" \
            'would write /Library/LaunchDaemons/dev.gate.prune-work.plist ' \
            'would run: launchctl bootout system/dev.gate.prune-work ' \
            'would run: launchctl bootstrap system /Library/LaunchDaemons/dev.gate.prune-work.plist' &&
        expect has 'SKIP System: run with sudo and --system' &&
        expect grep -qF 'SKIP System: run with sudo and --system' "$p/install.log" &&
        expect [ "$(ls -A "$p")" = install.log ] &&
        expect absent "$r" && expect [ -z "$(ls -A "$home")" ]
}

# The dry run's prune-job plist text, for a root holding every character XML escapes, is lint-clean where plutil exists
# (macOS); elsewhere only the escaping is checked.
case_prune_plist_lints() {
    local r="$scratch/r-<&>" plist="$scratch/prune-work.plist"
    run --prefix "$scratch/p-lint" --root "$r" --only System
    expect rc_is 0 && expect has "<string>$scratch/r-&lt;&amp;&gt;/bin/gate-box</string>" || return 1
    sed -n 's|^.*would write /Library/LaunchDaemons/dev\.gate\.prune-work\.plist (root:wheel, 0644): ||p' "$scratch/out" >"$plist"
    expect [ -s "$plist" ] && expect absent "$r" || return 1
    if ! command -v plutil >/dev/null 2>&1; then return 0; fi
    expect plutil -lint "$plist"
}

# The System step reads the builder's home from `dscl . -read /Users/<user> NFSHomeDirectory`; the parse is taken out of
# the installer, which cannot be sourced, and called on sample lines.
case_home_from_dscl() {
    local got
    eval "$(sed -n '/^home_from_dscl() {$/,/^}$/p' "$script")"
    expect [ "$(type -t home_from_dscl)" = function ] || return 1
    if ! got=$(home_from_dscl "$(printf '%s\n' 'RecordName: someone' 'NFSHomeDirectory: /Users/someone')"); then
        echo 'a NFSHomeDirectory line gave no home'
        return 1
    fi
    expect [ "$got" = /Users/someone ] || return 1
    if got=$(home_from_dscl 'RecordName: someone'); then
        echo "a line without NFSHomeDirectory gave the home '$got'"
        return 1
    fi
}

# Before the prune job is pointed at the root, the root and its bin must each be a folder of the builder's, mode 700,
# not a link; the check is taken out of the installer and called on sample `stat -f '%HT|%Su|%Lp'` lines.
case_prune_root_check() {
    local why line
    eval "$(sed -n '/^private_folder_of() {$/,/^}$/p' "$script")"
    expect [ "$(type -t private_folder_of)" = function ] || return 1
    if ! why=$(private_folder_of /r/bin someone 'Directory|someone|700'); then
        echo "a folder of the builder's with mode 700 was refused: $why"
        return 1
    fi
    expect [ -z "$why" ] || return 1
    for line in 'Symbolic Link|someone|700' 'Regular File|someone|700' 'Directory|other|700' 'Directory|someone|755'; do
        if why=$(private_folder_of /r/bin someone "$line"); then
            echo "'$line' was taken"
            return 1
        fi
        case "$why" in
            '/r/bin '*) ;;
            *)
                echo "'$line' gave '$why', which does not name the folder"
                return 1
                ;;
        esac
    done
}

# A root whose bin already holds gate-box gets no line saying the job fails until gate push installs it.
case_system_dry_run_with_gate_box() {
    local r="$scratch/r-gate-box"
    mkdir -p "$r/bin"
    : >"$r/bin/gate-box"
    run --prefix "$scratch/p-gate-box" --root "$r" --only System
    expect rc_is 0 && expect has "would check that $r and $r/bin " && expect lacks 'gate-box is not in' "$scratch/out"
}

# The prune job's log folder, when it is there, must be a folder of root's that no one else can write, and its log a
# plain file of one link owned by the builder; the checks are taken out of the installer and called on sample
# `stat -f '%HT|%Su|%Lp'` and `stat -f '%HT|%Su|%l'` lines.
case_prune_log_checks() {
    local why line
    eval "$(sed -n -e '/^root_log_folder_of() {$/,/^}$/p' -e '/^builder_log_of() {$/,/^}$/p' "$script")"
    expect [ "$(type -t root_log_folder_of)" = function ] && expect [ "$(type -t builder_log_of)" = function ] || return 1
    if ! why=$(root_log_folder_of /l 'Directory|root|755'); then
        echo "a folder of root's with mode 755 was refused: $why"
        return 1
    fi
    if ! why=$(builder_log_of /l/f someone 'Regular File|someone|1'); then
        echo "a plain file of the builder's with one link was refused: $why"
        return 1
    fi
    for line in 'Symbolic Link|root|755' 'Regular File|root|755' 'Directory|other|755' 'Directory|root|775' \
        'Directory|root|757'; do
        if why=$(root_log_folder_of /l "$line"); then
            echo "the folder '$line' was taken"
            return 1
        fi
        expect [ "${why#/l }" != "$why" ] || return 1
    done
    # The same judgement is made of the folder's parent before the folder is made.
    if ! why=$(root_log_folder_of /Library/Logs 'Directory|root|755'); then
        echo "a stock /Library/Logs was refused: $why"
        return 1
    fi
    why=$(root_log_folder_of /Library/Logs 'Directory|admin|775') && echo 'a parent of admin'"'"'s, mode 775, was taken'
    expect [ "${why#/Library/Logs is owned by admin}" != "$why" ] || return 1
    for line in 'Symbolic Link|someone|1' 'Regular File|someone|2' 'Regular File|other|1' 'Directory|someone|1'; do
        if why=$(builder_log_of /l/f someone "$line"); then
            echo "the log '$line' was taken"
            return 1
        fi
        expect [ "${why#/l/f }" != "$why" ] || return 1
    done
}

# The stand-ins case_prune_bootout_in_progress gives the installer's reload: the step's log and fail, no wait, and a
# launchctl, logging each call to $calls, whose bootout exits 36 and whose job is loaded until two looks after it.
# shellcheck disable=SC2329 # declared into the reload's own bash and called there
reload_stubs() {
    log() { printf '%s\n' "$1"; }
    fail() {
        printf 'fail: %s\n' "$1"
        exit 1
    }
    sleep() { :; }
    launchctl() {
        printf '%s\n' "$*" >>"$calls"
        case "$1" in
            print) [ "$(awk '/^bootout /{b=1} b && /^print /{n++} END{print n+0}' "$calls")" -lt 2 ] ;;
            bootout) return 36 ;;
            *) return 0 ;;
        esac
    }
}

# launchctl bootout exits 36 while the old job's process is still ending: the step says so, waits for the job to go
# and bootstraps it, rather than failing on the bootout's status. The reload is taken out of the installer and run
# under the step's set -eE and ERR trap in a bash of its own, as the step's subshell is (one inside this case's `if`
# would ignore both), with launchctl stubbed.
case_prune_bootout_in_progress() {
    local calls="$scratch/launchctl-calls" reload="$scratch/reload.sh"
    : >"$calls"
    {
        sed -n -e '/^sys() {$/,/^}$/p' -e '/^prune_job_reloaded() {$/,/^}$/p' "$script"
        declare -f reload_stubs
        # shellcheck disable=SC2016 # these lines are the reload's own script, which expands them
        printf '%s\n' 'set -eE' "trap 'echo \"trapped: \$BASH_COMMAND\"; exit 9' ERR" 'calls="$1"' reload_stubs \
            system=1 prune_label=dev.gate.prune-work prune_bootout_looks=20 \
            prune_plist_path=/Library/LaunchDaemons/dev.gate.prune-work.plist prune_job_reloaded "echo 'reached the end'"
    } >"$reload"
    expect grep -q '^prune_job_reloaded() {$' "$reload" || return 1
    bash "$reload" "$calls" >"$scratch/out" 2>&1 || true
    expect has 'bootout of dev.gate.prune-work said 36; waiting for it to go' &&
        expect has 'run: launchctl bootstrap system /Library/LaunchDaemons/dev.gate.prune-work.plist' &&
        expect has 'reached the end' && expect lacks 'trapped' "$scratch/out" &&
        expect [ "$(tail -n 1 "$calls")" = 'bootstrap system /Library/LaunchDaemons/dev.gate.prune-work.plist' ]
}

case_system_needs_root() {
    run --prefix "$scratch/p-sysuser" --system --user nobody --only System
    expect rc_is 2 && expect has '--system needs root' && expect absent "$scratch/p-sysuser"
}

# The step's tool check needs the installed tools, so it may FAIL here; what is tested is the block itself.
case_login_path_block() {
    local zp="$home/.zprofile" second="$scratch/p-second"
    local want_path="export PATH=\"$second/bin:$second/dotnet:$second/node/bin:$scratch/cargo/bin:/opt/homebrew/bin:\$PATH\""
    printf '%s\n' 'export KEEP_ME=1' >"$zp"
    run --prefix "$scratch/p-first" --only 'Login PATH'
    expect [ "$rc" -le 1 ] && expect grep -qF "$scratch/p-first/bin" "$zp" || return 1
    run --prefix "$second" --only 'Login PATH'
    expect [ "$rc" -le 1 ] &&
        expect [ "$(line_count '# >>> gate builder >>>' "$zp")" = 1 ] &&
        expect [ "$(line_count '# <<< gate builder <<<' "$zp")" = 1 ] &&
        expect lacks "$scratch/p-first" "$zp" &&
        expect [ "$(line_count "$want_path" "$zp")" = 1 ] &&
        expect [ "$(line_count "export DOTNET_ROOT=\"$second/dotnet\"" "$zp")" = 1 ] &&
        expect [ "$(line_count 'export DOTNET_CLI_TELEMETRY_OPTOUT=1' "$zp")" = 1 ] &&
        expect [ "$(line_count "export GATE_GODOT_ROOT=\"$second/godot\"" "$zp")" = 1 ] &&
        expect [ "$(line_count 'export HOMEBREW_NO_AUTO_UPDATE=1' "$zp")" = 1 ] &&
        expect [ "$(line_count 'export HOMEBREW_NO_ANALYTICS=1' "$zp")" = 1 ] &&
        expect [ "$(head -n 1 "$zp")" = 'export KEEP_ME=1' ]
}

# The package caches the box's restores make land in the builder's home are readable by every account the home's mode
# lets in, and every standard macOS account is in that group; the step makes the home 0700, and says so once.
case_private_home() {
    chmod 750 "$home"
    run --only 'Private home'
    expect rc_is 0 || return 1
    # Git Bash cannot hold a folder's mode, so the mode and the line it makes are checked on the Unix boxes only.
    case "$(uname -s)" in
        Darwin | Linux) ;;
        *) return 0 ;;
    esac
    expect [ "$(mode_of "$home")" = 700 ] || return 1
    expect has 'had mode 750; set to 700' || return 1
    run --only 'Private home'
    expect rc_is 0 && expect lacks 'set to 700' "$scratch/out"
}

# A home that is a link is refused before anything is changed; a trailing slash would make -L follow it.
case_private_home_refuses_a_link() {
    # Git Bash writes ln -s as a copy, not a link, so the refusal is checked on the Unix boxes only.
    case "$(uname -s)" in
        Darwin | Linux) ;;
        *) return 0 ;;
    esac
    mkdir -p "$scratch/real-home"
    ln -s "$scratch/real-home" "$scratch/link-home"
    set +e
    env HOME="$scratch/link-home" CARGO_HOME="$scratch/cargo" INSTALL_MACOS_TEST_UID=501 \
        bash "$script" --only 'Private home' >"$scratch/out" 2>&1
    rc=$?
    set -e
    expect rc_is 1 && expect has 'is a symlink; point HOME at the builder'"'"'s own folder'
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

case_list_homebrew() {
    run --list
    expect rc_is 0 &&
        expect [ "$(sed -n '15p' "$scratch/out")" = 'Homebrew' ] &&
        expect [ "$(sed -n '16p' "$scratch/out")" = 'FFmpeg' ] &&
        expect [ "$(sed -n '17p' "$scratch/out")" = 'Login PATH' ]
}

# The dry run comes before the steps may change the machine: it prints both steps' commands and logs SKIP for both.
case_admin_steps_dry_run() {
    local p="$scratch/p-admin-dry" before after
    before=$(ls -A /opt 2>/dev/null || true)
    run --prefix "$p" --only Homebrew,FFmpeg
    after=$(ls -A /opt 2>/dev/null || true)
    expect rc_is 0 &&
        expect has 'would run: sudo -v (asks for the admin password)' &&
        expect has 'would download https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh' &&
        expect has 'run it with NONINTERACTIVE=1 /bin/bash' &&
        expect has 'would run: /opt/homebrew/bin/brew analytics off' &&
        expect has 'would run: HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 /opt/homebrew/bin/brew install ffmpeg' &&
        expect has 'would run: /opt/homebrew/bin/ffmpeg -version' &&
        expect has 'SKIP Homebrew: run with --admin as an admin account' &&
        expect has 'SKIP FFmpeg: run with --admin as an admin account' &&
        expect grep -qF 'SKIP Homebrew: run with --admin as an admin account' "$p/install.log" &&
        expect grep -qF 'SKIP FFmpeg: run with --admin as an admin account' "$p/install.log" &&
        expect [ "$before" = "$after" ]
}

# Every --admin case passes --user nobody: after its --admin checks and before anything is written, the script refuses
# --user without --system, so a refusal that regressed stops there instead of reaching sudo, the installer or brew.
case_admin_refused_as_root() {
    run_groups_as 0 'staff admin' --admin --user nobody --prefix "$scratch/p-admin-root" --only Homebrew,FFmpeg
    expect rc_is 2 && expect has '--admin runs as an admin account, not root: run it without sudo' &&
        expect absent "$scratch/p-admin-root"
}

case_admin_refused_non_admin() {
    run_admin_as 'staff' --admin --user nobody --prefix "$scratch/p-admin-grp" --only Homebrew,FFmpeg
    expect rc_is 2 && expect has '--admin needs an account in the admin group' &&
        expect absent "$scratch/p-admin-grp"
}

# The group is matched as a whole word: a name that merely holds or ends in 'admin' is not the group.
case_admin_group_words() {
    local p="$scratch/p-admin-words"
    run_admin_as 'staff _lpadmin' --admin --user nobody --prefix "$p" --only Homebrew,FFmpeg
    expect rc_is 2 && expect has '--admin needs an account in the admin group' || return 1
    run_admin_as 'staff admins' --admin --user nobody --prefix "$p" --only Homebrew,FFmpeg
    expect rc_is 2 && expect has '--admin needs an account in the admin group' && expect absent "$p"
}

case_admin_accepted() {
    local p="$scratch/p-admin-ok"
    run_admin_as 'staff admin' --admin --user nobody --prefix "$p" --only homebrew,FFMPEG
    expect rc_is 2 && expect has '--user only goes with --system' && expect lacks '--admin' "$scratch/out" &&
        expect absent "$p"
}

case_admin_refused_with_system() {
    run_admin_as 'staff admin' --admin --system --user nobody --prefix "$scratch/p-admin-sys" --only Homebrew,FFmpeg
    expect rc_is 2 && expect has '--admin and --system are separate runs' &&
        expect absent "$scratch/p-admin-sys"
}

case_admin_refused_other_step() {
    run_admin_as 'staff admin' --admin --user nobody --prefix "$scratch/p-admin-rust" --only Rust
    expect rc_is 2 && expect has '--admin runs only the Homebrew and FFmpeg steps: pass --only Homebrew,FFmpeg' &&
        expect absent "$scratch/p-admin-rust"
}

case_admin_refused_without_only() {
    run_admin_as 'staff admin' --admin --user nobody --prefix "$scratch/p-admin-all"
    expect rc_is 2 && expect has '--admin runs only the Homebrew and FFmpeg steps: pass --only Homebrew,FFmpeg' &&
        expect absent "$scratch/p-admin-all"
}

# The root rule exempts Homebrew and FFmpeg: as root they are reachable only beside --system, and without --admin both
# dry-run. The run stops at --user naming no account, so no step changes anything.
case_root_exempts_admin_steps() {
    run_as 0 --system --user 'no-such-builder-user' --only 'System,Homebrew,FFmpeg'
    expect rc_is 2 && expect lacks 'refuses to run as root' "$scratch/out" &&
        expect has "--user 'no-such-builder-user' is not an account on this machine"
}

check '--list prints every step and exits 0' case_list
check '--list names the Homebrew and FFmpeg steps where FFmpeg was' case_list_homebrew
check 'the Homebrew and FFmpeg steps print their commands, log SKIP and touch nothing without --admin' case_admin_steps_dry_run
check '--admin as root exits 2' case_admin_refused_as_root
check '--admin outside the admin group exits 2' case_admin_refused_non_admin
check 'the admin group is matched as a whole word' case_admin_group_words
check 'a valid --admin run passes every --admin check' case_admin_accepted
check '--admin with --system exits 2' case_admin_refused_with_system
check '--admin with a --only naming another step exits 2' case_admin_refused_other_step
check '--admin without --only exits 2' case_admin_refused_without_only
check 'as root, Homebrew and FFmpeg are not refused as toolchain steps' case_root_exempts_admin_steps
check '--only with a prefix matching no step exits 2 and creates nothing' case_only_bogus
check 'an unknown flag exits 2' case_unknown_flag
check 'a relative --prefix exits 2' case_relative_prefix
check '--only System without --system prints the commands, logs SKIP and changes nothing' case_system_dry_run
if command -v plutil >/dev/null 2>&1; then
    check 'the prune job plist escapes its root and passes plutil -lint' case_prune_plist_lints
else
    check 'the prune job plist escapes its root (plutil -lint SKIP: no plutil here)' case_prune_plist_lints
fi
check 'the builder home is read from a dscl NFSHomeDirectory line' case_home_from_dscl
check 'the prune job takes only a root and bin of the builder'"'"'s, mode 700, not links' case_prune_root_check
check 'the System dry run says when gate-box is not in the root'"'"'s bin, and only then' case_system_dry_run_with_gate_box
check 'the prune log folder must be root'"'"'s and writable by no one else, its log the builder'"'"'s alone' case_prune_log_checks
check 'a bootout still in progress is waited out and the prune job bootstrapped' case_prune_bootout_in_progress
check '--system without root exits 2' case_system_needs_root
check 'the Login PATH block is written once and replaced on rerun' case_login_path_block
check 'the Private home step makes the home mode 0700 and says so only when it changes it' case_private_home
check 'a home that is a link is refused' case_private_home_refuses_a_link
check 'a toolchain step as root is refused' case_root_refused
check 'a toolchain step beside System under --system as root is refused' case_root_step_beside_system_refused

printf '%s passed, %s failed\n' "$passes" "$fails"
[ "$fails" = 0 ]
