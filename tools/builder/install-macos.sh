#!/usr/bin/env bash
# Installs the macOS (Apple-silicon) builder's toolchains at pinned versions from official release URLs, its login PATH,
# its git config and the gate's work root, all as the builder user without sudo. A separate System step, run only under
# sudo with --system, keeps the machine awake, raises the open-file limits and turns on Remote Login. Two more steps,
# Homebrew and FFmpeg, need an admin account and change the machine only under --admin.
#
# Run it as the builder user, over ssh or copied to the machine:
#   ssh <builder> 'bash -s' -- [flags] < tools/builder/install-macos.sh
#   bash install-macos.sh [flags]
# the System step once, separately:
#   sudo bash install-macos.sh --system --user <builder user> --only System
# and the Homebrew and FFmpeg steps once, as an admin account, in an interactive terminal (an ssh -t session works):
# creating /opt/homebrew needs sudo, so the Homebrew step runs sudo -v and asks for that account's password up front.
#   bash install-macos.sh --admin --only Homebrew,FFmpeg
#
# Flags:
#   --prefix <dir>   where every tool lands (default $HOME/gate-tools): dotnet, pwsh, node, godot, bin, downloads and
#                    install.log. A leading ~/ is expanded, so a quoted ~/dir passes through ssh.
#   --root <dir>     the gate's work root (default $HOME/gate-remote), with work/ and bin/ under it, all mode 0700.
#   --only <p>[,<p>...]  runs just the steps whose names start with one of the prefixes (case-insensitive, like the
#                    Windows script's -Only), then the summary. A prefix that matches no step exits 2 before anything is
#                    written; --only Summary matches no step on purpose and runs only the summary.
#   --list           prints the step names and exits 0.
#   --system         lets the System step change the machine; needs root and --user. Without it the System step
#                    prints the commands it would run and logs SKIP.
#   --user <name>    the builder user, named in the System step's log; only taken with --system.
#   --admin          lets the Homebrew and FFmpeg steps change the machine; needs an account in the admin group, not
#                    root, not --system, and --only Homebrew,FFmpeg. Without it both print the commands they would run
#                    and log SKIP. An --admin run logs to stdout only, never into that account's prefix.
# An unknown argument exits 2. Every step but System, Homebrew and FFmpeg refuses to run as root (exit 2): the
# toolchains must land in the builder user's home, and --admin refuses root itself. Rust and cargo-nextest go to
# $CARGO_HOME (default ~/.cargo) and rustup's $RUSTUP_HOME, as rustup itself places them.
#
# Each step logs BEGIN, then OK, SKIP or FAIL with the reason, to stdout and timestamped to <prefix>/install.log; a
# failed step does not stop the later ones, and the script exits 1 when any step failed. Rerunnable: finished downloads
# in <prefix>/downloads are reused, and each step replaces what it installed before.
#
# Test hooks: INSTALL_MACOS_TEST_UID, when set, stands in for `id -u`, so test-install-macos.sh can check the root
# refusal without root; INSTALL_MACOS_TEST_GROUPS, when set, stands in for `id -Gn`'s output for the --admin check.
set -euo pipefail

prog='install-macos'
prefix="$HOME/gate-tools"
root="$HOME/gate-remote"
only=''
list=0
system=0
admin=0
builder_user=''

die2() {
    printf '%s: %s\n' "$prog" "$1" >&2
    exit 2
}

expand_home() {
    case "$1" in
        \~) printf '%s' "$HOME" ;;
        \~/*) printf '%s/%s' "$HOME" "${1#\~/}" ;;
        *) printf '%s' "$1" ;;
    esac
}

# The prefix and CARGO_HOME reach a double-quoted line in ~/.zprofile, so only a plain absolute path is taken.
check_path_arg() {
    case "$2" in
        /*) ;;
        *) die2 "$1 '$2' is not an absolute path" ;;
    esac
    case "$2" in
        *'"'* | *'$'* | *'`'* | *\\*) die2 "$1 '$2' holds a quote, \$, backtick or backslash; give a plain path" ;;
    esac
}

usage_flags='--prefix <dir>, --root <dir>, --only <prefix>[,<prefix>...], --list, --system, --user <name>, --admin'
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix | --root | --only | --user)
            [ "$#" -ge 2 ] && [ -n "$2" ] || die2 "$1 needs a value"
            case "$1" in
                --prefix) prefix="$2" ;;
                --root) root="$2" ;;
                --only) only="${only:+$only,}$2" ;;
                --user) builder_user="$2" ;;
            esac
            shift 2
            ;;
        --list) list=1 && shift ;;
        --system) system=1 && shift ;;
        --admin) admin=1 && shift ;;
        *) die2 "unknown argument '$1'. Flags: $usage_flags" ;;
    esac
done
prefix=$(expand_home "$prefix")
root=$(expand_home "$root")
cargo_home=$(expand_home "${CARGO_HOME:-$HOME/.cargo}")
check_path_arg --prefix "$prefix"
check_path_arg --root "$root"
check_path_arg CARGO_HOME "$cargo_home"

step_names=()
step_fns=()
step() {
    step_names+=("$1")
    step_fns+=("$2")
}

lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }

# True when the step name starts with the prefix, compared case-insensitively.
prefix_match() {
    local name prefix_lc
    name=$(lower "$1")
    prefix_lc=$(lower "$2")
    case "$name" in
        "$prefix_lc"*) return 0 ;;
        *) return 1 ;;
    esac
}

only_items() { printf '%s\n' "$only" | tr ',' '\n'; }

selected() {
    local p
    [ -n "$only" ] || return 0
    while IFS= read -r p; do
        if [ -n "$p" ] && prefix_match "$1" "$p"; then return 0; fi
    done < <(only_items)
    return 1
}

log_file=''
log() {
    local line
    line="$(date '+%Y-%m-%dT%H:%M:%S') $1"
    printf '%s\n' "$line"
    if [ -n "$log_file" ]; then printf '%s\n' "$line" >>"$log_file"; fi
}

reason_file=''
# A step calls fail or skip to end itself with a reason; any other failing command ends it through the ERR trap, which
# appends the command, so the first line of the reason file is the first failure.
fail() {
    printf '%s\n' "$1" >"$reason_file"
    exit 1
}
skip() {
    printf 'SKIP %s\n' "$1" >"$reason_file"
    exit 0
}

failed=''
run_step() {
    local name="$1" fn="$2" rc reason
    : >"$reason_file"
    log "BEGIN $name"
    set +e
    (
        set -eE
        trap 'printf "%s exited %s\n" "$BASH_COMMAND" "$?" >>"$reason_file"' ERR
        "$fn"
    )
    rc=$?
    set -e
    reason=$(head -n 1 "$reason_file")
    if [ "$rc" = 0 ]; then
        case "$reason" in
            'SKIP '*) log "SKIP $name: ${reason#SKIP }" ;;
            *) log "OK $name" ;;
        esac
    else
        log "FAIL $name: ${reason:-exited $rc}"
        failed="${failed:+$failed, }$name"
    fi
}

fetched=''
# Downloads a URL into <prefix>/downloads once, named from the URL's last segment unless a name is given, and leaves
# the path in $fetched. A partial download never takes the final name, so a rerun fetches it again.
fetch() {
    local url="$1" name="${2:-${1##*/}}"
    fetched="$prefix/downloads/$name"
    if [ ! -f "$fetched" ]; then
        mkdir -p "$prefix/downloads"
        curl -fsSL --retry 3 -o "$fetched.part" "$url"
        mv "$fetched.part" "$fetched"
    fi
}

# Compares a download's SHA-<bits> with the pinned value, deleting a download that does not match.
check_sum() {
    local bits="$1" file="$2" want="$3" got
    got=$(shasum -a "$bits" "$file" | cut -d ' ' -f 1)
    if [ "$got" != "$want" ]; then
        rm -f "$file"
        fail "${file##*/} SHA-$bits is $got, expected $want; the download was deleted, rerun to fetch it again"
    fi
}

# Unpacks a zip into a fresh folder under downloads and leaves its path in $unpacked.
unpacked=''
unpack_zip() {
    unpacked="$prefix/downloads/$2-extract"
    rm -rf "$unpacked"
    mkdir -p "$unpacked"
    ditto -x -k "$1" "$unpacked"
}

# Microsoft publishes no checksum for dotnet-install.sh; the script fetches the SDK itself.
install_dotnet_sdk() {
    fetch 'https://builds.dotnet.microsoft.com/dotnet/scripts/v1/dotnet-install.sh'
    bash "$fetched" --version "$1" --install-dir "$prefix/dotnet" --no-path
}
step_dotnet_10() { install_dotnet_sdk 10.0.401; }
step_dotnet_8() { install_dotnet_sdk 8.0.425; }
step_wasm_tools() { "$prefix/dotnet/dotnet" workload install wasm-tools; }

step_pwsh() {
    local dir="$prefix/pwsh/7.6.6"
    # From https://github.com/PowerShell/PowerShell/releases/download/v7.6.6/hashes.sha256
    fetch 'https://github.com/PowerShell/PowerShell/releases/download/v7.6.6/powershell-7.6.6-osx-arm64.tar.gz'
    check_sum 256 "$fetched" '6df833d094ebac1c1a74340d7b3437f4aaf5e03ce640484a1c4359f3ce8b3db1'
    rm -rf "$dir"
    mkdir -p "$dir" "$prefix/bin"
    tar -xzf "$fetched" -C "$dir"
    chmod +x "$dir/pwsh"
    ln -sfn "$dir/pwsh" "$prefix/bin/pwsh"
}

step_node() {
    # From https://nodejs.org/dist/v24.20.0/SHASUMS256.txt
    fetch 'https://nodejs.org/dist/v24.20.0/node-v24.20.0-darwin-arm64.tar.gz'
    check_sum 256 "$fetched" '40e5607e5ecb3db9192723776da2d75d966260fc74a7a9e731c1bd67dda96bc8'
    mkdir -p "$prefix/node"
    tar -xzf "$fetched" -C "$prefix/node" --strip-components 1
}

# npm checks the package against the registry's integrity hash; the global prefix is the user-owned node folder.
step_pnpm() { "$prefix/node/bin/npm" install -g --prefix "$prefix/node" pnpm@12.5.1; }

step_uv_prek() {
    mkdir -p "$prefix/bin"
    # From https://github.com/astral-sh/uv/releases/download/0.12.18/uv-aarch64-apple-darwin.tar.gz.sha256
    fetch 'https://github.com/astral-sh/uv/releases/download/0.12.18/uv-aarch64-apple-darwin.tar.gz'
    check_sum 256 "$fetched" 'cf40e0c6a202190ccd9e0406dcfdd5b2d6668a9a5c779b17948963df32aafe5b'
    tar -xzf "$fetched" -C "$prefix/bin" --strip-components 1
    # From https://github.com/j178/prek/releases/download/v0.5.0/prek-aarch64-apple-darwin.tar.gz.sha256
    fetch 'https://github.com/j178/prek/releases/download/v0.5.0/prek-aarch64-apple-darwin.tar.gz'
    check_sum 256 "$fetched" '73947efd142420dc1ad2856d155385b3fd54b88ba8bb769187dc8b359cc94681'
    tar -xzf "$fetched" -C "$prefix/bin" --strip-components 1
}

# rustup publishes no checksum for sh.rustup.rs; the script fetches rustup-init itself.
step_rust() {
    local tc
    if [ ! -x "$cargo_home/bin/rustup" ]; then
        fetch 'https://sh.rustup.rs' rustup-init.sh
        sh "$fetched" -y --no-modify-path --default-toolchain stable --profile minimal -c clippy -c rustfmt
    fi
    for tc in stable 1.98.1 1.96.1 1.89.0; do
        "$cargo_home/bin/rustup" toolchain install "$tc" --profile minimal -c clippy -c rustfmt
    done
}

step_nextest() {
    # From https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.148/cargo-nextest-0.9.148-universal-apple-darwin.sha256
    fetch 'https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.148/cargo-nextest-0.9.148-universal-apple-darwin.tar.gz'
    check_sum 256 "$fetched" '6c23b7fb4ca82c571fc8dfedbab45bfa773d45918f933170c85d21cd7f4b852b'
    mkdir -p "$cargo_home/bin"
    tar -xzf "$fetched" -C "$cargo_home/bin"
}

step_git_lfs() {
    # From https://github.com/git-lfs/git-lfs/releases/download/v3.8.0/sha256sums.asc
    fetch 'https://github.com/git-lfs/git-lfs/releases/download/v3.8.0/git-lfs-darwin-arm64-v3.8.0.zip'
    check_sum 256 "$fetched" 'caff76a7d070d8160c89bc39b6e85d98f24135b6fed038a3b4de2590d25102d8'
    unpack_zip "$fetched" git-lfs
    mkdir -p "$prefix/bin"
    cp "$unpacked/git-lfs-3.8.0/git-lfs" "$prefix/bin/git-lfs"
    chmod +x "$prefix/bin/git-lfs"
    rm -rf "$unpacked"
    git lfs install --skip-repo
}

# The gate's macOS Godot support reads GATE_GODOT_ROOT/<x.y.z>/Godot.app; this step only installs it.
step_godot() {
    local dest="$prefix/godot/4.7.2" app
    # From https://github.com/godotengine/godot-builds/releases/download/4.7.2-stable/SHA512-SUMS.txt
    fetch 'https://github.com/godotengine/godot-builds/releases/download/4.7.2-stable/Godot_v4.7.2-stable_mono_macos.universal.zip'
    check_sum 512 "$fetched" '0862c53d7158c7a67f745e2e46f90b68cf5343cbe8b95d6d4333c469e42ca104af9c121d1746a50e5d221a99d09d82ef7016495f8e0d09255842884ed0502795'
    unpack_zip "$fetched" godot
    for app in "$unpacked"/*.app; do
        [ -d "$app" ] || fail "no .app bundle at the top of ${fetched##*/}"
        rm -rf "$dest/Godot.app"
        mkdir -p "$dest"
        mv "$app" "$dest/Godot.app"
        break
    done
    rm -rf "$unpacked"
}

step_bun() {
    # From https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/SHASUMS256.txt
    fetch 'https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-darwin-aarch64.zip'
    check_sum 256 "$fetched" '90987a3a16d7db556d886ac3d551e7b6d3edf0a1cf43acaed622e8676be1d12f'
    unpack_zip "$fetched" bun
    mkdir -p "$prefix/bin"
    cp "$unpacked/bun-darwin-aarch64/bun" "$prefix/bin/bun"
    chmod +x "$prefix/bin/bun"
    ln -sfn bun "$prefix/bin/bunx"
    rm -rf "$unpacked"
}

brew_bin='/opt/homebrew/bin/brew'
ffmpeg_bin='/opt/homebrew/bin/ffmpeg'
# Homebrew's documented install method. Unlike this script's other downloads it is unpinned by nature: the installer
# always fetches Homebrew's current HEAD.
homebrew_url='https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh'

step_homebrew() {
    local out tmp
    if [ "$admin" != 1 ]; then
        log 'would run: sudo -v (asks for the admin password)'
        log "would download $homebrew_url and run it with NONINTERACTIVE=1 /bin/bash"
        log "would run: $brew_bin analytics off"
        skip 'run with --admin as an admin account'
    fi
    if [ ! -x "$brew_bin" ]; then
        # NONINTERACTIVE makes the installer call sudo -n, which never asks for a password, so the timestamp has to be
        # live before it runs: sudo -v asks for the password on the terminal and keeps it live for the install.
        sudo -v || fail "the admin's password is needed to create /opt/homebrew: run it interactively, over ssh -t"
        tmp=$(mktemp "${TMPDIR:-/tmp}/homebrew-install.XXXXXX") ||
            fail 'a temporary file for the Homebrew installer could not be made'
        if ! curl -fsSL -o "$tmp" "$homebrew_url"; then
            rm -f "$tmp"
            fail "downloading the Homebrew installer from $homebrew_url failed"
        fi
        if ! NONINTERACTIVE=1 /bin/bash "$tmp" </dev/null; then
            rm -f "$tmp"
            fail 'the Homebrew install failed'
        fi
        rm -f "$tmp"
        [ -x "$brew_bin" ] || fail "the Homebrew install finished without $brew_bin"
    fi
    if ! out=$("$brew_bin" --version 2>&1 </dev/null); then
        fail "$brew_bin --version failed: ${out%%$'\n'*}"
    fi
    log "$brew_bin --version: ${out%%$'\n'*}"
    "$brew_bin" analytics off </dev/null || fail "$brew_bin analytics off failed"
}

step_ffmpeg() {
    local out
    if [ "$admin" != 1 ]; then
        log "would run: HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 $brew_bin install ffmpeg"
        log "would run: $ffmpeg_bin -version"
        skip 'run with --admin as an admin account'
    fi
    [ -x "$brew_bin" ] || fail "$brew_bin is missing: run the Homebrew step first"
    HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 "$brew_bin" install ffmpeg </dev/null ||
        fail "$brew_bin install ffmpeg failed"
    if ! out=$("$ffmpeg_bin" -version 2>&1 </dev/null); then
        fail "$ffmpeg_bin -version failed: ${out%%$'\n'*}"
    fi
    log "${out%%$'\n'*}"
}

zprofile_begin='# >>> gate builder >>>'
zprofile_end='# <<< gate builder <<<'
# /opt/homebrew/bin comes after the gate-tools folders, so the pinned tools win over brew's. The block is written whether
# or not Homebrew is installed: a missing folder on PATH is harmless.
zprofile_block() {
    printf '%s\n' "$zprofile_begin" \
        "export PATH=\"$prefix/bin:$prefix/dotnet:$prefix/node/bin:$cargo_home/bin:/opt/homebrew/bin:\$PATH\"" \
        "export DOTNET_ROOT=\"$prefix/dotnet\"" \
        'export DOTNET_CLI_TELEMETRY_OPTOUT=1' \
        "export GATE_GODOT_ROOT=\"$prefix/godot\"" \
        'export HOMEBREW_NO_AUTO_UPDATE=1' \
        'export HOMEBREW_NO_ANALYTICS=1' \
        "$zprofile_end"
}

# The box reads the builder user's login-shell PATH, so the block goes to ~/.zprofile, replacing the one there.
step_login_path() {
    local zp="$HOME/.zprofile" kept='' out tool path want
    if [ -f "$zp" ]; then
        if ! kept=$(awk -v b="$zprofile_begin" -v e="$zprofile_end" \
            '$0 == b { s = 1; next } $0 == e { s = 0; next } !s { print } END { if (s) exit 3 }' "$zp"); then
            fail "$zp has '$zprofile_begin' without '$zprofile_end'; fix it by hand"
        fi
    fi
    {
        if [ -n "$kept" ]; then printf '%s\n' "$kept"; fi
        zprofile_block
    } >"$zp"
    log "wrote the gate builder block to $zp"
    # shellcheck disable=SC2016 # the zsh script expands its own variables
    if ! out=$(env -i HOME="$HOME" USER="${USER:-}" LOGNAME="${LOGNAME:-}" TERM=dumb /bin/zsh -l -c \
        'for t in dotnet pwsh node cargo; do
             p=$(command -v "$t") || { echo "$t: not found"; exit 1; }
             v=$("$t" --version 2>&1) || { echo "$t: --version failed: $v"; exit 1; }
             echo "$t $p $v"
         done' 2>&1); then
        fail "a fresh zsh -l does not run the tools: $out"
    fi
    while read -r tool path _; do
        case "$tool" in
            dotnet) want="$prefix/dotnet/" ;;
            pwsh) want="$prefix/bin/" ;;
            node) want="$prefix/node/bin/" ;;
            cargo) want="$cargo_home/bin/" ;;
            *) continue ;;
        esac
        case "$path" in
            "$want"*) ;;
            *) fail "$tool resolves to $path in a fresh zsh -l, not under $want" ;;
        esac
    done <<<"$out"
    log "a fresh zsh -l resolves: $(printf '%s' "$out" | tr '\n' ';')"
}

step_git_config() {
    local old rc
    if old=$(git config --global --get core.autocrlf); then
        :
    else
        rc=$?
        [ "$rc" = 1 ] || fail "git config --global --get core.autocrlf exited $rc"
        old=''
    fi
    if [ "$old" != false ]; then
        git config --global core.autocrlf false
        log "core.autocrlf was ${old:-unset}; set to false"
    fi
    git config --global gc.pruneExpire never
}

mode_of() {
    if [ "$(uname -s)" = Darwin ]; then stat -f '%Lp' "$1"; else stat -c '%a' "$1"; fi
}

# The box refuses a work root that is a link, not its user's, or writable by group or others.
step_gate_root() {
    local d mode
    mkdir -p "$(dirname "$root")"
    for d in "$root" "$root/work" "$root/bin"; do
        [ ! -L "$d" ] || fail "$d is a symlink; the box refuses one, so point --root at a real folder"
        [ ! -e "$d" ] || [ -d "$d" ] || fail "$d exists and is not a folder"
        if [ ! -d "$d" ]; then mkdir -m 700 "$d"; fi
        [ -O "$d" ] || fail "$d is owned by another account; the box refuses a work root its user does not own"
        mode=$(mode_of "$d")
        if [ "$mode" != 700 ]; then
            chmod 700 "$d"
            log "$d had mode $mode; set to 700"
        fi
    done
}

maxfiles_plist_path='/Library/LaunchDaemons/limit.maxfiles.plist'
maxfiles_plist() {
    printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' \
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
        '<plist version="1.0">' '<dict>' \
        '  <key>Label</key><string>limit.maxfiles</string>' \
        '  <key>ProgramArguments</key>' \
        '  <array><string>launchctl</string><string>limit</string><string>maxfiles</string><string>524288</string><string>524288</string></array>' \
        '  <key>RunAtLoad</key><true/>' '  <key>ServiceIPC</key><false/>' \
        '</dict>' '</plist>'
}

sys() {
    if [ "$system" = 1 ]; then
        log "run: $*"
        "$@"
    else
        log "would run: $*"
    fi
}

write_maxfiles_plist() {
    maxfiles_plist >"$maxfiles_plist_path"
    chown root:wheel "$maxfiles_plist_path"
    chmod 644 "$maxfiles_plist_path"
}

# Keeps every /etc/sysctl.conf line but the two limits, which it writes at the end.
write_sysctl_conf() {
    local f='/etc/sysctl.conf' kept=''
    if [ -f "$f" ]; then kept=$(awk '!/^kern\.maxfiles(perproc)?=/' "$f"); fi
    {
        if [ -n "$kept" ]; then printf '%s\n' "$kept"; fi
        printf '%s\n' 'kern.maxfiles=524288' 'kern.maxfilesperproc=524288'
    } >"$f"
}

step_system() {
    if [ "$system" = 1 ]; then log "System settings for builder user $builder_user"; fi
    sys pmset -a sleep 0 disksleep 0 autorestart 1 womp 1
    if [ "$system" = 1 ]; then
        log "writing $maxfiles_plist_path (root:wheel, 0644)"
        write_maxfiles_plist
        if launchctl print system/limit.maxfiles >/dev/null 2>&1; then sys launchctl bootout system/limit.maxfiles; fi
    else
        log "would write $maxfiles_plist_path (root:wheel, 0644): $(maxfiles_plist | tr -d '\n')"
        log 'would run: launchctl bootout system/limit.maxfiles (only when it is loaded)'
    fi
    sys launchctl bootstrap system "$maxfiles_plist_path"
    sys sysctl -w kern.maxfiles=524288
    sys sysctl -w kern.maxfilesperproc=524288
    if [ "$system" = 1 ]; then
        log 'setting kern.maxfiles=524288 and kern.maxfilesperproc=524288 in /etc/sysctl.conf'
        write_sysctl_conf
    else
        log 'would set kern.maxfiles=524288 and kern.maxfilesperproc=524288 in /etc/sysctl.conf, replacing their lines'
    fi
    sys systemsetup -setremotelogin on
    # Spotlight indexing of build trees costs CPU and disk on every build; nobody searches this machine.
    sys mdutil -a -i off
    # Some .NET packages ship x86_64-only macOS tools (Grpc.Tools' protoc), which run only under Rosetta.
    sys softwareupdate --install-rosetta --agree-to-license
    [ "$system" = 1 ] || skip 'run with sudo and --system'
}

version_of() {
    local label="$1" exe="$2" out
    shift 2
    if [ ! -x "$exe" ]; then
        log "  $label: missing"
    elif out=$("$exe" "$@" 2>&1); then
        log "  $label: ${out%%$'\n'*}"
    else
        log "  $label: $exe $* failed: ${out%%$'\n'*}"
    fi
}

# The builder reaches ffmpeg through its login shell, so the summary asks a fresh zsh -l as the Login PATH step does.
# A missing ffmpeg only reports: it never fails the run.
ffmpeg_summary() {
    local out path ver
    # shellcheck disable=SC2016 # the zsh script expands its own variables
    if out=$(env -i HOME="$HOME" USER="${USER:-}" LOGNAME="${LOGNAME:-}" TERM=dumb /bin/zsh -l -c \
        'p=$(command -v ffmpeg) || exit 3; printf "%s\n" "$p"; ffmpeg -version 2>&1 | head -n 1' 2>&1); then
        path=${out%%$'\n'*}
        ver=${out#*$'\n'}
        log "  ffmpeg $ver at $path"
    else
        log '  ffmpeg: not installed (run the Homebrew and FFmpeg steps with --admin as an admin account)'
    fi
}

summary() {
    log 'Summary:'
    version_of dotnet "$prefix/dotnet/dotnet" --version
    version_of pwsh "$prefix/bin/pwsh" --version
    version_of node "$prefix/node/bin/node" --version
    version_of pnpm "$prefix/node/bin/pnpm" --version
    version_of uv "$prefix/bin/uv" --version
    version_of prek "$prefix/bin/prek" --version
    version_of rustc "$cargo_home/bin/rustc" --version
    version_of cargo "$cargo_home/bin/cargo" --version
    version_of cargo-nextest "$cargo_home/bin/cargo-nextest" nextest --version
    version_of git-lfs "$prefix/bin/git-lfs" --version
    version_of godot "$prefix/godot/4.7.2/Godot.app/Contents/MacOS/Godot" --headless --version
    version_of bun "$prefix/bin/bun" --version
    ffmpeg_summary
}

step '.NET SDK 10.0.401' step_dotnet_10
step '.NET SDK 8.0.425' step_dotnet_8
step '.NET workload wasm-tools' step_wasm_tools
step 'PowerShell 7.6.6' step_pwsh
step 'Node.js 24.20.0' step_node
step 'pnpm 12.5.1' step_pnpm
step 'uv 0.12.18 and prek 0.5.0' step_uv_prek
step 'Rust (stable, 1.98.1, 1.96.1, 1.89.0)' step_rust
step 'cargo-nextest 0.9.148' step_nextest
step 'Git LFS 3.8.0' step_git_lfs
step 'Godot 4.7.2 mono' step_godot
step 'Bun 1.4.2' step_bun
step 'Homebrew' step_homebrew
step 'FFmpeg' step_ffmpeg
step 'Login PATH' step_login_path
step 'Git config' step_git_config
step 'Gate work root' step_gate_root
step 'System' step_system

if [ "$list" = 1 ]; then
    printf '%s\n' "${step_names[@]}"
    exit 0
fi

if [ -n "$only" ]; then
    while IFS= read -r p; do
        if [ -z "$p" ] || [ "$(lower "$p")" = summary ]; then continue; fi
        matched=0
        for n in "${step_names[@]}"; do
            if prefix_match "$n" "$p"; then matched=1 && break; fi
        done
        if [ "$matched" = 0 ]; then
            joined=$(printf '%s, ' "${step_names[@]}")
            die2 "--only '$p' matches no step. Steps: ${joined%, }"
        fi
    done < <(only_items)
fi

uid="${INSTALL_MACOS_TEST_UID:-$(id -u)}"

in_admin_group() {
    local groups
    if [ -n "${INSTALL_MACOS_TEST_GROUPS+x}" ]; then
        groups="$INSTALL_MACOS_TEST_GROUPS"
    else
        groups=$(id -Gn 2>/dev/null) || return 1
    fi
    case " $groups " in
        *' admin '*) return 0 ;;
        *) return 1 ;;
    esac
}

if [ "$admin" = 1 ]; then
    [ "$system" = 0 ] || die2 '--admin and --system are separate runs'
    [ "$uid" != 0 ] || die2 '--admin runs as an admin account, not root: run it without sudo'
    in_admin_group || die2 '--admin needs an account in the admin group'
    for n in "${step_names[@]}"; do
        case "$n" in
            Homebrew | FFmpeg) ;;
            *)
                if selected "$n"; then
                    die2 '--admin runs only the Homebrew and FFmpeg steps: pass --only Homebrew,FFmpeg'
                fi
                ;;
        esac
    done
fi

if [ "$uid" = 0 ]; then
    [ "$system" = 1 ] || die2 "running as root: the toolchain steps must land in the builder user's home; run without sudo, and use sudo only for --system --only System"
    for n in "${step_names[@]}"; do
        if selected "$n" && [ "$n" != System ] && [ "$n" != Homebrew ] && [ "$n" != FFmpeg ]; then
            die2 "step '$n' refuses to run as root: it must land in the builder user's home; under sudo pass --only System"
        fi
    done
else
    [ "$system" = 0 ] || die2 '--system needs root: run it under sudo with --user <builder user> --only System'
fi
if [ "$system" = 1 ]; then
    [ -n "$builder_user" ] || die2 '--system needs --user <name>, the builder user'
    id -u "$builder_user" >/dev/null 2>&1 || die2 "--user '$builder_user' is not an account on this machine"
    [ "$(id -u "$builder_user")" != 0 ] || die2 "--user '$builder_user' is root; name the builder user"
elif [ -n "$builder_user" ]; then
    die2 '--user only goes with --system'
fi

# A root-owned log in the builder user's prefix would stop the user's next run appending to it, so a root run logs to
# stdout only and skips the summary, which reads the user's tools. An --admin run belongs to the admin's home, so it
# writes no log there either and prints only the ffmpeg summary line.
if [ "$uid" != 0 ] && [ "$admin" = 0 ]; then
    mkdir -p "$prefix"
    log_file="$prefix/install.log"
fi
reason_file=$(mktemp "${TMPDIR:-/tmp}/install-macos.XXXXXX")
trap 'rm -f "$reason_file"' EXIT

export CARGO_HOME="$cargo_home"
export DOTNET_ROOT="$prefix/dotnet"
export DOTNET_CLI_TELEMETRY_OPTOUT=1
export PATH="$prefix/bin:$prefix/dotnet:$prefix/node/bin:$cargo_home/bin:$PATH"

i=0
while [ "$i" -lt "${#step_names[@]}" ]; do
    if selected "${step_names[$i]}"; then run_step "${step_names[$i]}" "${step_fns[$i]}"; fi
    i=$((i + 1))
done
if [ "$uid" != 0 ]; then
    if [ "$admin" = 1 ]; then ffmpeg_summary; else summary; fi
fi
if [ -n "$failed" ]; then
    log "failed steps: $failed"
    exit 1
fi
