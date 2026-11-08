#!/usr/bin/env bash
# Build the nestegg rescue stick image (hybrid ISO for USB sticks and DVDs,
# boots in UEFI and BIOS mode) with Debian live-build in a container.
#
# Usage: packaging/rescue/build.sh [path/to/nestegg] [output.iso]
# Needs Docker with --privileged (live-build mounts proc/sys in a chroot).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="$(realpath "${1:-$repo/target/release/nestegg}")"
out="$(realpath -m "${2:-$repo/target/nestegg-rescue-amd64.iso}")"
version="$("$bin" --version | awk '{print $2}')"
# Same pinned keyring as the end-to-end tests.
KEYRING_SHA256=136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45

[[ -x "$bin" ]] || { echo "nestegg binary not found: $bin"; exit 2; }
mkdir -p "$(dirname "$out")"
work="$(mktemp -d)"
trap 'docker run --rm -v "$work:/w" debian:trixie rm -rf /w/build >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
cp -a "$here/live" "$work/build"
install -D -m 0755 "$bin" "$work/build/config/includes.chroot/usr/local/bin/nestegg"
install -D -m 0644 "$repo/LICENSE" "$work/build/config/includes.chroot/usr/local/share/doc/nestegg/LICENSE"

docker run --rm --privileged -v "$work/build:/build" -w /build \
  -e KEYRING_SHA256="$KEYRING_SHA256" -e VERSION="$version" debian:trixie bash -euo pipefail -c '
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq live-build curl ca-certificates >/dev/null
  curl -fsSL -o config/archives/pbs-client.key.chroot \
    https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg
  echo "$KEYRING_SHA256  config/archives/pbs-client.key.chroot" | sha256sum -c - >/dev/null
  cp config/archives/pbs-client.key.chroot config/archives/pbs-client.key.binary
  lb config \
    --distribution trixie \
    --archive-areas "main contrib non-free non-free-firmware" \
    --binary-images iso-hybrid \
    --bootloaders "grub-efi,syslinux" \
    --debian-installer none \
    --apt-recommends false \
    --memtest none \
    --firmware-binary false \
    --firmware-chroot false \
    --iso-application "nestegg rescue $VERSION" \
    --iso-publisher "nestegg" \
    --iso-volume "NESTEGG_RESCUE" \
    --bootappend-live "boot=live components hostname=nestegg-rescue locales=en_US.UTF-8 console=tty0 console=ttyS0,115200n8" \
    >/dev/null
  lb build 2>&1 | tail -n 25
  ls -l *.iso
'
iso="$(ls "$work"/build/*.iso | head -1)"
cp "$iso" "$out"
sha256sum "$out" | sed "s#  .*#  $(basename "$out")#" > "$out.sha256"
echo "built $out"
