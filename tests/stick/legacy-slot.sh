#!/usr/bin/env bash
# Unreleased single-FAT layout: refuse all readers/writers, never migrate.
set -Eeuo pipefail
bin=$1; iso=$2
work=$(mktemp -d /tmp/nestegg-legacy-slot.XXXXXX)
loop=
cleanup() {
    local rc=$?
    trap - EXIT
    if mountpoint -q "$work/esp"; then umount "$work/esp" || exit 1; fi
    [[ -z $loop ]] || losetup -d "$loop"
    rm -rf "$work"
    exit "$rc"
}
trap cleanup EXIT
trap 'echo "ERROR: legacy refusal test failed at line $LINENO" >&2; tail -12 "$work/"*.log 2>/dev/null || true' ERR
truncate -s 2G "$work/legacy.img"
loop=$(losetup --find --show --partscan "$work/legacy.img")
sgdisk --clear --new=1:2048:+1M --typecode=1:ef02 --new=2:0:+64M --typecode=2:ef00 --new=3:0:0 --typecode=3:8300 "$loop" >/dev/null
blockdev --rereadpt "$loop"
udevadm settle --timeout=30 || true
mkfs.vfat -F32 "${loop}p2" >/dev/null
mkfs.ext4 -q -F -m 0 -L NESTEGG_DATA "${loop}p3"
mkdir "$work/esp"
mount -o nodev,nosuid,noexec "${loop}p2" "$work/esp"
printf 'nestegg-stick-v1\nboot_uuid=%s\ndata_uuid=%s\n' "$(blkid -p -s UUID -o value "${loop}p2")" "$(blkid -p -s UUID -o value "${loop}p3")" > "$work/esp/nestegg-stick"
sync "$work/esp/nestegg-stick" "$work/esp"
umount "$work/esp"
losetup -d "$loop"; loop=
sha256sum "$work/legacy.img" > "$work/before"
for action in info update add; do
    args=()
    [[ $action == info ]] || args=(--image "$iso")
    if "$bin" stick "$action" "$work/legacy.img" --test-image "${args[@]}" > "$work/$action.log" 2>&1; then exit 1; fi
    grep -q 'old single-FAT stick layout: create the stick again; this erases all backups' "$work/$action.log"
done
sha256sum "$work/legacy.img" > "$work/after"
cmp "$work/before" "$work/after"
echo 'PASS v1 info/update/add require recreation and warn about erasing backups; no medium writes'
