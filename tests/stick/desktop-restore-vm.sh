#!/usr/bin/env bash
# Desktop restore API acceptance in a disposable KVM guest; no host users/devices changed.
# Usage: sudo desktop-restore-vm.sh BINARY RESCUE_ISO TRUSTED_PBS_SQUASHFS STATIC_CLIENT
set -Eeuo pipefail
[[ $EUID == 0 && -e /dev/kvm ]] || { echo 'root and KVM required' >&2; exit 2; }
bin=$(realpath "${1:?binary}"); iso=$(realpath "${2:?rescue ISO}")
image=$(realpath "${3:?PBS squashfs}"); client=$(realpath "${4:?official static client}")
here=$(cd "$(dirname "$0")" && pwd)
if [[ ${NESTEGG_RESTORE_VM_NS:-0} != 1 ]]; then
    exec unshare --mount --propagation private env NESTEGG_RESTORE_VM_NS=1 "$0" "$@"
fi
work=$(mktemp -d /tmp/nestegg-restore-api-vm.XXXXXX)
pid=
cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n $pid ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    if mountpoint -q "$work/iso"; then umount "$work/iso" || exit 1; fi
    if [[ -n ${KEEP_LOGS:-} ]]; then mkdir -p "$KEEP_LOGS"; cp "$work/"*.log "$KEEP_LOGS/"; fi
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
trap 'tail -60 "$work/console.log" 2>/dev/null || true' ERR
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$work/iso" "$work/share"
mount -o loop,ro,nodev,nosuid,noexec "$iso" "$work/iso"
cp "$(find "$work/iso/live" -maxdepth 1 -name 'vmlinuz*' -print -quit)" "$work/kernel"
cp "$(find "$work/iso/live" -maxdepth 1 -name 'initrd*' -print -quit)" "$work/initrd"
umount "$work/iso"
cp "$bin" "$work/share/nestegg"
cp "$image" "$work/share/pbs.squashfs"
cp "$client" "$work/share/client"
cp "$here/desktop-restore.py" "$work/share/"
cat > "$work/share/guest.sh" <<'GUEST'
#!/bin/bash
set -Eeuo pipefail
unset SUDO_UID SUDO_GID SUDO_USER CREDENTIALS_DIRECTORY
work=/run/desktop-fixture
mkdir -p "$work" /var/lib/nestegg /run/api-credentials /run/desktop-api
mount -t tmpfs -o mode=0755 tmpfs /var/lib/nestegg
mkdir /var/lib/nestegg/stick
cp /mnt/fixture/nestegg /usr/local/bin/nestegg-api-fixture
cp /mnt/fixture/client /usr/local/bin/client-api-fixture-real
cat > /usr/local/bin/client-api-fixture <<'CLIENT'
#!/bin/bash
if [[ $1 == catalog && $2 == dump && -e /run/desktop-api-gate/pause ]]; then
    touch /run/desktop-api-gate/busy
    while [[ -e /run/desktop-api-gate/pause ]]; do sleep .1; done
fi
exec /usr/local/bin/client-api-fixture-real "$@"
CLIENT
chmod +x /usr/local/bin/nestegg-api-fixture /usr/local/bin/client-api-fixture /usr/local/bin/client-api-fixture-real
cp /mnt/fixture/pbs.squashfs /var/lib/nestegg/stick/pbs.squashfs
sha256sum /var/lib/nestegg/stick/pbs.squashfs | cut -d' ' -f1 > /var/lib/nestegg/stick/pbs.squashfs.sha256
printf 'label: gpt\n, , L\n' | sfdisk /dev/vda
partprobe /dev/vda
for _ in {1..50}; do [[ -b /dev/vda1 ]] && break; sleep .1; done
mkfs.ext4 -q -m 0 /dev/vda1
uuid=$(blkid -p -s UUID -o value /dev/vda1)
mkdir -p /dev/disk/by-uuid "$work/data"
[[ -e /dev/disk/by-uuid/$uuid ]] || ln -s /dev/vda1 "/dev/disk/by-uuid/$uuid"
mount -o nodev,nosuid,noexec /dev/vda1 "$work/data"
mkdir "$work/data/nestegg"
umount "$work/data"
groupadd -f nestegg
useradd -m -G nestegg alice
printf 'hello from alice\n' > /home/alice/notes.txt
chown alice:alice /home/alice/notes.txt
mkdir /home/alice/Documents
printf 'document from alice\n' > /home/alice/Documents/report.txt
chown -R alice:alice /home/alice/Documents
mkdir /run/desktop-api-gate
chown alice:alice /run/desktop-api-gate
export NESTEGG_TEST_BROWSE_GATE=/run/desktop-api-gate
# Only the fixture home is backed up. Source metadata remains '/' so the
# non-root API can verify the real /home/alice scope of this archive.
python3 - "$uuid" > "$work/config.toml" <<'PY'
import json, pathlib, sys
excludes = [str(p) for p in pathlib.Path('/').iterdir() if p.name != 'home']
print('backup_id = "desktop"\n[stick]\ndata_uuids = ' + json.dumps([sys.argv[1]]))
print('minimum_free_bytes = 1048576\n[backup]\nsystem = false\ninventory = false')
print('sources = [{archive="system", path="/"}, {archive="docs", path="/home/alice/Documents"}]\nexcludes = ' + json.dumps(excludes))
print('[notify]\ndesktop = false')
PY
export NESTEGG_CONFIG="$work/config.toml" NESTEGG_RUNTIME_DIR=/run/desktop-api NESTEGG_STATE_DIR=/var/lib/nestegg NESTEGG_CREDENTIALS_DIR=/run/api-credentials NESTEGG_PBS_CLIENT=/usr/local/bin/client-api-fixture NESTEGG_SYSTEMD_CREDS="$work/no-creds" NESTEGG_SYSTEMCTL="$work/no-systemctl"
chmod 700 /run/api-credentials /run/desktop-api
/usr/local/bin/nestegg-api-fixture key create --allow-plain
openssl rand -base64 32 > "$work/password"
/usr/local/bin/nestegg-api-fixture stick key-copy --uuid "$uuid" --password-fd 7 7<"$work/password"
/usr/local/bin/nestegg-api-fixture run --force --target stick --stick-uuid "$uuid"
chmod 0755 /run/desktop-api
/usr/local/bin/nestegg-api-fixture daemon > /run/api-daemon.log 2>&1 &
api_pid=$!
trap 'kill "$api_pid" 2>/dev/null || true; wait "$api_pid" 2>/dev/null || true' EXIT
for _ in {1..100}; do [[ -S /run/desktop-api/api.sock ]] && break; sleep .1; done
runuser -u alice -- python3 /mnt/fixture/desktop-restore.py
GUEST
cat > "$work/steps" <<'STEPS'
expect root@.*# 240
send mkdir -p /mnt/fixture; mount -t 9p -o trans=virtio,ro fixture /mnt/fixture; bash /mnt/fixture/guest.sh >/run/accept.log 2>&1; result=$?; cat /run/accept.log; printf 'RESTORE-API-FINISHED-%s\n' "$result"
expect RESTORE-API-FINISHED-[0-9] 600
send poweroff -f
STEPS
truncate -s 3G "$work/stick.raw"
qemu-system-x86_64 -enable-kvm -cpu host -m 4096 -smp 2 -display none -monitor none -nic none \
    -kernel "$work/kernel" -initrd "$work/initrd" \
    -append 'boot=live components hostname=desktop.example.com locales=en_US.UTF-8 console=ttyS0,115200n8' \
    -drive "file=$work/stick.raw,format=raw,if=virtio" -cdrom "$iso" \
    -virtfs "local,path=$work/share,mount_tag=fixture,security_model=none,readonly=on" \
    -serial "unix:$work/console.sock,server=on,wait=on" > "$work/qemu.log" 2>&1 &
pid=$!
timeout 900 python3 "$here/../rescue/console.py" "$work/console.sock" "$work/console.log" "$work/steps"
wait "$pid"; pid=
grep -q 'RESTORE-API-FINISHED-0' "$work/console.log"
echo 'PASS desktop stick restore API in disposable KVM guest'
