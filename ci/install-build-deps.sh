#!/usr/bin/env bash
# Installs what building the workspace needs on Ubuntu 22.04 or newer: a C
# toolchain and the system libraries the -sys crates link against. Wayland,
# Vulkan and OpenGL are loaded at run time, so they are not needed to build.
set -euo pipefail

sudo=
if [[ $(id -u) != 0 ]]; then sudo=sudo; fi

$sudo apt-get update -qq
DEBIAN_FRONTEND=noninteractive $sudo apt-get install -y -qq --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  git \
  pkg-config \
  libdbus-1-dev \
  libfontconfig-dev \
  libxcb1-dev \
  libxkbcommon-x11-dev
