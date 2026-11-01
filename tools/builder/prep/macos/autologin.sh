#!/usr/bin/env bash
# Logs the builder in automatically at boot. The password, typed twice without echo and checked with dscl, is stored
# only in /etc/kcpassword (root-only, reversibly encoded, as macOS automatic login requires: XORed with Apple's key and
# padded to a multiple of 12 bytes, root:wheel 0600); the login window's autoLoginUser names the builder.
# sysadminctl -autologin set is not used: it fails with error 22 on macOS 27. While FileVault is on, which bypasses
# automatic login, the step ends with a SKIP line. Re-run it with --force after a password change.
# Usage: sudo bash autologin.sh --user <name> [--force] [--dry-run]
# Exits 0 when done, already done or skipped, 1 when it failed, lacks root or the password does not log in, 2 on a bad
# argument or no console.
set -euo pipefail

prog='prep autologin'
user=''
dry_run=0
force=0
die() {
    printf '%s: %s\n' "$prog" "$1" >&2
    exit "${2:-1}"
}
has_console() { [ -t 0 ] || [ "${GATE_PREP_TEST_TTY:-}" = 1 ]; }

# Prints the /etc/kcpassword bytes for a password: its bytes, then zeros up to the next multiple of 12 (12 more when the
# length already is one), each XORed with the key's byte at its index modulo 11.
kcpassword_bytes() {
    local LC_ALL=C
    local pw="$1" n pad i b hex
    local -a key=(0x7D 0x89 0x52 0x23 0xD2 0xBC 0xDD 0xEA 0xA3 0xB9 0x1F)
    n=${#pw}
    pad=$((12 - n % 12))
    for ((i = 0; i < n + pad; i++)); do
        b=0
        if [ "$i" -lt "$n" ]; then printf -v b '%d' "'${pw:i:1}"; fi
        printf -v hex '%02x' $(((b & 255) ^ key[i % 11]))
        # shellcheck disable=SC2059 # the format is the byte's own \x escape
        printf "\\x$hex"
    done
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --user)
            [ "$#" -ge 2 ] && [ -n "$2" ] || die '--user needs a value' 2
            user="$2" && shift 2
            ;;
        --force) force=1 && shift ;;
        --dry-run) dry_run=1 && shift ;;
        *) die "unknown argument '$1'. Flags: --user <name>, --force, --dry-run" 2 ;;
    esac
done
[ -n "$user" ] || die '--user <name> is required' 2
[[ "$user" =~ ^[A-Za-z_][A-Za-z0-9_.-]*$ ]] ||
    die "--user '$user' is not an account name: a letter or _, then letters, digits, _, . or -" 2

kcpassword="${GATE_PREP_ROOT:-}/etc/kcpassword"
prefs="${GATE_PREP_ROOT:-}/Library/Preferences/com.apple.loginwindow"
status=$(fdesetup status 2>&1) || die "fdesetup status failed: $status"
case "$status" in
    'FileVault is Off.'*) ;;
    *)
        echo 'SKIP FileVault is on; turn it off (step filevault-off) and run this step again'
        exit 0
        ;;
esac
current=$(defaults read "$prefs" autoLoginUser 2>/dev/null || true)
if [ "$force" = 0 ] && [ "$current" = "$user" ] && [ -f "$kcpassword" ]; then
    echo "already done: $user logs in automatically (re-run with --force after a password change)"
    exit 0
fi
if [ "$dry_run" = 1 ]; then
    echo "would write $kcpassword (the password typed twice and checked, XOR-encoded; root:wheel, 0600)"
    echo "would run: defaults write $prefs autoLoginUser -string $user"
    exit 0
fi
[ "${GATE_PREP_TEST_UID:-$(id -u)}" = 0 ] || die 'needs root: run it with sudo'
id -u "$user" >/dev/null 2>&1 || die "there is no account $user; run the builder-account step first"
has_console || die "autologin asks for $user's password; run it at the console or over ssh -t" 2

{ set +x; } 2>/dev/null
password=''
read_password() {
    local try first second
    for try in 1 2 3; do
        IFS= read -rs -p "$user's password: " first || die 'no password read' 2
        printf '\n' >&2
        IFS= read -rs -p 'the same again: ' second || die 'no password read' 2
        printf '\n' >&2
        if [ -n "$first" ] && [ "$first" = "$second" ]; then
            password="$first"
            return 0
        fi
        printf '%s: the two differ or are empty (try %s of 3)\n' "$prog" "$try" >&2
    done
    die 'the passwords did not match in 3 tries; nothing changed'
}
read_password

# dscl checks a password only from argv. That is accepted here: the password is the builder's own, it is visible for
# milliseconds, and only the builder and the admin run processes on the box.
if ! dscl /Search -authonly "$user" "$password" >/dev/null 2>&1; then
    password=''
    die "the password does not log in as $user; nothing written"
fi

staged="$kcpassword.gate-prep.$$"
(
    umask 077
    kcpassword_bytes "$password" >"$staged"
)
password=''
if ! { chown root:wheel "$staged" && chmod 600 "$staged" && mv -f "$staged" "$kcpassword"; }; then
    rm -f "$staged"
    die "could not put $kcpassword in place"
fi
defaults write "$prefs" autoLoginUser -string "$user" || die "defaults write $prefs autoLoginUser failed"
echo "$user logs in automatically from the next boot"
