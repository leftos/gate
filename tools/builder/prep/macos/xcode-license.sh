#!/usr/bin/env bash
# Accepts the Xcode license, so builds that call Xcode's tools run. Xcode counts as installed when xcode-select -p ends
# in .app/Contents/Developer; otherwise the step prints SKIP no Xcode.
# Usage: sudo bash xcode-license.sh --user <name> [--dry-run]
# Exits 0 when done, already done or skipped, 1 when it failed or lacks root, 2 on a bad argument.
set -euo pipefail

prog='prep xcode-license'
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

developer=$(xcode-select -p 2>/dev/null || true)
case "$developer" in
    *.app/Contents/Developer) ;;
    *)
        echo 'SKIP no Xcode'
        exit 0
        ;;
esac
if xcodebuild -license check >/dev/null 2>&1; then
    echo 'already done: the Xcode license is accepted'
    exit 0
fi
if [ "$dry_run" = 1 ]; then
    echo 'would run: xcodebuild -license accept'
    exit 0
fi
[ "${GATE_PREP_TEST_UID:-$(id -u)}" = 0 ] || die 'needs root: run it with sudo'
xcodebuild -license accept || die 'xcodebuild -license accept failed; read its output above'
echo 'accepted the Xcode license'
