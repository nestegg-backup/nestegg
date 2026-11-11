#!/usr/bin/env bash
# Invoked only by the root Rust launcher in new mount/PID/net/UTS namespaces.
set -euo pipefail
base=$1; image=$2; data=$3
mount --make-rprivate /
hostname pbs.example.com
ip link set lo up
mkdir "$base/lower" "$base/ram"
mount -t squashfs -o loop,ro,nodev,nosuid "$image" "$base/lower"
mount -t tmpfs -o mode=0700 tmpfs "$base/ram"
mkdir "$base/ram/upper" "$base/ram/work" "$base/ram/root"
root=$base/ram/root
mount -t overlay overlay -o "lowerdir=$base/lower,upperdir=$base/ram/upper,workdir=$base/ram/work" "$root"
mkdir -p "$root/mnt/stick/nestegg" "$root/etc/proxmox-backup" "$root/run" "$root/dev" "$root/proc" "$root/sys"
mount --bind "$data" "$root/mnt/stick/nestegg"
mount -o remount,bind,nodev,nosuid,noexec "$root/mnt/stick/nestegg"
# live-config normally supplies these only when the ISO boots. A raw squashfs
# has no localhost resolver entry; the manager must reach the local API.
rm -f "$root/etc/hosts" "$root/etc/hostname"
printf '127.0.0.1 localhost pbs.example.com\n::1 localhost ip6-localhost\n' > "$root/etc/hosts"
printf 'pbs.example.com\n' > "$root/etc/hostname"
mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs "$root/run"
mount -t tmpfs -o mode=0700,nosuid,nodev,noexec tmpfs "$root/etc/proxmox-backup"
mount -t proc -o nosuid,nodev,noexec proc "$root/proc"
mount -t tmpfs -o mode=0755,nosuid,noexec tmpfs "$root/dev"
for spec in 'null 1 3 666' 'zero 1 5 666' 'random 1 8 666' 'urandom 1 9 666' 'tty 5 0 666'; do
  read -r name major minor mode <<<"$spec"
  mknod -m "$mode" "$root/dev/$name" c "$major" "$minor"
done
mkdir "$root/dev/pts" "$root/dev/shm"
mount -t devpts -o newinstance,ptmxmode=0666,mode=0620 devpts "$root/dev/pts"
mount -t tmpfs -o mode=1777,nosuid,nodev tmpfs "$root/dev/shm"
ln -s pts/ptmx "$root/dev/ptmx"
ln -s /proc/self/fd "$root/dev/fd"
ln -s fd/0 "$root/dev/stdin"; ln -s fd/1 "$root/dev/stdout"; ln -s fd/2 "$root/dev/stderr"
mount -t sysfs -o ro,nosuid,nodev,noexec sysfs "$root/sys"
cp "$base/server.sh" "$root/run/nestegg-server.sh"
# stdout is the private readiness pipe, stderr a root-private diagnostic log.
exec chroot "$root" /bin/bash /run/nestegg-server.sh
