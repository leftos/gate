#!/usr/bin/env bash
# Installs Homebrew's open-source tailscale formula when it is missing (as the admin account that started sudo, since
# Homebrew refuses root), starts tailscaled as a LaunchDaemon with brew services as root, then runs tailscale up: you
# sign in through the link it prints. No auth key is ever taken.
# Usage: sudo bash tailscale.sh --user <name> [--dry-run]
# Exits 0 when done or already done, 1 when it failed or lacks root, 2 on a bad argument.
set -euo pipefail

prog='prep tailscale'
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

# Homebrew's own places: Apple silicon, then Intel.
brew=''
for candidate in "${GATE_PREP_ROOT:-}/opt/homebrew/bin/brew" "${GATE_PREP_ROOT:-}/usr/local/bin/brew"; do
    if [ -x "$candidate" ]; then
        brew="$candidate"
        break
    fi
done
ts=''
if command -v tailscale >/dev/null 2>&1; then
    ts=$(command -v tailscale)
elif [ -n "$brew" ] && [ -x "${brew%/brew}/tailscale" ]; then
    ts="${brew%/brew}/tailscale"
fi
if [ -n "$ts" ] && "$ts" status >/dev/null 2>&1; then
    echo 'already done: tailscale status succeeds'
    exit 0
fi
no_brew='no Homebrew at /opt/homebrew/bin/brew or /usr/local/bin/brew; install it first (install-macos.sh --admin --only Homebrew)'
if [ "$dry_run" = 1 ]; then
    [ -n "$brew" ] || echo "$no_brew"
    [ -n "$ts" ] || echo "would run: sudo -u ${SUDO_USER:-<the admin account>} -H ${brew:-brew} install tailscale"
    echo "would run: ${brew:-brew} services start tailscale (as root: tailscaled as a LaunchDaemon)"
    echo 'would run: tailscale up (sign in through the link it prints)'
    exit 0
fi
[ "${GATE_PREP_TEST_UID:-$(id -u)}" = 0 ] || die 'needs root: run it with sudo'
[ -n "$brew" ] || die "$no_brew"

if [ -z "$ts" ]; then
    admin="${SUDO_USER:-}"
    [ -n "$admin" ] && [ "$admin" != root ] ||
        die 'Homebrew refuses root: run prep with sudo from an admin account, so this step can run brew as that account'
    [[ "$admin" =~ ^[A-Za-z_][A-Za-z0-9_.-]*$ ]] || die "SUDO_USER '$admin' is not an account name"
    sudo -u "$admin" -H "$brew" install tailscale || die 'brew install tailscale failed'
    ts="${brew%/brew}/tailscale"
    [ -x "$ts" ] || die "brew install tailscale left no $ts"
fi
"$brew" services start tailscale || die 'brew services start tailscale failed; read its output above'
"$ts" up || die 'tailscale up failed; read its output above'
echo 'Tailscale is up'
