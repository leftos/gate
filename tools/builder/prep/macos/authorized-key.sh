#!/usr/bin/env bash
# Appends the PC's public key to the builder's ~/.ssh/authorized_keys when it is not there yet, folder 700 and file 600.
# Root only reads the key file: every write in the builder's home runs as the builder (sudo -u), with the key on stdin,
# so a link in that home cannot turn a root write elsewhere.
# Usage: sudo bash authorized-key.sh --user <name> --public-key-file <path> [--dry-run]
# Exits 0 when done or already done, 1 when it failed or lacks root, 2 on a bad argument.
set -euo pipefail

prog='prep authorized-key'
user=''
key_file=''
dry_run=0
die() {
    printf '%s: %s\n' "$prog" "$1" >&2
    exit "${2:-1}"
}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --user | --public-key-file)
            [ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 needs a value" 2
            if [ "$1" = --user ]; then user="$2"; else key_file="$2"; fi
            shift 2
            ;;
        --dry-run) dry_run=1 && shift ;;
        *) die "unknown argument '$1'. Flags: --user <name>, --public-key-file <path>, --dry-run" 2 ;;
    esac
done
[ -n "$user" ] || die '--user <name> is required' 2
[[ "$user" =~ ^[A-Za-z_][A-Za-z0-9_.-]*$ ]] ||
    die "--user '$user' is not an account name: a letter or _, then letters, digits, _, . or -" 2
[ -n "$key_file" ] || die '--public-key-file <path> is required: the PC'"'"'s public key' 2
[ -f "$key_file" ] || die "--public-key-file '$key_file' is not a file" 2
! grep -q -- '-----BEGIN' "$key_file" || die "'$key_file': that is a private key; pass the .pub file" 2
key=$(tr -d '\r' <"$key_file" | grep -v '^[[:space:]]*$' | head -n 1 || true)
[ -n "$key" ] || die "'$key_file' holds no key line" 2

# ssh-keygen reads the line from a copy, which also keeps a path starting with - from reading as a flag.
checked=$(mktemp "${TMPDIR:-/tmp}/prep-authorized-key.XXXXXX")
trap 'rm -f "$checked"' EXIT
printf '%s\n' "$key" >"$checked"
ssh-keygen -l -f "$checked" >/dev/null 2>&1 || die "'$key_file' is not a public key ssh-keygen -l can read; pass the PC's .pub file" 2

home=''
if info=$(dscl . -read "/Users/$user" NFSHomeDirectory 2>/dev/null); then
    home=$(printf '%s\n' "$info" | sed -n 's/^NFSHomeDirectory: \(\/.*\)$/\1/p' | head -n 1)
fi
if [ -z "$home" ]; then
    [ "$dry_run" = 0 ] || {
        echo "would append the key in $key_file to $user's ~/.ssh/authorized_keys once the account exists"
        exit 0
    }
    die "$user has no home folder in dscl; run the builder-account step first"
fi
ssh_dir="$home/.ssh"
keys="$ssh_dir/authorized_keys"
[ ! -L "$ssh_dir" ] && [ ! -L "$keys" ] || die "$ssh_dir or $keys is a symlink; remove it and run the step again"

if [ -f "$keys" ] && grep -qxF -- "$key" "$keys" 2>/dev/null; then
    echo "already done: $keys holds the key"
    exit 0
fi
if [ "$dry_run" = 1 ]; then
    echo "would append the key in $key_file to $keys as $user (folder 700, file 600)"
    exit 0
fi
[ "${GATE_PREP_TEST_UID:-$(id -u)}" = 0 ] || die 'needs root: run it with sudo'

# shellcheck disable=SC2016 # the script is the builder's shell's, which expands it
append='umask 077
dir="$HOME/.ssh"
file="$dir/authorized_keys"
IFS= read -r key || exit 1
mkdir -p "$dir" && touch "$file" && chmod 700 "$dir" && chmod 600 "$file" || exit 1
if grep -qxF -e "$key" "$file"; then echo present; exit 0; fi
if [ -s "$file" ] && [ -n "$(tail -c 1 "$file")" ]; then printf "\n" >>"$file"; fi
printf "%s\n" "$key" >>"$file" && echo appended'
result=$(printf '%s\n' "$key" | sudo -u "$user" -H /bin/sh -c "$append") || die "writing $keys as $user failed"
case "$result" in
    present) echo "already done: $keys holds the key" ;;
    appended) echo "appended the key to $keys" ;;
    *) die "writing $keys as $user said '$result'" ;;
esac
