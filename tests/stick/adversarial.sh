#!/usr/bin/env bash
# Sourced only by create.sh, with its private mounts and disposable image.
# Every mutation is restored before returning to the normal acceptance flow.
# Variables and helpers are supplied by the calling acceptance fixture.
# shellcheck disable=SC2154
expect_slot() {
    "$bin" stick info "$stick" --test-image > "$work/adversarial-info.json"
    grep -q "\"active_slot\": \"$1\"" "$work/adversarial-info.json"
}
attach
mount -o nodev,nosuid,noexec "${loop}p4" "$work/esp"
cp "$work/esp/live/b/boot.cfg" "$work/saved.cfg"
cp "$work/esp/live/b/commit.sha256" "$work/saved.commit"
cp "$work/esp/nestegg-stick" "$work/saved.marker"
rm "$work/esp/live/b/boot.cfg"
sync "$work/esp/live/b"; umount "$work/esp"; detach
expect_slot a
both missing-descriptor a
attach; mount -o nodev,nosuid,noexec "${loop}p4" "$work/esp"
printf '# torn\n' > "$work/esp/live/b/boot.cfg"
sync "$work/esp/live/b/boot.cfg"; umount "$work/esp"; detach
expect_slot a
attach; mount -o nodev,nosuid,noexec "${loop}p4" "$work/esp"
cp "$work/saved.cfg" "$work/esp/live/b/boot.cfg"
# A self-consistent commit does not make arbitrary GRUB text safe to parse as
# root, nor may metadata claim a slot whose payload is truncated.
printf '\ntouch %s\n' "$work/should-not-exist" >> "$work/esp/live/b/boot.cfg"
printf '%s  /live/b/boot.cfg\n' "$(sha256sum "$work/esp/live/b/boot.cfg" | cut -d' ' -f1)" > "$work/esp/live/b/commit.sha256"
sync "$work/esp/live/b/boot.cfg" "$work/esp/live/b/commit.sha256"
umount "$work/esp"; detach
expect_slot a
[[ ! -e $work/should-not-exist ]]
attach; mount -o nodev,nosuid,noexec "${loop}p4" "$work/esp"
cp "$work/saved.cfg" "$work/esp/live/b/boot.cfg"
cp "$work/saved.commit" "$work/esp/live/b/commit.sha256"
cp "$work/esp/live/b/filesystem.squashfs" "$work/saved.squashfs"
truncate -s 1 "$work/esp/live/b/filesystem.squashfs"
sync "$work/esp/live/b/filesystem.squashfs"; umount "$work/esp"; detach
expect_slot a
both truncated-payload a
attach; mount -o nodev,nosuid,noexec "${loop}p4" "$work/esp"
cp "$work/saved.squashfs" "$work/esp/live/b/filesystem.squashfs"
# An unverified hash file cannot control info/add's reported image hash.
printf '%064d\n' 0 > "$work/esp/live/b/.nestegg-sha256"
sync "$work/esp/live/b/filesystem.squashfs" "$work/esp/live/b/.nestegg-sha256"
umount "$work/esp"; detach
expect_slot b
cmp "$work/info-b.json" "$work/adversarial-info.json"
attach; mount -o nodev,nosuid,noexec "${loop}p2" "$work/esp"
cp "$work/esp/nestegg-stick" "$work/saved.esp-marker"
printf 'unexpected=crafted\n' >> "$work/esp/nestegg-stick"
sync "$work/esp/nestegg-stick"; umount "$work/esp"; detach
if "$bin" stick info "$stick" --test-image > "$work/crafted-marker.log" 2>&1; then exit 1; fi
grep -q 'invalid stick marker' "$work/crafted-marker.log"
attach; mount -o nodev,nosuid,noexec "${loop}p2" "$work/esp"
cp "$work/saved.esp-marker" "$work/esp/nestegg-stick"
# A marker must never supply a format/write destination. Reject a path in
# place of a recorded serial before any medium write, even during update.
sed 's#^slot_a_uuid=.*#slot_a_uuid=/dev/not-a-slot#' "$work/saved.esp-marker" > "$work/esp/nestegg-stick"
sync "$work/esp/nestegg-stick"; umount "$work/esp"; detach
sha256sum "$stick" > "$work/crafted-update-before"
if "$bin" stick update "$stick" --test-image --image "$iso" > "$work/crafted-update.log" 2>&1; then exit 1; fi
grep -q 'invalid stick marker UUID' "$work/crafted-update.log"
sha256sum "$stick" > "$work/crafted-update-after"
cmp "$work/crafted-update-before" "$work/crafted-update-after"
attach; mount -o nodev,nosuid,noexec "${loop}p2" "$work/esp"
cp "$work/saved.esp-marker" "$work/esp/nestegg-stick"
# The root-mounted FAT file has root uid/mode, but is still untrusted code.
printf 'untrusted ISO\n' > "$work/esp/untrusted.iso"
truncate -s 8G "$work/untrusted-target.img"
if "$bin" stick create "$work/untrusted-target.img" --test-image --image "$work/esp/untrusted.iso" --sha256 "$(printf '%064d' 0)" > "$work/fat-input.log" 2>&1; then exit 1; fi
grep -q 'FAT or removable' "$work/fat-input.log"
rm "$work/esp/untrusted.iso"
sync "$work/esp/nestegg-stick"; umount "$work/esp"; detach
# Insufficient INACTIVE partition space is refused before any stick write.
attach; mount -o nodev,nosuid,noexec "${loop}p3" "$work/esp"
remaining=$(( $(stat -f -c %a "$work/esp") * $(stat -f -c %S "$work/esp") - 1048576 ))
head -c "$remaining" /dev/zero > "$work/esp/filler"
sync "$work/esp/filler"; umount "$work/esp"; detach
if "$bin" stick update "$stick" --test-image --image "$iso" > "$work/full-slot.log" 2>&1; then exit 1; fi
grep -q 'not enough free space' "$work/full-slot.log"
expect_slot b
attach; mount -o nodev,nosuid,noexec "${loop}p3" "$work/esp"
rm "$work/esp/filler"
sync "$work/esp"; umount "$work/esp"; detach
# Real UUID collision: by-uuid may expose only one clone; lsblk must find both.
cp --reflink=auto --sparse=always "$stick" "$work/clone.img"
attach
clone_loop=$(losetup --find --show --partscan "$work/clone.img")
udevadm settle --timeout=30 || true
if "$bin" stick info > "$work/cloned-uuid.log" 2>&1; then exit 1; fi
grep -q 'cloned data UUID' "$work/cloned-uuid.log"
losetup -d "$clone_loop"; clone_loop=
detach
rm "$work/clone.img" "$work/saved.squashfs"
# Allocate the CLI's OWN loop, then put it in use before inspect(). This tests
# the real CLI guard, rather than its unrelated pre-existing-loop refusal.
cat > "$work/tools/losetup" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
if [[ $1 == --find ]]; then
    owned=$(/usr/sbin/losetup "$@")
    # The partition nodes appear only after udev has processed the new loop.
    udevadm settle --timeout=30 || true
    for _ in {1..100}; do [[ -b ${owned}p5 ]] && break; sleep 0.1; done
    if [[ $NESTEGG_TEST_GUARD == mount ]]; then
        mount -o nodev,nosuid,noexec "${owned}p5" "$NESTEGG_TEST_GUARD_MOUNT"
    else
        sectors=$(blockdev --getsz "$owned")
        printf '0 %s linear %s 0\n' "$sectors" "$owned" | dmsetup create "$NESTEGG_TEST_HOLDER"
    fi
    printf '%s\n' "$owned"
else
    exec /usr/sbin/losetup "$@"
fi
WRAPPER
chmod +x "$work/tools/losetup"
for mode in mount holder; do
    holder=
    [[ $mode == mount ]] || holder=nestegg-stick-guard-$$
    if PATH="$work/tools:$PATH" NESTEGG_TEST_GUARD="$mode" NESTEGG_TEST_GUARD_MOUNT="$work/guard" NESTEGG_TEST_HOLDER="$holder" "$bin" stick info "$stick" --test-image > "$work/guard-$mode.log" 2>&1; then exit 1; fi
    grep -Eq 'in use|holders|active' "$work/guard-$mode.log"
    if [[ $mode == mount ]]; then umount "$work/guard"; else dmsetup remove "$holder"; fi
    holder=
done
rm "$work/tools/losetup"
echo 'PASS missing/torn/crafted descriptor, truncated payload, forged hash, crafted marker, FAT input, cloned UUID, mounted and held disk guards'
