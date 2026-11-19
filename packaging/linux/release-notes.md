## Install

Download `hardcover-desktop-<version>-x86_64-linux.tar.gz` and unpack it into `~/.local`:

```sh
tar -xzf hardcover-desktop-*-x86_64-linux.tar.gz -C ~/.local --strip-components=1
```

This installs `~/.local/bin/hardcover-desktop` with its desktop entry, icon and metainfo, so the app appears in your launcher. Installed this way, the app updates itself: it offers each new release and installs it in one click.

To check the download first, verify its signature with [minisign](https://jedisct1.github.io/minisign/):

```sh
minisign -Vm hardcover-desktop-*-x86_64-linux.tar.gz -P RWQmH1HvOAVP+8piXBk3ZC+nqzBVFZFcynEg8NUsgQDCSYYAkrSkPRXW
```

Requires x86_64 Linux with glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 and later), Vulkan or OpenGL graphics drivers, and a Secret Service keyring such as GNOME Keyring or KWallet to store your sign-in.
