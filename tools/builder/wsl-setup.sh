#!/usr/bin/env bash
# Runs as root inside the builder's Ubuntu WSL distro (install-toolchains.ps1, step "WSL Ubuntu"): the Linux packages
# a routed repo's Linux build needs plus the tools its WSL pipeline calls, then rustup with that repo's pinned
# toolchain, and incremental = false in the build table of root's cargo config.toml. Rerunnable.
set -euo pipefail

# Prints the cargo config.toml on stdin with build.incremental set to false, every other line kept: an existing
# incremental line in the build table, a [build] table or top-level build.<key> dotted keys, with the keys bare,
# basic-quoted or literal-quoted, is replaced, else the key goes after the build table's last key, else a [build] table
# is appended. On an inline build table, an incremental key set twice or both build forms it prints only the reason and
# exits 1. A copy of install-macos.sh's function of the same name: this script is copied to the builder and run inside
# WSL on its own, so the two cannot share a file.
cargo_incremental_merged() {
  awk '
  function bare(s) { gsub(/["\047]/, "", s); return s }
  { line[NR] = $0 }
  /^[ \t]*\[/ {
    name = $0
    sub(/^[ \t]*\[[ \t]*/, "", name)
    sub(/[ \t]*\].*$/, "", name)
    section = bare(name)
    if (section == "build") { table = 1; end = NR }
    next
  }
  section == "" && /^[ \t]*("build"|\047build\047|build)[ \t]*=/ { if (!inline) inline = NR; next }
  section == "" && /^[ \t]*("build"|\047build\047|build)[ \t]*\./ {
    dotted = 1
    end = NR
    if ($0 ~ /^[ \t]*("build"|\047build\047|build)[ \t]*\.[ \t]*("incremental"|\047incremental\047|incremental)[ \t]*=/) {
      if (key) { if (!twice) twice = NR } else key = NR
    }
    next
  }
  section == "build" && /^[ \t]*[^# \t]/ {
    end = NR
    if ($0 ~ /^[ \t]*("incremental"|\047incremental\047|incremental)[ \t]*=/) {
      if (key) { if (!twice) twice = NR } else key = NR
    }
  }
  END {
    err = ""
    if (inline) err = "line " inline " sets build as an inline table, which this step does not rewrite: " line[inline]
    else if (table && dotted) err = "has both a [build] table and top-level build.<key> keys"
    else if (twice) err = "sets build.incremental twice; line " twice ": " line[twice]
    if (err != "") { print "cargo config.toml " err; exit 1 }
    entry = dotted ? "build.incremental = false" : "incremental = false"
    for (i = 1; i <= NR; i++) {
      if (i == key) print entry
      else print line[i]
      if (!key && i == end) print entry
    }
    if (!key && !end) {
      if (NR) print ""
      print "[build]"
      print entry
    }
  }'
}

# Sets incremental = false in the build table of the cargo config.toml at $1, creating it when absent. Routed cargo
# runs build without incremental compilation, because the incremental cache was most of a work folder's size;
# build.incremental overrides every profile's setting, and a run's CARGO_INCREMENTAL still overrides it. The merge goes
# to a scratch file first and is copied over the config, so a refusal leaves the config as it was and returns 1.
cargo_config_without_incremental() {
  local config="$1" merged="$1.gate-tmp" why
  mkdir -p "$(dirname "$config")"
  if [ -e "$config" ]; then
    if ! cargo_incremental_merged <"$config" >"$merged"; then
      why=$(cat "$merged")
      rm -f "$merged"
      echo "$config: ${why#cargo config.toml }" >&2
      return 1
    fi
  else
    cargo_incremental_merged </dev/null >"$merged"
  fi
  cat "$merged" >"$config"
  rm -f "$merged"
  echo "$config [build] incremental = false"
}

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install --no-install-recommends -y \
  build-essential pkg-config libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev \
  libfreetype6-dev libfontconfig1-dev fonts-dejavu-core libasound2-dev \
  python3 jq rsync acl curl ca-certificates git
if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.96.1 -c clippy -c rustfmt
fi
# shellcheck source=/dev/null
. "$HOME/.cargo/env"
rustup toolchain install 1.96.1 --profile minimal -c clippy -c rustfmt
rustup toolchain list
cargo_config_without_incremental "${CARGO_HOME:-$HOME/.cargo}/config.toml"
