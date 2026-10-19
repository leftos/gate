#!/usr/bin/env bash
# Runs as root inside the builder's Ubuntu WSL distro (install-toolchains.ps1, step "WSL Ubuntu"): the Linux packages
# a routed repo's Linux build needs plus the tools its WSL pipeline calls, then rustup with that repo's pinned
# toolchain. Rerunnable.
set -euo pipefail
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
