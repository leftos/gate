#!/usr/bin/env bash
# Tests prep-macos.sh and its step scripts without changing the machine: the manifest against the step scripts, the
# checklist menu, the confirmation question and the log on a copy whose steps are stand-ins, the argument refusals, the
# dry run of the defaults, and each step's change path with the macOS tools (sudo included) stubbed; then the kcpassword
# encoding and a scan for private values. Runs on macOS and in Git Bash, as a normal user.
# Usage: bash tools/builder/test-prep-macos.sh
set -euo pipefail

if [ "$(id -u)" = 0 ]; then
    echo 'run the tests as a normal user; they need no root' >&2
    exit 2
fi

here=$(cd "$(dirname "$0")" && pwd)
master="$here/prep-macos.sh"
steps_dir="$here/prep/macos"
manifest="$steps_dir/steps.tsv"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/test-prep-macos.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
home="$scratch/home"
stubs="$scratch/stubs"
calls="$scratch/calls"
users="$scratch/users"
user='gate-prep-test-user'
key="$scratch/pc.pub"
mkdir -p "$home" "$stubs" "$users"
printf '%s\n' 'test-key-type AAAAtestkey pc' >"$key"

# Each stubbed tool records its words in $calls and answers from the STUB_* variables. An account exists when $users
# holds a file of its name; sysadminctl -addUser makes one. Every other query says "not yet", so steps take their change
# path. sudo -u <user> -H runs the rest of its words with HOME at the builder's home.
make_stub() {
    printf '%s\n' '#!/usr/bin/env bash' "printf '%s\n' \"$1 \$*\" >>\"\$GATE_STUB_CALLS\"" "$2" >"$stubs/$1"
    chmod +x "$stubs/$1"
}
# shellcheck disable=SC2016 # each body is the stub's own script, which expands it
make_stubs() {
    make_stub sysadminctl '[ "$1" != -addUser ] || [ -n "${STUB_ADDUSER_NOOP:-}" ] || : >"$STUB_USERS/$2"'
    make_stub id '[ -e "$STUB_USERS/$2" ] || exit 1; case "$1" in -u) echo 502 ;; -gn) echo staff ;; esac'
    make_stub systemsetup 'case "$1" in
        -getremotelogin) if [ -e "$GATE_STUB_CALLS.on" ]; then echo "Remote Login: On"; else echo "Remote Login: Off"; fi ;;
        -setremotelogin) [ -n "${STUB_SET_NOOP:-}" ] || : >"$GATE_STUB_CALLS.on" ;;
    esac'
    make_stub dseditgroup 'case "$2" in checkmember) exit 1 ;; esac'
    make_stub fdesetup 'case "$1" in status) echo "FileVault is ${STUB_FILEVAULT:-Off}." ;; esac'
    make_stub defaults 'case "$1" in read) [ -n "${STUB_AUTOLOGIN_USER:-}" ] || exit 1; echo "$STUB_AUTOLOGIN_USER" ;; esac'
    make_stub sshd 'case "$1" in
        -T)
            echo "port 22"
            for w in $(sed -n "s/^AllowUsers //p" "$GATE_PREP_ROOT/etc/ssh/sshd_config.d/100-gate-prep.conf" 2>/dev/null) \
                ${STUB_SSHD_OTHERS:-}; do echo "allowusers $w"; done ;;
        -t) exit "${STUB_SSHD_RC:-0}" ;;
    esac'
    make_stub xcode-select 'echo "${STUB_XCODE_DIR:-/Applications/Xcode.app/Contents/Developer}"'
    make_stub xcodebuild 'case "$2" in check) exit "${STUB_LICENSE_RC:-1}" ;; esac'
    make_stub dscl 'case "$2" in -authonly) exit "${STUB_AUTH_RC:-0}" ;; *) echo "NFSHomeDirectory: $STUB_HOME" ;; esac'
    make_stub ssh-keygen 'exit "${STUB_KEYGEN_RC:-0}"'
    make_stub chown 'exit 0'
    make_stub sudo '[ "$1" = -u ] && [ "$3" = -H ] || exit 9; shift 3; HOME="$STUB_HOME" exec "$@"'
}
make_stubs

# Runs a script with the stubs first on PATH, the scratch root under every system path, the uid in $uid (root unless a
# case says otherwise), stdin from $input and SUDO_USER from $sudo_user; leaves its exit code in $rc and its stdout and
# stderr in $scratch/out.
rc=0
uid=0
input=/dev/null
tty=''
sudo_user=''
run_script() {
    local script="$1"
    shift
    set +e
    env HOME="$home" PATH="$stubs:$PATH" GATE_PREP_ROOT="$scratch/root" GATE_PREP_TEST_UID="$uid" \
        GATE_PREP_TEST_TTY="$tty" GATE_STUB_CALLS="$calls" STUB_HOME="$home/$user" STUB_USERS="$users" \
        SUDO_USER="$sudo_user" bash "$script" "$@" <"$input" >"$scratch/out" 2>&1
    rc=$?
    set -e
}
fresh() {
    rm -rf "${scratch:?}/root" "${home:?}/${user:?}" "${users:?}" "${calls:?}" "$calls.on"
    mkdir -p "$scratch/root/etc/ssh/sshd_config.d" "$scratch/root/Library/Preferences" "$users"
    : >"$calls"
}
feed() { printf '%b' "$1" >"$scratch/input"; }

# A copy of the master and manifest whose step scripts are stand-ins: each records its id and words in $ran, prints a
# line, and ends 3 when it is $STUB_FAIL_STEP or prints a SKIP line when it is $STUB_SKIP_STEP.
copy="$scratch/copy"
ran="$scratch/ran"
mkdir -p "$copy/prep/macos"
cp "$master" "$copy/prep-macos.sh"
cp "$manifest" "$copy/prep/macos/steps.tsv"
for f in "$steps_dir"/*.sh; do
    id=$(basename "$f" .sh)
    printf '%s\n' '#!/usr/bin/env bash' "printf '%s\n' \"$id \$*\" >>\"$ran\"" "echo 'step output, kept out of the log'" \
        "[ \"\${STUB_FAIL_STEP:-}\" != $id ] || exit 3" "[ \"\${STUB_SKIP_STEP:-}\" != $id ] || echo 'SKIP stubbed reason'" \
        >"$copy/prep/macos/$id.sh"
done
run_copy() {
    rm -rf "$scratch/root"
    : >"$ran"
    run_script "$copy/prep-macos.sh" "$@"
}
menu() {
    feed "$1"
    input="$scratch/input" tty=1 run_copy --user "$user" --public-key-file "$key" "${@:2}"
}
state_dir() { printf '%s' "$scratch/root/Library/Application Support/gate-prep"; }

expect() {
    if ! "$@"; then
        echo "expected: $*"
        return 1
    fi
}
has() { grep -qF -- "$1" "$scratch/out"; }
lacks() { ! grep -qF -- "$1" "$2"; }
rc_is() { [ "$rc" = "$1" ]; }
absent() { [ ! -e "$1" ]; }
mode_of() {
    if [ "$(uname -s)" = Darwin ]; then stat -f '%Lp' "$1"; else stat -c '%a' "$1"; fi
}
unix_box() {
    case "$(uname -s)" in
        Darwin | Linux) return 0 ;;
        *) return 1 ;;
    esac
}
# The text of the last menu drawn. Piped input echoes no newline, so a redraw starts on the prompt's line.
last_menu() { awk '/^(> )?gate builder prep/{buf=""} {buf=buf $0 "\n"} END{printf "%s", buf}' "$scratch/out"; }
menu_has() { last_menu | grep -qF -- "$1"; }
draws() { grep -cE '^(> )?gate builder prep' "$scratch/out" || true; }

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

defaults_ids='builder-account
remote-login
authorized-key
ssh-allow-users
xcode-license
autologin'

case_manifest() {
    local ids scripts n
    expect [ "$(head -n 1 "$manifest")" = "$(printf 'id\tname\tdefault\tdestructive\tinteractive\tcaveat\tdetail')" ] || return 1
    n=$(($(awk -F '\t' 'NR > 1 && NF != 7' "$manifest" | wc -l)))
    expect [ "$n" -eq 0 ] || return 1
    expect [ -z "$(awk -F '\t' 'NR > 1 && ($3 !~ /^[01]$/ || $4 !~ /^[01]$/ || $5 !~ /^[01]$/)' "$manifest")" ] || return 1
    expect [ -z "$(awk -F '\t' 'NR > 1 && $4 == 1 && $3 == 1' "$manifest")" ] || return 1
    ids=$(awk -F '\t' 'NR > 1 {print $1}' "$manifest" | sort)
    expect [ "$(printf '%s\n' "$ids" | uniq -d)" = '' ] || return 1
    scripts=$(for f in "$steps_dir"/*.sh; do basename "$f" .sh; done | sort)
    expect [ "$ids" = "$scripts" ] || return 1
    expect [ "$(awk -F '\t' 'NR > 1 && $3 == 1 {print $1}' "$manifest")" = "$defaults_ids" ]
}

case_list() {
    run_script "$master" --list
    expect rc_is 0 && expect has 'builder-account' && expect has 'FileVault off' &&
        expect has 'after a reboot the Mac waits at the unlock screen' && expect [ "$(grep -c '^ *[0-9]' "$scratch/out")" = 8 ]
}

case_menu_toggles() {
    menu '4 6\nq\n'
    expect rc_is 0 && expect [ "$(draws)" = 2 ] && expect menu_has '4  [x] Tailscale (optional)' &&
        expect menu_has '6  [ ] Xcode license accepted' && expect [ ! -s "$ran" ]
}

case_menu_all_and_defaults() {
    menu 'a\nq\n'
    expect rc_is 0 && expect menu_has '7  [x] FileVault off' && expect menu_has '4  [x] Tailscale (optional)' || return 1
    menu 'a\nd\nq\n'
    expect rc_is 0 && expect [ "$(draws)" = 3 ] && expect menu_has '7  [ ] FileVault off' &&
        expect menu_has '1  [x] Builder account'
}

case_menu_bad_token() {
    menu 'zz 3\nq\n'
    expect rc_is 0 && expect has "prep: 'zz' is not a step number, a, d or q" && expect [ "$(draws)" = 2 ] &&
        expect menu_has '3  [x] The PC' && expect [ ! -s "$ran" ]
}

case_menu_quit() {
    menu 'q\n'
    expect rc_is 0 && expect [ ! -s "$ran" ] && expect absent "$(state_dir)"
}

case_menu_enter_runs_defaults() {
    menu '\n'
    expect rc_is 0 && expect [ "$(cut -d ' ' -f 1 "$ran")" = "$defaults_ids" ] &&
        expect grep -qxF "authorized-key --user $user --public-key-file $key" "$ran" &&
        expect grep -qxF "remote-login --user $user" "$ran" &&
        expect [ "$(cut -f 1 "$(state_dir)/state")" = "$defaults_ids" ] &&
        expect grep -qF 'BEGIN builder-account' "$(state_dir)/prep.log" &&
        expect grep -qF 'OK autologin' "$(state_dir)/prep.log" &&
        expect grep -qF 'COMPLETE failed=0' "$(state_dir)/prep.log"
}

case_menu_done_from_state() {
    rm -rf "$scratch/root"
    mkdir -p "$(state_dir)"
    printf 'remote-login\t2026-01-01T00:00:00\n' >"$(state_dir)/state"
    : >"$ran"
    feed '\n'
    input="$scratch/input" tty=1 run_script "$copy/prep-macos.sh" --user "$user" --public-key-file "$key"
    expect rc_is 0 && expect menu_has '2  [done] Remote Login (ssh)' && expect lacks 'remote-login' "$ran" &&
        expect grep -qx "builder-account --user $user" "$ran"
}

case_log_fail_and_skip() {
    STUB_FAIL_STEP=xcode-license STUB_SKIP_STEP=autologin run_copy --user "$user" --public-key-file "$key" --defaults
    local log
    log="$(state_dir)/prep.log"
    expect rc_is 1 && expect grep -qE ' FAIL xcode-license: exit 3$' "$log" &&
        expect grep -qE ' SKIP autologin: stubbed reason$' "$log" && expect grep -qF 'COMPLETE failed=1' "$log" &&
        expect lacks 'step output' "$log" && expect has 'step output, kept out of the log' &&
        expect lacks xcode-license "$(state_dir)/state" && expect lacks autologin "$(state_dir)/state" &&
        expect grep -q '^remote-login' "$(state_dir)/state"
}

case_confirm() {
    feed 'y\n'
    input="$scratch/input" tty=1 run_copy --user "$user" --steps xcode-license,remote-login
    expect rc_is 0 && expect has 'prep: run remote-login xcode-license? [y/N]' && expect grep -q '^xcode-license' "$ran" || return 1
    feed 'n\n'
    input="$scratch/input" tty=1 run_copy --user "$user" --steps xcode-license
    expect rc_is 1 && expect has 'prep: not confirmed; nothing ran' && expect [ ! -s "$ran" ] || return 1
    input="$scratch/input" tty=1 run_copy --user "$user" --steps xcode-license --dry-run
    expect rc_is 1 && expect has 'prep: run xcode-license? [y/N]' && expect [ ! -s "$ran" ] || return 1
    input="$scratch/input" tty=1 run_copy --user "$user" --steps xcode-license --yes
    expect rc_is 0 && expect lacks '[y/N]' "$scratch/out" && expect grep -q '^xcode-license' "$ran" || return 1
    run_copy --user "$user" --steps xcode-license
    expect rc_is 0 && expect lacks '[y/N]' "$scratch/out" && expect grep -q '^xcode-license' "$ran"
}

case_nothing_to_run() {
    local id
    rm -rf "$scratch/root"
    mkdir -p "$(state_dir)"
    for id in $defaults_ids; do printf '%s\t2026-01-02T03:04:05\n' "$id"; done >"$(state_dir)/state"
    : >"$ran"
    feed 'y\n'
    input="$scratch/input" tty=1 run_script "$copy/prep-macos.sh" --user "$user" --defaults
    expect rc_is 0 && expect has 'prep: nothing to run' && expect lacks '[y/N]' "$scratch/out" && expect [ ! -s "$ran" ] &&
        expect has 'SKIP builder-account: done at 2026-01-02T03:04:05' &&
        expect grep -qF 'SKIP autologin: done at 2026-01-02T03:04:05' "$(state_dir)/prep.log"
}

case_unknown_step() {
    run_copy --user "$user" --steps 'xcode-license,bogus' --yes
    expect rc_is 2 && expect has "prep: --steps 'bogus' is not a step" && expect [ ! -s "$ran" ] && expect absent "$scratch/root"
}

case_defaults_with_steps() {
    run_copy --user "$user" --steps xcode-license --defaults --yes
    expect rc_is 2 && expect has '--defaults and --steps are separate runs' && expect [ ! -s "$ran" ]
}

case_key_needed() {
    run_copy --user "$user" --steps authorized-key --yes
    expect rc_is 2 && expect has 'authorized-key needs --public-key-file <path>' && expect [ ! -s "$ran" ] &&
        expect absent "$scratch/root"
}

case_user_names() {
    local bad s
    for bad in '../' 'a b' '.' '..' '-x' 'x/y'; do
        run_copy --user "$bad" --steps xcode-license --yes
        expect rc_is 2 && expect has 'is not an account name' && expect [ ! -s "$ran" ] || return 1
        for s in "$steps_dir"/*.sh; do
            if [ "$(basename "$s")" = authorized-key.sh ]; then
                run_script "$s" --user "$bad" --public-key-file "$key"
            else
                run_script "$s" --user "$bad"
            fi
            if ! { expect rc_is 2 && expect has 'is not an account name'; }; then
                echo "in $s"
                return 1
            fi
        done
    done
}

case_no_tty_menu() {
    run_copy --user "$user"
    expect rc_is 2 && expect has 'prep: no console for the menu; run it at the console, over ssh -t, or with --defaults or --steps' &&
        expect [ ! -s "$ran" ]
}

case_not_root() {
    uid=501 run_copy --user "$user" --steps xcode-license --yes
    expect rc_is 1 && expect has 'prep: run it with sudo' && expect [ ! -s "$ran" ]
}

tree_of() { (cd "$1" && find . | LC_ALL=C sort && find . -type f -exec cat {} +); }

case_dry_run_defaults() {
    local before word
    fresh
    before=$(tree_of "$scratch/root")
    uid=501 run_script "$master" --user "$user" --public-key-file "$key" --dry-run --defaults
    expect rc_is 0 && expect has 'would run: sysadminctl -addUser' && expect has 'would run: systemsetup -setremotelogin on' &&
        expect has 'would append' && expect has 'would write' && expect has 'would run: xcodebuild -license accept' &&
        expect has 'COMPLETE failed=0' && expect [ "$(tree_of "$scratch/root")" = "$before" ] &&
        expect absent "$home/$user" || return 1
    for word in -addUser -setremotelogin '-o edit' write '-license accept' install services ' up' disable 'sudo '; do
        expect lacks "$word" "$calls" || return 1
    done
}

case_builder_account() {
    fresh
    : >"$users/$user"
    run_script "$steps_dir/builder-account.sh" --user "$user"
    expect rc_is 0 && expect has 'already done' && expect lacks -addUser "$calls" || return 1
    fresh
    run_script "$steps_dir/builder-account.sh" --user "$user"
    expect rc_is 2 && expect has 'run it at the console or over ssh -t' && expect lacks -addUser "$calls" || return 1
    tty=1 run_script "$steps_dir/builder-account.sh" --user "$user"
    expect rc_is 0 && expect grep -qxF "sysadminctl -addUser $user -password -" "$calls" && expect has "created" || return 1
    fresh
    STUB_ADDUSER_NOOP=1 tty=1 run_script "$steps_dir/builder-account.sh" --user "$user"
    expect rc_is 1 && expect has "still no account $user"
}

case_remote_login() {
    fresh
    run_script "$steps_dir/remote-login.sh" --user "$user"
    expect rc_is 0 && expect grep -qxF 'systemsetup -setremotelogin on' "$calls" &&
        expect grep -qxF "dseditgroup -o edit -a $user -t user com.apple.access_ssh" "$calls" || return 1
    fresh
    STUB_SET_NOOP=1 run_script "$steps_dir/remote-login.sh" --user "$user"
    expect rc_is 1 && expect has 'Remote Login is still off' && expect has 'Full Disk Access'
}

case_authorized_key() {
    local keys="$home/$user/.ssh/authorized_keys"
    fresh
    mkdir -p "$home/$user"
    printf 'test-key-type AAAAtestkey pc\r\n' >"$scratch/crlf.pub"
    run_script "$steps_dir/authorized-key.sh" --user "$user" --public-key-file "$scratch/crlf.pub"
    expect rc_is 0 && expect [ "$(cat "$keys")" = 'test-key-type AAAAtestkey pc' ] &&
        expect grep -qF "sudo -u $user -H /bin/sh -c" "$calls" && expect lacks chown "$calls" || return 1
    if unix_box; then expect [ "$(mode_of "$keys")" = 600 ] && expect [ "$(mode_of "$home/$user/.ssh")" = 700 ] || return 1; fi
    run_script "$steps_dir/authorized-key.sh" --user "$user" --public-key-file "$key"
    expect rc_is 0 && expect has 'already done' && expect [ "$(grep -c AAAAtestkey "$keys")" = 1 ] || return 1
    printf '%s\n' '-----BEGIN TEST BLOCK-----' 'AAAA' >"$scratch/private"
    run_script "$steps_dir/authorized-key.sh" --user "$user" --public-key-file "$scratch/private"
    expect rc_is 2 && expect has 'that is a private key; pass the .pub file' || return 1
    STUB_KEYGEN_RC=1 run_script "$steps_dir/authorized-key.sh" --user "$user" --public-key-file "$key"
    expect rc_is 2 && expect has 'ssh-keygen'
}

case_allow_users() {
    local d="$scratch/root/etc/ssh/sshd_config.d"
    fresh
    STUB_SSHD_RC=1 run_script "$steps_dir/ssh-allow-users.sh" --user "$user"
    expect rc_is 1 && expect has 'sshd -t' && expect absent "$d/100-gate-prep.conf" || return 1
    sudo_user=the-admin run_script "$steps_dir/ssh-allow-users.sh" --user "$user"
    expect rc_is 0 && expect grep -qxF "AllowUsers $user the-admin" "$d/100-gate-prep.conf" || return 1
    sudo_user=the-admin run_script "$steps_dir/ssh-allow-users.sh" --user "$user"
    expect rc_is 0 && expect has 'already done' || return 1
    fresh
    STUB_SSHD_OTHERS='someone@here other' run_script "$steps_dir/ssh-allow-users.sh" --user "$user"
    expect rc_is 1 && expect has 'someone@here' && expect has 'merge' && expect absent "$d/100-gate-prep.conf"
}

case_xcode_license() {
    fresh
    run_script "$steps_dir/xcode-license.sh" --user "$user"
    expect rc_is 0 && expect grep -qxF 'xcodebuild -license accept' "$calls" || return 1
    fresh
    STUB_LICENSE_RC=0 run_script "$steps_dir/xcode-license.sh" --user "$user"
    expect rc_is 0 && expect has 'already done' && expect lacks 'accept' "$calls" || return 1
    STUB_XCODE_DIR=/Library/Developer/CommandLineTools run_script "$steps_dir/xcode-license.sh" --user "$user"
    expect rc_is 0 && expect has 'SKIP no Xcode'
}

# Puts a brew at Homebrew's Apple-silicon place under the scratch root; its install drops a tailscale beside it whose
# status says "not up". Both record their own path and words in $calls.
# shellcheck disable=SC2016 # each line is the stub's own script, which expands it
place_brew() {
    local b="$scratch/root/opt/homebrew/bin"
    mkdir -p "$b"
    printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\n" "$0 $*" >>"$GATE_STUB_CALLS"' 'case "$1" in status) exit 1 ;; esac' \
        >"$scratch/tailscale-stub"
    printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\n" "$0 $*" >>"$GATE_STUB_CALLS"' \
        "[ \"\$1\" != install ] || { cp '$scratch/tailscale-stub' \"\${0%/brew}/tailscale\" && chmod +x \"\${0%/brew}/tailscale\"; }" \
        >"$b/brew"
    chmod +x "$b/brew"
}
# Whether $calls holds a line equal to each text given, in the order given.
calls_in_order() {
    local at=0 now text
    for text in "$@"; do
        now=$(grep -nxF -- "$text" "$calls" | head -n 1 | cut -d : -f 1)
        if [ -z "$now" ] || [ "$now" -le "$at" ]; then
            echo "'$text' is not a call after line $at"
            return 1
        fi
        at=$now
    done
}

case_tailscale() {
    local b="$scratch/root/opt/homebrew/bin"
    fresh
    place_brew
    sudo_user=the-admin run_script "$steps_dir/tailscale.sh" --user "$user"
    expect rc_is 0 && expect calls_in_order "sudo -u the-admin -H $b/brew install tailscale" \
        "$b/brew services start tailscale" "$b/tailscale up" || return 1
    fresh
    place_brew
    run_script "$steps_dir/tailscale.sh" --user "$user"
    expect rc_is 1 && expect has 'Homebrew refuses root' && expect lacks install "$calls"
}

case_filevault_off() {
    fresh
    STUB_FILEVAULT=On tty=1 run_script "$steps_dir/filevault-off.sh" --user "$user"
    expect rc_is 0 && expect grep -qxF 'fdesetup disable' "$calls" || return 1
    fresh
    STUB_FILEVAULT=On run_script "$steps_dir/filevault-off.sh" --user "$user"
    expect rc_is 2 && expect has 'run it at the console or over ssh -t' && expect lacks disable "$calls"
}

case_autologin_filevault_on() {
    fresh
    STUB_FILEVAULT=On run_script "$steps_dir/autologin.sh" --user "$user"
    expect rc_is 0 && expect has 'SKIP FileVault is on; turn it off (step filevault-off) and run this step again' &&
        expect absent "$scratch/root/etc/kcpassword"
}

case_autologin_writes() {
    local kc="$scratch/root/etc/kcpassword" prefs="$scratch/root/Library/Preferences/com.apple.loginwindow"
    fresh
    : >"$users/$user"
    feed 'first-pw\nfirst-pw\n'
    STUB_AUTH_RC=1 input="$scratch/input" tty=1 run_script "$steps_dir/autologin.sh" --user "$user"
    expect rc_is 1 && expect has "the password does not log in as $user; nothing written" && expect absent "$kc" &&
        expect lacks 'defaults write' "$calls" || return 1
    input="$scratch/input" tty=1 run_script "$steps_dir/autologin.sh" --user "$user"
    expect rc_is 0 && expect [ "$(od -An -v -tx1 "$kc" | tr -s ' \n' '  ' | sed 's/ *$//')" = "$(expected_kcpassword first-pw)" ] &&
        expect grep -qxF "defaults write $prefs autoLoginUser -string $user" "$calls" || return 1
    if unix_box; then expect [ "$(mode_of "$kc")" = 600 ] || return 1; fi
    feed 'second-pw\nsecond-pw\n'
    STUB_AUTOLOGIN_USER="$user" input="$scratch/input" tty=1 run_script "$steps_dir/autologin.sh" --user "$user"
    expect rc_is 0 && expect has 'already done' || return 1
    STUB_AUTOLOGIN_USER="$user" input="$scratch/input" tty=1 run_script "$steps_dir/autologin.sh" --user "$user" --force
    expect rc_is 0 && expect [ "$(od -An -v -tx1 "$kc" | tr -s ' \n' '  ' | sed 's/ *$//')" = "$(expected_kcpassword second-pw)" ]
}

# The expected kcpassword bytes, from the rule: the password's bytes followed by zeros up to the next multiple of 12
# (a full 12 more when the length already is one), each XORed with Apple's 11-byte key taken in turn.
expected_kcpassword() {
    local i n pad
    local -a k=(0x7D 0x89 0x52 0x23 0xD2 0xBC 0xDD 0xEA 0xA3 0xB9 0x1F) b
    read -ra b <<<"$(printf '%s' "$1" | od -An -v -tu1 | tr -s ' \n' '  ')"
    n=${#b[@]}
    pad=$((12 - n % 12))
    for ((i = 0; i < n + pad; i++)); do
        printf ' %02x' $((${b[i]:-0} ^ k[i % 11]))
    done
}

case_kcpassword_bytes() {
    local pw got
    eval "$(sed -n '/^kcpassword_bytes() {$/,/^}$/p' "$steps_dir/autologin.sh")"
    expect [ "$(type -t kcpassword_bytes)" = function ] || return 1
    for pw in abc 'twelve-chars' 'thirteen-char' 'pässwörd-ü'; do
        got=$(kcpassword_bytes "$pw" | od -An -v -tx1 | tr -s ' \n' '  ' | sed 's/ *$//')
        expect [ "$got" = "$(expected_kcpassword "$pw")" ] || return 1
    done
    expect [ "$(expected_kcpassword abc)" = ' 1c eb 31 23 d2 bc dd ea a3 b9 1f 7d' ] &&
        expect [ "$(($(expected_kcpassword 'pässwörd-ü' | wc -w)))" = 24 ]
}

case_secret_scan() {
    local files=("$master" "$manifest" "$steps_dir"/*.sh "$0")
    expect lacks_any '[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}' "${files[@]}" &&
        expect lacks_any 'ssh-(rsa|ed25519|dss|ecdsa)' "${files[@]}" &&
        expect lacks_any '/Users/[A-Za-z]|/home/[A-Za-z]|C:\\Users|X:\\dev|[.]ts[.]net' "${files[@]}"
}
lacks_any() {
    local pattern="$1"
    shift
    ! grep -nE -- "$pattern" "$@"
}

check 'the manifest parses, has a script per entry and an entry per script, and unique ids' case_manifest
check '--list prints every step with its caveat' case_list
check 'the menu toggles steps by number' case_menu_toggles
check 'the menu ticks all with a and resets with d' case_menu_all_and_defaults
check 'a bad menu token is named and the menu redrawn' case_menu_bad_token
check 'q quits the menu with exit 0 and runs nothing' case_menu_quit
check 'Enter runs exactly the defaults and records them' case_menu_enter_runs_defaults
check 'a step in the state file shows [done] and is not run again' case_menu_done_from_state
check 'a failed step logs FAIL <id>: exit <n>, a skipped one SKIP, neither is recorded' case_log_fail_and_skip
check 'the question is asked at a console only, unless --yes, and a no runs nothing' case_confirm
check '--defaults with every default done logs SKIP done at <time> and runs nothing' case_nothing_to_run
check 'an unknown step exits 2 before anything runs' case_unknown_step
check '--defaults with --steps exits 2' case_defaults_with_steps
check 'authorized-key without --public-key-file exits 2 before anything runs' case_key_needed
check 'the master and every step refuse a --user that is not an account name' case_user_names
check 'with no console the menu is refused' case_no_tty_menu
check 'a run without root exits 1' case_not_root
check '--dry-run --defaults changes nothing' case_dry_run_defaults
check 'builder-account: an existing account, the console refusal, the sysadminctl call and its check' case_builder_account
check 'remote-login turns it on and fails when the re-read is still off' case_remote_login
check 'authorized-key appends once, as the builder, and refuses a private key or one ssh-keygen cannot read' case_authorized_key
check 'ssh-allow-users: rollback, the sudo user, already done, and a rule from another file' case_allow_users
check 'xcode-license accepts, sees it done, and skips without Xcode' case_xcode_license
check 'tailscale installs as the sudo user, starts the service and runs up, and refuses without a sudo user' case_tailscale
check 'filevault-off runs fdesetup disable at a console and refuses without one' case_filevault_off
check 'autologin skips while FileVault is on' case_autologin_filevault_on
check 'autologin checks the password, writes kcpassword 600 and autoLoginUser, and rewrites only with --force' case_autologin_writes
check 'the kcpassword encoding matches the XOR rule' case_kcpassword_bytes
check 'no script holds an IP literal, an ssh key, a home path or a tailnet name' case_secret_scan

printf '%s passed, %s failed\n' "$passes" "$fails"
[ "$fails" = 0 ]
