#!/usr/bin/env bash
# Real dm-log-writes replay on disposable image files. No physical media.
# Usage: sudo -n durability.sh BINARY ISO /path/to/replay-log
# replay-log upstream: josefbacik/log-writes, commit
# 7b70d8a6863c5de30933d42a7672d35d01d2dc6c (build outside this repository).
set -Eeuo pipefail
[[ $EUID == 0 ]] || exit 2
bin=$(realpath "${1:?binary required}")
iso=$(realpath "${2:?ISO required}")
replay=$(realpath "${3:?dm-log-writes replay-log tool required}")
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
if [[ ${NESTEGG_REPLAY_PRIVATE:-0} != 1 ]]; then
    exec unshare --mount --propagation private env NESTEGG_REPLAY_PRIVATE=1 "$0" "$@"
fi
work=$(mktemp -d /tmp/nestegg-stick-replay.XXXXXX)
origin=; payload=; journal=; cut_loop=
prefix=nestegg-replay-$$
cleanup() {
    local status=$? failed=0 item
    trap - EXIT; trap '' INT TERM
    if mountpoint -q "$work/esp"; then umount "$work/esp" || failed=1; fi
    for item in "${prefix}-disk2" "${prefix}-disk3" "${prefix}-disk"; do
        if dmsetup info "$item" >/dev/null 2>&1; then dmsetup remove "$item" || failed=1; fi
    done
    for item in "$cut_loop" "$journal" "$payload" "$origin"; do
        [[ -z $item ]] || losetup -d "$item" || failed=1
    done
    if ((failed)); then echo "ERROR: replay cleanup incomplete: $work" >&2; exit 1; fi
    if losetup -ln -O BACK-FILE | grep -Fq "$work/"; then echo "ERROR: replay loop retained: $work" >&2; exit 1; fi
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
trap 'echo "ERROR: replay acceptance failed at line $LINENO" >&2' ERR
trap 'exit 130' INT
trap 'exit 143' TERM
# The caller loads/unloads the kernel module; do not change host module state.
dmsetup targets | grep -q '^log-writes ' || { echo 'Load dm-log-writes before running replay acceptance' >&2; exit 2; }
mkdir "$work/esp" "$work/tools" "$work/state" "$work/runtime"
chmod 0700 "$work/state" "$work/runtime"
cp --reflink=auto "$iso" "$work/release.iso"
cp "$iso.sha256" "$work/release.iso.sha256"
chmod 0600 "$work/release.iso" "$work/release.iso.sha256"
iso=$work/release.iso
export NESTEGG_CONFIG="$work/config.toml" NESTEGG_STATE_DIR="$work/state" NESTEGG_RUNTIME_DIR="$work/runtime"
truncate -s 8G "$work/stick.img"
python3 "$here/confirm.py" "$bin" stick create "$work/stick.img" --test-image --image "$iso" > "$work/create.log" 2>&1
"$bin" stick update "$work/stick.img" --test-image --image "$iso" > "$work/update.log" 2>&1
origin=$(losetup --find --show --partscan "$work/stick.img")
mount -o nodev,nosuid,noexec "${origin}p2" "$work/esp"
# Model a prior interrupted inactive write. New payload bytes MUST be durable;
# a same-release update over identical old bytes would hide reordering bugs.
rm "$work/esp/live/a/commit.sha256"
old_size=$(stat -c %s "$work/esp/live/a/filesystem.squashfs")
dd if=/dev/zero of="$work/esp/live/a/filesystem.squashfs" bs=1M count=$(((old_size+1048575)/1048576)) conv=notrunc status=none
truncate -s "$old_size" "$work/esp/live/a/filesystem.squashfs"
sync "$work/esp/live/a/filesystem.squashfs" "$work/esp/live/a"
umount "$work/esp"
dd if="${origin}p2" of="$work/baseline.img" bs=4M conv=sparse status=none
cp --reflink=auto --sparse=always "$work/baseline.img" "$work/payload.img"
truncate -s 2G "$work/journal.img"
payload=$(losetup --find --show "$work/payload.img")
journal=$(losetup --find --show "$work/journal.img")
sectors=$(blockdev --getsz "$origin")
esp_sectors=$(blockdev --getsz "$payload")
data_start=$(lsblk -nr -o START "${origin}p3")
data_sectors=$(blockdev --getsz "${origin}p3")
dmsetup create "${prefix}-disk" --table "0 $sectors linear $origin 0"
dmsetup create "${prefix}-disk2" --table "0 $esp_sectors log-writes $payload $journal"
dmsetup create "${prefix}-disk3" --table "0 $data_sectors linear $origin $data_start"
# Negative control on the actual vfat/logged device: syncfs writes dirty data
# but must not satisfy the file-fsync device-flush assertion used below.
mount -o nodev,nosuid,noexec "/dev/mapper/${prefix}-disk2" "$work/esp"
printf 'flush control\n' > "$work/esp/flush-control"
dmsetup message "${prefix}-disk2" 0 'mark syncfs-before'
sync -f "$work/esp/flush-control"
dmsetup message "${prefix}-disk2" 0 'mark syncfs-after'
sync "$work/esp/flush-control"
dmsetup message "${prefix}-disk2" 0 'mark control-fsynced'
rm "$work/esp/flush-control"
sync "$work/esp"
umount "$work/esp"
cat > "$work/tools/sync" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  *"/esp"*)
    n=$(cat "$NESTEGG_REPLAY_COUNTER"); n=$((n+1)); echo "$n" > "$NESTEGG_REPLAY_COUNTER"
    printf '%s %s\n' "$n" "${1##*/}" >> "$NESTEGG_REPLAY_LABELS"
    dmsetup message "$NESTEGG_REPLAY_MAP" 0 "mark before-$n"
    /usr/bin/sync "$@"
    dmsetup message "$NESTEGG_REPLAY_MAP" 0 "mark after-$n"
    ;;
  *) exec /usr/bin/sync "$@" ;;
esac
WRAPPER
chmod +x "$work/tools/sync"
echo 0 > "$work/counter"
mkdir "$work/operation"
PATH="$work/tools:$PATH" NESTEGG_REPLAY_LABELS="$work/labels" NESTEGG_REPLAY_COUNTER="$work/counter" NESTEGG_REPLAY_MAP="${prefix}-disk2" \
    unshare --mount --propagation private bash "$repo/crates/nestegg/src/stick/media.sh" update \
    "/dev/mapper/${prefix}-disk" "$iso" "$work/state/stick" "$work/operation" 0 > "$work/result.json" 2> "$work/operation.log"
dmsetup message "${prefix}-disk2" 0 'mark done'
dmsetup remove "${prefix}-disk2"
legacy_before=$("$replay" --log "$work/journal.img" --find --end-mark syncfs-before)
legacy_after=$("$replay" --log "$work/journal.img" --find --end-mark syncfs-after)
control_after=$("$replay" --log "$work/journal.img" --find --end-mark control-fsynced)
flush=$("$replay" --log "$work/journal.img" --find --next-flush --start-entry "$((legacy_before+1))")
((flush >= legacy_after && flush < control_after)) || { echo 'ERROR: syncfs/fsync negative control did not distinguish device flushes' >&2; exit 1; }
# These are real device flushes, not just fsync function calls. A regression to
# sync -f on vfat has no flush between the before/after marks and fails here.
for name in filesystem.squashfs boot.cfg commit.sha256; do
    n=$(awk -v name="$name" '$2 == name {print $1}' "$work/labels")
    [[ $n =~ ^[0-9]+$ ]]
    before=$("$replay" --log "$work/journal.img" --find --end-mark "before-$n")
    after=$("$replay" --log "$work/journal.img" --find --end-mark "after-$n")
    flush=$("$replay" --log "$work/journal.img" --find --next-flush --start-entry "$((before+1))")
    ((flush < after)) || { echo "ERROR: no device flush for $name" >&2; exit 1; }
done
# Replay every flush/FUA boundary and both sides of every helper fsync batch.
# A published descriptor must ALWAYS have a complete payload, even before the
# final commit fsync returns. The previous committed payload must remain intact.
"$replay" --log "$work/journal.img" --num-entries > "$work/entries"
: > "$work/cuts"
for flag in --next-flush --next-fua; do
    start=0
    while entry=$("$replay" --log "$work/journal.img" --find "$flag" --start-entry "$start" 2>/dev/null); do
        echo "$((entry+1))" >> "$work/cuts"
        start=$((entry+1))
    done
done
for ((n=1;n<=$(cat "$work/counter");n++)); do
    for side in before after; do
        entry=$("$replay" --log "$work/journal.img" --find --end-mark "$side-$n")
        echo "$((entry+1))" >> "$work/cuts"
    done
done
cat "$work/entries" >> "$work/cuts"
sort -nu "$work/cuts" -o "$work/cuts"
count=0; published=0
while read -r limit; do
    cp --reflink=auto --sparse=always "$work/baseline.img" "$work/cut.img"
    cut_loop=$(losetup --find --show "$work/cut.img")
    "$replay" --log "$work/journal.img" --replay "$cut_loop" --limit "$limit" --no-discard
    mount -o ro,nodev,nosuid,noexec "$cut_loop" "$work/esp"
    # Previous slot: descriptor AND payload stay valid at every cut.
    for slot in b a; do
        dest=$work/esp/live/$slot
        if [[ ! -f $dest/commit.sha256 ]]; then [[ $slot == a ]]; continue; fi
        expected="$(sha256sum "$dest/boot.cfg" | cut -d' ' -f1)  /live/$slot/boot.cfg"
        if [[ $(cat "$dest/commit.sha256") != "$expected" ]]; then [[ $slot == a ]]; continue; fi
        kernel=$(sed -n 's/^# nestegg-kernel=//p' "$dest/boot.cfg")
        initrd=$(sed -n 's/^# nestegg-initrd=//p' "$dest/boot.cfg")
        actual=$(cd "$dest" && sha256sum filesystem.squashfs "$kernel" "$initrd" | sed "s#  #  /live/$slot/#")
        [[ $(cat "$dest/payload.sha256") == "$actual" ]] || { echo "ERROR: committed incomplete slot $slot at entry $limit" >&2; exit 1; }
        [[ $slot != a ]] || published=$((published+1))
    done
    umount "$work/esp"
    losetup -d "$cut_loop"; cut_loop=
    count=$((count+1))
done < "$work/cuts"
((count > 0 && published > 0))
echo "PASS dm-log-writes: $count flush/FUA/fsync-boundary replay cuts, $published with complete new commit; previous slot intact"
