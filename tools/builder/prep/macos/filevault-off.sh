#!/usr/bin/env bash
# Turns FileVault off when it is on: fdesetup disable asks for its own unlock credential, so the step needs a terminal.
# The disk then decrypts in the background. This lowers the security of the disk; automatic login needs it.
# Usage: sudo bash filevault-off.sh --user <name> [--dry-run]
# Exits 0 when done or already done, 1 when it failed or lacks root, 2 on a bad argument or no console.
set -euo pipefail

prog='prep filevault-off'
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

status=$(fdesetup status 2>&1) || die "fdesetup status failed: $status"
case "$status" in
    *'Decryption in progress'*)
        echo 'already done: decryption is in progress; wait for it to finish before automatic login'
        exit 0
        ;;
    'FileVault is Off.'*)
        echo 'already done: FileVault is off'
        exit 0
        ;;
esac
if [ "$dry_run" = 1 ]; then
    echo "would run: fdesetup disable (it asks for an unlock credential; now: ${status%%$'\n'*})"
    exit 0
fi
[ "${GATE_PREP_TEST_UID:-$(id -u)}" = 0 ] || die 'needs root: run it with sudo'
has_console || die 'fdesetup disable asks for a credential; run it at the console or over ssh -t' 2
fdesetup disable || die 'fdesetup disable failed; read its output above'
echo 'FileVault is turning off; the disk decrypts in the background'
