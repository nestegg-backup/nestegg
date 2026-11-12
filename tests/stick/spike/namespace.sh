#!/usr/bin/env bash
# Internal namespace PID 1. All writable PBS paths live in a tmpfs overlay.
set -euo pipefail
base=${1:?}; data=${2:?}; scripts=${3:?}; ipv6=${4:-normal}
mount --make-rprivate /
hostname pbs.example.com
ip link set lo up
if [[ $ipv6 == disabled ]]; then
  sysctl -qw net.ipv6.conf.all.disable_ipv6=1 net.ipv6.conf.default.disable_ipv6=1 net.ipv6.conf.lo.disable_ipv6=1
fi
mount -t tmpfs -o mode=0700,size=2G tmpfs "$base/runtime"
mkdir -p "$base/runtime/upper" "$base/runtime/work" "$base/runtime/root"
root=$base/runtime/root
mount -t overlay overlay -o "lowerdir=$base/lower,upperdir=$base/runtime/upper,workdir=$base/runtime/work" "$root"
mount --bind "$data/nestegg" "$root/mnt/stick/nestegg"
mount -t tmpfs -o mode=0755 tmpfs "$root/run"
mount -t tmpfs -o mode=0700 tmpfs "$root/etc/proxmox-backup"
mount -t proc proc "$root/proc"
mount -t tmpfs -o mode=0755,nosuid tmpfs "$root/dev"
for spec in 'null 1 3 666' 'zero 1 5 666' 'random 1 8 666' 'urandom 1 9 666' 'tty 5 0 666'; do
  read -r name major minor mode <<<"$spec"
  mknod -m "$mode" "$root/dev/$name" c "$major" "$minor"
done
mkdir "$root/dev/pts" "$root/dev/shm"
mount -t devpts -o newinstance,ptmxmode=0666,mode=0620 devpts "$root/dev/pts"
ln -s pts/ptmx "$root/dev/ptmx"
ln -s /proc/self/fd "$root/dev/fd"
ln -s fd/0 "$root/dev/stdin"; ln -s fd/1 "$root/dev/stdout"; ln -s fd/2 "$root/dev/stderr"
mount -t sysfs -o ro,nosuid,nodev,noexec sysfs "$root/sys"
cp "$scripts/server.sh" "$root/run/spike-server.sh"
# The signal reaches namespace PID 1; killing it also kills its descendants.
exec chroot "$root" /bin/bash /run/spike-server.sh
