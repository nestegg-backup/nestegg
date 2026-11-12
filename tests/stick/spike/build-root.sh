#!/usr/bin/env bash
# Build an official PBS root and extract a static client; do not change host apt.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
work=$(realpath "${1:?usage: build-root.sh EXISTING_EMPTY_DIRECTORY}")
[[ -d $work && -z $(ls -A "$work") ]] || { echo 'Need an empty directory' >&2; exit 2; }
tag="nestegg-stick-spike-build-$$"
container=
cleanup() {
  [[ -z $container ]] || docker rm -f "$container" >/dev/null
  docker image rm "$tag" "$tag-tools" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# Keep the same repository and pinned key as the maintained e2e fixture.
docker build --force-rm -t "$tag" -f "$repo/tests/e2e/pbs.Dockerfile" "$repo/tests/e2e" >"$work/build.log" 2>&1
cat >"$work/Dockerfile.tools" <<'DOCKER'
ARG BASE
FROM ${BASE}
RUN rm -f /etc/apt/sources.list.d/pbs-enterprise.sources /etc/apt/sources.list.d/pbs-enterprise.list \
 && apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends iproute2 jq expect strace \
 && printf 'deb [signed-by=/usr/share/keyrings/proxmox-archive-keyring.gpg] http://download.proxmox.com/debian/pbs-client trixie main\n' > /etc/apt/sources.list.d/client.list \
 && apt-get update && mkdir -p /opt/client && cd /opt/client \
 && apt-get download proxmox-backup-client-static && dpkg-deb -x ./*.deb . && rm ./*.deb \
 && rm -rf /var/lib/apt/lists/* \
 && for u in proxmox-backup.service proxmox-backup-proxy.service ssh.service postfix.service; do ln -sf /dev/null /etc/systemd/system/$u; done
DOCKER
docker build --force-rm --build-arg "BASE=$tag" -t "$tag-tools" -f "$work/Dockerfile.tools" "$work" >>"$work/build.log" 2>&1
container=$(docker create --network none "$tag-tools")
mkdir "$work/root" "$work/client"
docker export "$container" | sudo -n tar -C "$work/root" -xf -
sudo -n cp -a "$work/root/opt/client/." "$work/client/"
# Never inherit Docker-generated host identity or network configuration.
printf 'pbs.example.com\n' | sudo -n tee "$work/root/etc/hostname" >/dev/null
printf '127.0.0.1 localhost pbs.example.com\n::1 localhost\n' | sudo -n tee "$work/root/etc/hosts" >/dev/null
sudo -n truncate -s 0 "$work/root/etc/resolv.conf"
sudo -n rm -rf "$work/root/etc/proxmox-backup" "$work/root/opt/client"
sudo -n mkdir -p "$work/root/etc/proxmox-backup" "$work/root/mnt/stick/nestegg"
sudo -n mksquashfs "$work/root" "$work/pbs.squashfs" -noappend -comp zstd -processors 4 | tee "$work/squashfs.log" >/dev/null
sudo -n chroot "$work/root" dpkg-query -W proxmox-backup-server proxmox-backup-client | tee "$work/versions.txt" >/dev/null
sudo -n rm -rf "$work/root"
printf 'Built PBS squashfs and static client in %s\n' "$work"
