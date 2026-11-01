#!/usr/bin/env bash
# Creates the builder account, a standard (not admin) user; an existing account is left alone. sysadminctl asks for the
# password itself on the terminal (-password -), so the step never holds it.
# Usage: sudo bash builder-account.sh --user <name> [--dry-run]
# Exits 0 when done or already done, 1 when it failed or lacks root, 2 on a bad argument or no console for the password.
set -euo pipefail

prog='prep builder-account'
user=''
dry_run=0
die() {
    printf '%s: %s\n' "$prog" "$1" >&2
    exit "${2:-1}"
}
has_console() { [ -t 0 ] || [ "${GATE_PREP_TEST_TTY:-}" = 1 ]; }
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

if id -u "$user" >/dev/null 2>&1; then
    echo "already done: the account $user exists"
    exit 0
fi
if [ "$dry_run" = 1 ]; then
    echo "would run: sysadminctl -addUser $user -password - (a standard account; sysadminctl asks for the password)"
    exit 0
fi
[ "${GATE_PREP_TEST_UID:-$(id -u)}" = 0 ] || die 'needs root: run it with sudo'
has_console || die 'builder-account asks for the password; run it at the console or over ssh -t' 2

{ set +x; } 2>/dev/null
# sysadminctl's help: "Pass '-' instead of password in commands above to request prompt."
sysadminctl -addUser "$user" -password - || die "sysadminctl -addUser $user failed"
id -u "$user" >/dev/null 2>&1 || die "sysadminctl -addUser ran, but there is still no account $user; read its output above"
echo "created the standard account $user"
