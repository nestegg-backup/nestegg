#!/usr/bin/env bash
# Boot a disposable GPT disk made from an existing rescue ISO. No real disks.
set -euo pipefail
[[ $EUID == 0 ]] || { echo 'Run with sudo -n' >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
iso=$(realpath "${1:?usage: sudo boot.sh rescue.iso [pbs.squashfs]}")
pbs=${2:-}
# All mounts are private even if a builder exits unexpectedly.
if [[ ${SPIKE_PRIVATE_MOUNTS:-0} != 1 ]]; then
  exec unshare --mount --propagation private env SPIKE_PRIVATE_MOUNTS=1 "$0" "$@"
fi
work=$(mktemp -d /tmp/nestegg-stick-boot.XXXXXX)
loop=; qemu_pid=
cleanup() {
  local status=$? cleanup_failed=0 m log
  trap - EXIT
  trap '' INT TERM
  set +e
  [[ -z $qemu_pid ]] || { kill "$qemu_pid" 2>/dev/null; wait "$qemu_pid" 2>/dev/null; }
  for m in esp root iso; do
    if mountpoint -q "$work/$m"; then umount "$work/$m" || cleanup_failed=1; fi
  done
  if [[ -n $loop && $cleanup_failed == 0 ]]; then losetup -d "$loop" || cleanup_failed=1; fi
  if [[ -n ${KEEP_LOGS:-} ]]; then
    mkdir -p "$KEEP_LOGS" || cleanup_failed=1
    for log in "$work/"*.log; do
      [[ ! -f $log ]] || cp "$log" "$KEEP_LOGS/" || cleanup_failed=1
    done
  fi
  if [[ $cleanup_failed != 0 ]]; then
    echo "ERROR cleanup incomplete; owned resources and files retained at $work" >&2
    exit 1
  fi
  rm -rf "$work" || exit 1
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$work/iso" "$work/root" "$work/esp"
mount -o loop,ro "$iso" "$work/iso"
mount -t squashfs -o loop,ro "$work/iso/live/filesystem.squashfs" "$work/root"
truncate -s 6G "$work/stick.img"
sgdisk --clear --new=1:2048:+1M --typecode=1:ef02 --change-name=1:BIOS \
  --new=2:0:+1536M --typecode=2:ef00 --change-name=2:ESP \
  --new=3:0:0 --typecode=3:8300 --change-name=3:NESTEGG_DATA "$work/stick.img" >/dev/null
loop=$(losetup --find --show --partscan "$work/stick.img")
mkfs.vfat -F32 -n NESTEGGBOOT "${loop}p2" >/dev/null
mkfs.ext4 -q -m 0 -L NESTEGG_DATA "${loop}p3"
mount "${loop}p2" "$work/esp"
cp -a "$work/iso/live" "$work/esp/live"
if [[ -n $pbs ]]; then
  mkdir "$work/esp/live/spike"
  cp "$pbs" "$work/esp/live/spike/pbs.squashfs"
  if [[ -x $(dirname "$pbs")/client/usr/bin/proxmox-backup-client ]]; then
    cp "$(dirname "$pbs")/client/usr/bin/proxmox-backup-client" "$work/esp/live/spike/"
  fi
  cp "$here/namespace.sh" "$here/server.sh" "$here/vm-probe.sh" "$work/esp/live/spike/"
fi
uuid=$(blkid -s UUID -o value "${loop}p2")
grub-install --target=i386-pc --directory="$work/root/usr/lib/grub/i386-pc" \
  --boot-directory="$work/esp/boot" --recheck "$loop" >"$work/grub-bios.log" 2>&1
grub-install --target=x86_64-efi --directory="$work/root/usr/lib/grub/x86_64-efi" \
  --efi-directory="$work/esp" --boot-directory="$work/esp/boot" --removable --no-nvram >"$work/grub-uefi.log" 2>&1
kernel=$(basename "$(find "$work/esp/live" -maxdepth 1 -name 'vmlinuz*' | head -1)")
initrd=$(basename "$(find "$work/esp/live" -maxdepth 1 -name 'initrd*' | head -1)")
cat >"$work/esp/boot/grub/grub.cfg" <<CFG
serial --speed=115200
terminal_input serial
terminal_output serial
set timeout=1
menuentry 'nestegg spike' {
  search --no-floppy --fs-uuid --set=root $uuid
  linux /live/$kernel boot=live components live-media=/dev/disk/by-uuid/$uuid hostname=desktop.example.com locales=en_US.UTF-8 console=ttyS0,115200n8
  initrd /live/$initrd
}
CFG
sync
umount "$work/esp"
losetup -d "$loop"; loop=
pmbr_flag=$(od -An -tu1 -j446 -N1 "$work/stick.img" | tr -d ' ')
[[ $pmbr_flag == 0 ]] || { echo 'ERROR unexpected protective MBR boot flag' >&2; exit 1; }
cat >"$work/steps" <<'STEPS'
expect root@.*# 180
send findmnt -rn /run/live/medium; test -s /run/live/medium/live/filesystem.squashfs && printf 'STICK-%s\n' MEDIUM-OK
expect STICK-MEDIUM-OK 30
STEPS
if [[ -n $pbs ]]; then
  printf '%s\n' 'send bash /run/live/medium/live/spike/vm-probe.sh' 'expect VM-PROBE-DONE 120' >>"$work/steps"
fi
printf '%s\n' 'send poweroff -f' >>"$work/steps"
boot() {
  local mode=$1; shift
  qemu-system-x86_64 -enable-kvm -cpu host -m 2048 -smp 2 -display none -monitor none -nic none \
    -drive "file=$work/stick.img,format=raw,if=virtio" \
    -serial "unix:$work/console.sock,server=on,wait=on" "$@" >"$work/qemu-$mode.log" 2>&1 &
  qemu_pid=$!
  if timeout 240 python3 "$here/../../rescue/console.py" "$work/console.sock" "$work/boot-$mode.log" "$work/steps"; then
    wait "$qemu_pid"; qemu_pid=
    grep -q '^STICK-MEDIUM-OK' "$work/boot-$mode.log"
    if [[ -n $pbs ]]; then
      if [[ $mode == bios-ipv6-disabled ]]; then
        grep -q '^VM-PBS-FAILED' "$work/boot-$mode.log"
        grep -q 'Address family not supported' "$work/boot-$mode.log"
      else
        grep -q '^VM-PBS-STARTED' "$work/boot-$mode.log"
        grep -q '^VM-BACKUP-RESTORE-OK' "$work/boot-$mode.log"
      fi
    fi
    echo "PASS $mode: live medium found by UUID; expected PBS probe result"
  else
    kill "$qemu_pid" 2>/dev/null || true; wait "$qemu_pid" || true; qemu_pid=
    return 1
  fi
  rm -f "$work/console.sock"
}
# A plain GPT protective MBR first: do not set pmbr_boot preemptively.
boot bios
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars.fd"
boot uefi -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive "if=pflash,format=raw,file=$work/vars.fd"
echo 'PASS pmbr_boot not required by SeaBIOS'
if [[ -n $pbs ]]; then
  loop=$(losetup --find --show --partscan "$work/stick.img")
  mount "${loop}p2" "$work/esp"
  sed -i 's/boot=live components/boot=live components ipv6.disable=1/' "$work/esp/boot/grub/grub.cfg"
  umount "$work/esp"
  losetup -d "$loop"; loop=
  boot bios-ipv6-disabled
fi
