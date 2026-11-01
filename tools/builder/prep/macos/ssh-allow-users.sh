#!/usr/bin/env bash
# Lets only the builder account, and the admin that started sudo, log in over ssh: writes the drop-in
# /etc/ssh/sshd_config.d/100-gate-prep.conf with AllowUsers <user> [<sudo user>], then checks the config with sshd -t;
# when the check fails the drop-in is removed. It reads the effective set with sshd -T first and never widens a rule
# another file sets: an AllowUsers entry from elsewhere stops it. New ssh sessions take the drop-in; no restart needed.
# Usage: sudo bash ssh-allow-users.sh --user <name> [--dry-run]
# Exits 0 when done or already done, 1 when it failed, lacks root or meets another file's rule, 2 on a bad argument.
set -euo pipefail

prog='prep ssh-allow-users'
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

allow="$user"
admin="${SUDO_USER:-}"
if [ -n "$admin" ] && [ "$admin" != "$user" ] && [ "$admin" != root ]; then
    [[ "$admin" =~ ^[A-Za-z_][A-Za-z0-9_.-]*$ ]] || die "SUDO_USER '$admin' is not an account name"
    allow="$user $admin"
fi
drop_ins="${GATE_PREP_ROOT:-}/etc/ssh/sshd_config.d"
conf="$drop_ins/100-gate-prep.conf"
want="AllowUsers $allow"
[ -d "$drop_ins" ] || die "there is no $drop_ins, so this sshd reads no drop-ins; set AllowUsers in its sshd_config by hand"

sorted() { tr ' ' '\n' | sed '/^$/d' | LC_ALL=C sort -u; }
if ! dump=$(sshd -T 2>&1); then
    [ "$dry_run" = 1 ] || die "sshd -T failed, so the config cannot be read; fix it first: ${dump%%$'\n'*}"
    echo "could not read sshd -T (it needs root): ${dump%%$'\n'*}"
    echo "would write $conf: $want (root, 0644), then run: sshd -t"
    exit 0
fi
effective=$(printf '%s\n' "$dump" | awk '$1 == "allowusers" { print $2 }' | sorted)
ours=''
[ ! -f "$conf" ] || ours=$(sed -n 's/^AllowUsers //p' "$conf" | sorted)
others=$(LC_ALL=C comm -23 <(printf '%s\n' "$effective" | sed '/^$/d') <(printf '%s\n' "$ours" | sed '/^$/d'))
if [ -n "$others" ]; then
    named=$(printf '%s' "$others" | tr '\n' ' ')
    why="sshd already has AllowUsers ${named}from a file other than $conf"
    die "$why; merge them by hand into one AllowUsers line (this step never widens an existing rule)"
fi
if [ "$effective" = "$(printf '%s' "$allow" | sorted)" ]; then
    echo "already done: sshd -T allows $allow only"
    exit 0
fi
if [ "$dry_run" = 1 ]; then
    echo "would write $conf: $want (root, 0644), then run: sshd -t"
    exit 0
fi
[ "${GATE_PREP_TEST_UID:-$(id -u)}" = 0 ] || die 'needs root: run it with sudo'

printf '%s\n' "$want" >"$conf"
chmod 644 "$conf"
if ! out=$(sshd -t 2>&1); then
    rm -f "$conf"
    die "sshd -t failed with $conf in place, so it was removed: ${out:-no output}"
fi
echo "wrote $conf: only $allow may log in over ssh"
