#!/usr/bin/env bash
# Tests the macOS builder script `gate push` runs (MAC_SCRIPT in crates/gate/src/push.rs, read from there between its
# const line and its closing line) against a scratch root under mktemp -d, with a throw-away repo made from this tree and
# a real cargo build of its gate-box-unix: the first install past a failing RUSTC_WRAPPER and RUSTFLAGS, a second run,
# a held lock, a run killed mid-build, a cargo config and a toolchain file above the checkout, and a build killed whole
# at a short cargo bound.
# Runs on macOS only (it needs /usr/bin/lockf, /usr/bin/perl, cargo and cc); it touches nothing outside its scratch
# folder but the user's cargo caches.
# Usage: bash tools/builder/test-build-box-macos.sh
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
tree=$(cd "$here/../.." && pwd)
# The real path: the script refuses a root holding `//`, and macOS's TMPDIR ends with `/` under /var, a link.
scratch=$(mktemp -d "${TMPDIR:-/tmp}/test-build-box-macos.XXXXXX")
scratch=$(cd "$scratch" && pwd -P)
chmod 700 "$scratch"
holder=''
cleanup() {
    if [ -n "$holder" ]; then kill "$holder" 2>/dev/null || true; fi
    rm -rf "$scratch"
}
trap cleanup EXIT

script="$scratch/push.sh"
awk '/^const MAC_SCRIPT: &str = r#"$/ { f = 1; next } f && /^"#;$/ { exit } f { print }' "$tree/crates/gate/src/push.rs" >"$script"
grep -q '^main "\$@"$' "$script" || { echo "could not read MAC_SCRIPT from crates/gate/src/push.rs"; exit 2; }

root="$scratch/root"
gp="$root/gate-push"
# The root is left for the script to make.
repo="$scratch/repo"
mkdir "$repo"
tar -C "$tree" --exclude=./target --exclude=./.git --exclude=./.tmp -cf - . | tar -C "$repo" -xf -
git -C "$repo" init -q
git -C "$repo" add -A
git -C "$repo" -c user.name=test -c user.email=test@example.invalid -c commit.gpgsign=false commit -q -m tree
package=$(sed -n 's/^version = "\([0-9.]*\)"$/\1/p' "$tree/Cargo.toml" | head -n 1)

# The commit, id and version a build of the repo's HEAD names.
sha='' id='' version=''
named() {
    sha=$(git -C "$repo" rev-parse HEAD)
    id=${sha:0:12}
    version="$package+$id"
}

# Each run leaves its stdout in $scratch/out and its status in $rc.
rc=0
prepare() {
    set +e
    /bin/sh -s -- prepare "$root" <"$script" >"$scratch/out" 2>&1
    rc=$?
    set -e
}
pushed() { git -C "$repo" push -q "$gp/src.git" "+$sha:refs/gate-push/src"; }
build() {
    set +e
    /bin/sh -s -- build "$root" "$sha" "$id" "$version" <"$script" >"$scratch/out" 2>&1
    rc=$?
    set -e
}
# A build whose environment names a wrapper and a flag that fail any build taking them, which the script's empty
# RUSTC_WRAPPER and RUSTFLAGS override.
wrapped_build() {
    set +e
    RUSTC_WRAPPER=/usr/bin/false RUSTFLAGS=--no-such-rustc-flag \
        /bin/sh -s -- build "$root" "$sha" "$id" "$version" <"$script" >"$scratch/out" 2>&1
    rc=$?
    set -e
}

expect() {
    if ! "$@"; then
        echo "expected: $*"
        return 1
    fi
}
has() { grep -qxF -- "$1" "$scratch/out"; }
rc_is() { [ "$rc" = "$1" ]; }
says() { [ "$("$1" --version)" = "gate $2" ]; }

# Each case runs in this shell, so the commit a case names stays named for the next; its own lines go to $scratch/why.
passes=0
fails=0
check() {
    if "$2" >"$scratch/why" 2>&1; then
        passes=$((passes + 1))
        printf 'ok   %s\n' "$1"
    else
        fails=$((fails + 1))
        printf 'FAIL %s: %s\n' "$1" "$(cat "$scratch/why")"
        sed 's/^/     | /' "$scratch/out"
    fi
}
exec 3>&1

case_first_install() {
    named
    prepare
    expect rc_is 0 && expect has 'gate-push: version ' && expect has 'gate-push: ready' || return 1
    pushed
    local start=$SECONDS
    wrapped_build
    echo "     cold build: $((SECONDS - start)) s" >&3
    expect rc_is 0 && expect has "gate-push: installed $version" && expect says "$root/bin/gate-box" "$version" &&
        expect [ ! -e "$gp/building" ] && expect [ "$(ls "$root/bin")" = gate-box ] &&
        expect [ "$(stat -f '%Lp' "$root")" = 700 ] && expect [ "$(find "$gp/src.git/objects" -type f -perm +077 | wc -l)" -eq 0 ] &&
        expect [ "$(stat -f '%Lp' "$root/bin/gate-box")" = 700 ] && expect [ "$(stat -f '%Lp' "$gp")" = 700 ] &&
        expect [ "$(stat -f '%Lp' "$gp/build.log")" = 600 ]
}

case_second_run() {
    prepare
    expect rc_is 0 && expect has "gate-push: version $version" || return 1
    build
    expect rc_is 0 && expect has "gate-push: replaced $version" && expect has "gate-push: installed $version" &&
        expect says "$root/bin/gate-box" "$version"
}

# A second name of build.log fails the prepare, and a gate-box that is a link fails the build, each left as it was.
case_links_refused() {
    local log="$gp/build.log" box="$root/bin/gate-box"
    mv "$log" "$scratch/log.kept"
    : >"$scratch/other"
    ln "$scratch/other" "$log"
    prepare
    rm -f "$log"
    mv "$scratch/log.kept" "$log"
    expect rc_is 1 && expect has "gate-push: failed $log is a link" || return 1
    mv "$box" "$scratch/box.kept"
    ln -s "$scratch/box.kept" "$box"
    build
    local linked=0
    [ -L "$box" ] && linked=1
    rm -f "$box"
    mv "$scratch/box.kept" "$box"
    expect rc_is 1 && expect has "gate-push: failed $box is a symbolic link" && expect [ "$linked" = 1 ]
}

case_held_lock() {
    local ready="$scratch/held"
    (
        exec 9>>"$gp/push.lock"
        /usr/bin/lockf -s -t 0 9
        : >"$ready"
        exec sleep 120
    ) &
    holder=$!
    local waited=0
    while [ ! -e "$ready" ]; do
        sleep 0.1
        waited=$((waited + 1))
        [ "$waited" -lt 300 ] || { echo 'the holder never took the lock'; return 1; }
    done
    build
    kill "$holder"
    wait "$holder" 2>/dev/null || true
    holder=''
    expect rc_is 1 && expect has 'gate-push: failed another gate push is installing on this builder' &&
        expect says "$root/bin/gate-box" "$version"
}

# A new commit makes the build compile again; the script's shell is killed once cargo is running, which leaves cargo
# running and holding the lock, the building marker in place and the installed box as it was.
case_killed_build() {
    local before=$version pid waited=0
    printf '\n// a change the killed build compiles\n' >>"$repo/crates/gate-box-unix/src/main.rs"
    git -C "$repo" -c user.name=test -c user.email=test@example.invalid -c commit.gpgsign=false commit -q -am change
    named
    pushed
    /bin/sh -s -- build "$root" "$sha" "$id" "$version" <"$script" >"$scratch/out" 2>&1 &
    pid=$!
    while ! pgrep -f "$gp/target" >/dev/null; do
        sleep 0.2
        waited=$((waited + 1))
        [ "$waited" -lt 600 ] || { echo 'cargo never started'; return 1; }
    done
    kill -9 "$pid"
    wait "$pid" 2>/dev/null || true
    expect [ -e "$gp/building" ] && expect says "$root/bin/gate-box" "$before" || return 1
    waited=0
    while pgrep -f "$gp/target" >/dev/null; do
        sleep 0.5
        waited=$((waited + 1))
        [ "$waited" -lt 3600 ] || { echo 'the orphaned cargo never ended'; return 1; }
    done
    : >"$gp/target/sentinel"
    build
    expect rc_is 0 && expect has "gate-push: replaced $before" && expect has "gate-push: installed $version" &&
        expect says "$root/bin/gate-box" "$version" && expect [ ! -e "$gp/target/sentinel" ] && expect [ ! -e "$gp/building" ]
}

# A cargo config and a toolchain file in the root, above the checkout, would fail any build that read them: a linker
# that always fails and a toolchain that does not exist. A new commit makes the build link again.
case_config_above_checkout_unread() {
    local before=$version triple
    triple=$(rustc -vV | sed -n 's/^host: //p')
    mkdir -p "$root/.cargo"
    printf '[target.%s]\nlinker = "/usr/bin/false"\n' "$triple" >"$root/.cargo/config.toml"
    printf '[toolchain]\nchannel = "no-such-toolchain"\n' >"$root/rust-toolchain.toml"
    echo "     rustup's toolchain from /: $(cd / && rustup show active-toolchain 2>&1 | head -n 1)" >&3
    printf '\n// a change the build links\n' >>"$repo/crates/gate-box-unix/src/main.rs"
    git -C "$repo" -c user.name=test -c user.email=test@example.invalid -c commit.gpgsign=false commit -q -am relink
    named
    pushed
    build
    rm -rf "$root/.cargo" "$root/rust-toolchain.toml"
    expect rc_is 0 && expect has "gate-push: replaced $before" && expect has "gate-push: installed $version" &&
        expect says "$root/bin/gate-box" "$version"
}

# A build script that sleeps past a short cargo bound: the bound kills cargo's whole process group, the build script
# with it, so no process of the build is left, the lock is free and no building marker is left to make the next
# build cold.
case_bound_kills_the_build() {
    local short="$scratch/short.sh" waited=0
    sed 's/bounded 1800 cargo build/bounded 20 cargo build/; s/did not finish in 1800 s/did not finish in 20 s/' \
        "$script" >"$short"
    grep -q 'bounded 20 cargo build' "$short" || { echo 'could not shorten the cargo bound'; return 1; }
    printf 'fn main() {\n    std::thread::sleep(std::time::Duration::from_secs(600));\n}\n' \
        >"$repo/crates/gate-box-unix/build.rs"
    git -C "$repo" add crates/gate-box-unix/build.rs
    git -C "$repo" -c user.name=test -c user.email=test@example.invalid -c commit.gpgsign=false commit -q -m sleep
    named
    pushed
    set +e
    /bin/sh -s -- build "$root" "$sha" "$id" "$version" <"$short" >"$scratch/out" 2>&1
    rc=$?
    set -e
    expect rc_is 1 && expect has "gate-push: failed cargo build did not finish in 20 s; see $gp/build.log" &&
        expect [ ! -e "$gp/building" ] || return 1
    while pgrep -f "$gp/target" >/dev/null; do
        sleep 0.1
        waited=$((waited + 1))
        [ "$waited" -lt 50 ] || { echo "a process of the build outlived the bound: $(pgrep -lf "$gp/target")"; return 1; }
    done
    exec 8>>"$gp/push.lock"
    local free=0
    /usr/bin/lockf -s -t 0 8 && free=1
    exec 8>&-
    expect [ "$free" = 1 ]
}

check 'a first build, past a failing RUSTC_WRAPPER and RUSTFLAGS, installs gate-box saying the pushed version' case_first_install
check 'a second run sees the installed version and builds the same again' case_second_run
check 'a hard-linked build log and a linked box are refused' case_links_refused
check 'a held lock refuses with the busy line and leaves the box' case_held_lock
check 'a killed build leaves the box and the next run wipes target and installs' case_killed_build
check 'a cargo config and a toolchain file above the checkout are not read' case_config_above_checkout_unread
check 'the cargo bound kills every process of the build and frees the lock' case_bound_kills_the_build

printf '%s passed, %s failed\n' "$passes" "$fails"
[ "$fails" = 0 ]
