#!/usr/bin/env bash
# Turns on Remote Login (sshd), checks that it reads back on, and admits the builder account to ssh through the
# com.apple.access_ssh group.
# Usage: sudo bash remote-login.sh --user <name> [--dry-run]
# Exits 0 when done or already done, 1 when it failed or lacks root, 2 on a bad argument.
set -euo pipefail

prog='prep remote-login'
user=''
dry_run=0
die() {
    printf '%s: %s\n' "$prog" "$1" >&2
    exit "${2:-1}"
}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --user)
            [ "$#" -ge 2 ] && [ -n "$2" ] || die '--user needs a value' 2
            user="$2" && shift 2
            ;;
        --dry-run) dry_run=1 && shift ;;
        *) die "unknown argument '$1'. Flags: --user <name>, --dry-run" 2 ;;
    esac
done
[ -n "$user" ] || die '--user <name> is required' 2
[[ "$user" =~ ^[A-Za-z_][A-Za-z0-9_.-]*$ ]] ||
    die "--user '$user' is not an account name: a letter or _, then letters, digits, _, . or -" 2
group='com.apple.access_ssh'
fix='give the terminal Full Disk Access (System Settings, Privacy & Security), or turn on Remote Login in Sharing'
is_on() { case "$1" in *': On'*) return 0 ;; *) return 1 ;; esac }

# systemsetup reads the state only as root, so a dry run without root takes it as off and says so.
on=0
if state=$(systemsetup -getremotelogin 2>&1); then
    if is_on "$state"; then on=1; fi
elif [ "$dry_run" = 0 ]; then
    die "systemsetup -getremotelogin failed: $state"
else
    echo "could not read Remote Login's state without root: $state"
fi
# With no access group, Remote Login admits every account, the builder included.
member=1
if dseditgroup -o read "$group" >/dev/null 2>&1 && ! dseditgroup -o checkmember -m "$user" "$group" >/dev/null 2>&1; then
    member=0
fi
if [ "$on" = 1 ] && [ "$member" = 1 ]; then
    echo "already done: Remote Login is on and admits $user"
    exit 0
fi
if [ "$dry_run" = 1 ]; then
    [ "$on" = 1 ] || echo 'would run: systemsetup -setremotelogin on'
    [ "$member" = 1 ] || echo "would run: dseditgroup -o edit -a $user -t user $group"
    exit 0
fi
[ "${GATE_PREP_TEST_UID:-$(id -u)}" = 0 ] || die 'needs root: run it with sudo'

if [ "$on" = 0 ]; then
    systemsetup -setremotelogin on || die "systemsetup -setremotelogin on failed; $fix"
    state=$(systemsetup -getremotelogin 2>&1) || true
    is_on "$state" || die "Remote Login is still off after systemsetup -setremotelogin on (${state:-no output}); $fix"
    echo 'turned on Remote Login'
fi
if [ "$member" = 0 ]; then
    dseditgroup -o edit -a "$user" -t user "$group" || die "dseditgroup could not add $user to $group"
    echo "added $user to $group"
fi
