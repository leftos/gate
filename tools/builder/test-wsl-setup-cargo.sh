#!/usr/bin/env bash
# Tests wsl-setup.sh's cargo config step without running the setup: the two functions that write build.incremental =
# false into cargo's config.toml are taken out of the script, which installs packages and cannot be sourced, and called
# on a config in a temporary folder. Runs in Git Bash and on Linux.
# Usage: bash tools/builder/test-wsl-setup-cargo.sh
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
script="$here/wsl-setup.sh"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/test-wsl-setup-cargo.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
config="$scratch/cargo/config.toml"

eval "$(sed -n -e '/^cargo_incremental_merged() {$/,/^}$/p' -e '/^cargo_config_without_incremental() {$/,/^}$/p' "$script")"

# Writes the lines given (none: no file) as the config, then runs the step on it; leaves its exit code in $rc and its
# stdout and stderr in $scratch/out.
rc=0
run_step() {
    rm -rf "$scratch/cargo"
    if [ "$#" -gt 0 ]; then
        mkdir -p "$scratch/cargo"
        printf '%s\n' "$@" >"$config"
    fi
    run_again
}
run_again() {
    set +e
    (cargo_config_without_incremental "$config") >"$scratch/out" 2>&1
    rc=$?
    set -e
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
# Whether the config file holds exactly the lines given.
config_is() {
    local want
    want=$(printf '%s\n' "$@")
    if [ "$(cat "$config")" != "$want" ]; then
        printf 'config.toml holds:\n%s\nexpected:\n%s\n' "$(cat "$config")" "$want"
        return 1
    fi
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

case_absent() {
    run_step
    printf '[build]\nincremental = false\n' >"$scratch/want"
    expect rc_is 0 && expect cmp -s "$scratch/want" "$config" && expect has "$config [build] incremental = false" || return 1
    run_again
    expect rc_is 0 && expect cmp -s "$scratch/want" "$config"
}

case_other_keys() {
    run_step '# mine' '[net]' 'retry = 2' '' '[build]' 'jobs = 4' '' '[env]' 'A = "1"'
    expect rc_is 0 && expect config_is '# mine' '[net]' 'retry = 2' '' '[build]' 'jobs = 4' 'incremental = false' '' \
        '[env]' 'A = "1"' || return 1
    run_step '[net]' 'retry = 2'
    expect rc_is 0 && expect config_is '[net]' 'retry = 2' '' '[build]' 'incremental = false'
}

case_incremental_true() {
    run_step '[build]' 'incremental = true' 'jobs = 4'
    expect rc_is 0 && expect config_is '[build]' 'incremental = false' 'jobs = 4' || return 1
    run_step 'build.jobs = 4' 'build.incremental = true'
    expect rc_is 0 && expect config_is 'build.jobs = 4' 'build.incremental = false'
}

case_inline_refused() {
    run_step 'build = { jobs = 4 }'
    expect rc_is 1 &&
        expect has "$config: line 1 sets build as an inline table, which this step does not rewrite: build = { jobs = 4 }" &&
        expect config_is 'build = { jobs = 4 }' && expect absent "$config.gate-tmp"
}

check 'a missing config gets [build] incremental = false, and a second run leaves it unchanged' case_absent
check 'a config with other keys keeps every line and gains incremental = false in its build table' case_other_keys
check 'an existing incremental = true is replaced, in a [build] table and as a dotted key' case_incremental_true
check 'an inline build table is refused and the config left as it was' case_inline_refused

printf '%d passed, %d failed\n' "$passes" "$fails"
[ "$fails" -eq 0 ]
