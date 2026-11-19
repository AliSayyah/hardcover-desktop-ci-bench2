#!/usr/bin/env bash
# Installs what building the workspace needs on Ubuntu 22.04 or newer: a C
# toolchain and the system libraries the -sys crates link against. Wayland,
# Vulkan and OpenGL are loaded at run time, so they are not needed to build.
set -euo pipefail

sudo=
if [[ $(id -u) != 0 ]]; then sudo=sudo; fi

packages=(
  build-essential
  ca-certificates
  curl
  git
  pkg-config
  libdbus-1-dev
  libfontconfig-dev
  libxcb1-dev
  libxkbcommon-x11-dev
)

install() {
  DEBIAN_FRONTEND=noninteractive $sudo apt-get install -y -qq --no-install-recommends "${packages[@]}"
}

# CI restores the package lists along with the packages from its cache and
# sets APT_LISTS_CACHED=1, so a slow mirror can't stall the job. If the
# cached lists no longer work, update them and try again.
if [[ ${APT_LISTS_CACHED:-} == 1 ]] && install; then
  exit 0
fi
$sudo apt-get update -qq
install
