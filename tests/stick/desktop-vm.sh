#!/usr/bin/env bash
# Real systemd/udev add/remove in a disposable KVM guest. No host config changes.
# Usage: sudo desktop-vm.sh BINARY RESCUE_ISO TRUSTED_PBS_SQUASHFS STATIC_CLIENT
set -Eeuo pipefail
[[ $EUID == 0 ]] || { echo 'Run with sudo -n' >&2; exit 2; }
if [[ ${NESTEGG_STICK_VM_NS:-0} != 1 ]]; then
    exec unshare --mount --propagation private env NESTEGG_STICK_VM_NS=1 "$0" "$@"
fi
bin=$(realpath "${1:?binary}"); iso=$(realpath "${2:?ISO}")
image=$(realpath "${3:?PBS squashfs}"); client=$(realpath "${4:?client}")
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d /tmp/nestegg-desktop-vm.XXXXXX)
pid=
cleanup() {
    local status=$?
    trap - EXIT
    [[ -z $pid ]] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
    if mountpoint -q "$work/iso"; then umount "$work/iso" || exit 1; fi
    if [[ -n ${KEEP_LOGS:-} ]]; then mkdir -p "$KEEP_LOGS"; cp "$work/"*.log "$KEEP_LOGS/"; fi
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$work/iso" "$work/share"
mount -o loop,ro,nodev,nosuid,noexec "$iso" "$work/iso"
kernel=$(rg --files "$work/iso/live" | rg '/vmlinuz[^/]*$' | head -1)
initrd=$(rg --files "$work/iso/live" | rg '/initrd[^/]*$' | head -1)
cp "$kernel" "$work/kernel"; cp "$initrd" "$work/initrd"
umount "$work/iso"
cp "$bin" "$work/share/nestegg"
cp "$image" "$work/share/pbs.squashfs"
cp "$client" "$work/share/client"
cp "$here/hotplug-vm.sh" "$work/share/"
cp "$here/../../packaging/systemd/nestegg-stick-pbs@.service" "$work/share/"
cp "$here/../../packaging/udev/70-nestegg-stick.rules" "$work/share/"
cat >"$work/steps" <<'STEPS'
expect root@.*# 240
send mkdir -p /mnt/fixture; mount -t 9p -o trans=virtio,ro fixture /mnt/fixture; touch /run/nestegg-disposable-vm; bash -x /mnt/fixture/hotplug-vm.sh >/run/hotplug.log 2>&1; result=$?; cat /run/hotplug.log; printf 'VM-FINISHED-%s\n' "$result"
expect VM-FINISHED-[0-9] 300
send poweroff -f
STEPS
qemu-system-x86_64 -enable-kvm -cpu host -m 4096 -smp 2 -display none -monitor none -nic none \
    -kernel "$work/kernel" -initrd "$work/initrd" \
    -append 'boot=live components hostname=desktop.example.com locales=en_US.UTF-8 console=ttyS0,115200n8' \
    -cdrom "$iso" -virtfs "local,path=$work/share,mount_tag=fixture,security_model=none,readonly=on" \
    -serial "unix:$work/console.sock,server=on,wait=on" >"$work/qemu.log" 2>&1 &
pid=$!
timeout 600 python3 "$here/../rescue/console.py" "$work/console.sock" "$work/console.log" "$work/steps"
wait "$pid"; pid=
grep -q 'VM-FINISHED-0' "$work/console.log"
grep -q 'VM-HOTPLUG-PASS' "$work/console.log"
echo 'PASS real udev plug-in, bound systemd service and partition automounter flags in disposable KVM'
