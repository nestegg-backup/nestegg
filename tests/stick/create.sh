#!/usr/bin/env bash
# Phase-one acceptance: ONLY disposable regular image files and owned loops.
# Usage: sudo -n tests/stick/create.sh BINARY VERIFIED_ISO [LAUNCHER_TEST_BINARY]
# Build the integration binary first: cargo test --test stick_launcher --no-run
set -Eeuo pipefail
[[ $EUID == 0 ]] || { echo 'Run with sudo -n' >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
bin=$(realpath "${1:?usage: create.sh BINARY ISO [LAUNCHER_TEST_BINARY]}")
iso=$(realpath "${2:?ISO required}")
probe=${3:-}
if [[ -z $probe ]]; then
    # Newer cargo versions keep test binaries outside target/debug/deps; take
    # the newest build wherever it is.
    probe=$(find "$repo/target/debug" -type f -name 'stick_launcher-*' -executable -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -1 | cut -d' ' -f2-)
fi
[[ -x $probe ]] || { echo 'Build cargo test --test stick_launcher --no-run first' >&2; exit 2; }
probe=$(realpath "$probe")
if [[ ${NESTEGG_STICK_PRIVATE_MOUNTS:-0} != 1 ]]; then
    exec unshare --mount --propagation private env NESTEGG_STICK_PRIVATE_MOUNTS=1 "$0" "$@"
fi
work=$(mktemp -d /tmp/nestegg-stick-test.XXXXXX)
loop=; clone_loop=; holder=; qemu_pid=; update_pid=
cleanup() {
    local status=$? failed=0 point
    trap - EXIT; trap '' INT TERM
    set +e
    if [[ -n $update_pid ]]; then kill -TERM "$update_pid" 2>/dev/null; wait "$update_pid" 2>/dev/null; fi
    if [[ -n $qemu_pid ]]; then kill "$qemu_pid" 2>/dev/null; wait "$qemu_pid" 2>/dev/null; fi
    [[ -z $holder ]] || dmsetup remove "$holder" || failed=1
    for point in data esp iso root guard; do
        if mountpoint -q "$work/$point"; then umount "$work/$point" || failed=1; fi
    done
    [[ -z $clone_loop ]] || losetup -d "$clone_loop" || failed=1
    umount /var/lib/nestegg 2>/dev/null || true
    umount /var/lib 2>/dev/null || true
    if [[ -n $loop && $failed == 0 ]]; then losetup -d "$loop" || failed=1; fi
    if [[ -n ${KEEP_LOGS:-} ]]; then
        mkdir -p "$KEEP_LOGS" || failed=1
        for log in "$work/"*.log; do [[ ! -f $log ]] || cp "$log" "$KEEP_LOGS/" || failed=1; done
    fi
    if ((failed)); then echo "ERROR: cleanup incomplete; retained owned resources at $work" >&2; exit 1; fi
    # Check there are no loops or processes that still refer to our private path.
    if losetup -l -n -O BACK-FILE | grep -Fq "$work/"; then
        echo "ERROR: owned loop remains at $work" >&2; exit 1
    fi
    rm -rf "$work"
    exit "$status"
}
trap 'echo "ERROR: stick acceptance failed at line $LINENO" >&2' ERR
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
"$here/grub-config.sh"
mkdir -p "$work/"{data,esp,iso,root,state,runtime,home,guard,tools}
# All fixed cache paths and NSS fixtures exist only in this mount namespace.
mount -t tmpfs -o mode=0755 tmpfs /var/lib
mkdir /var/lib/nestegg
mount --bind "$work/state" /var/lib/nestegg
cp /etc/group "$work/group"
if ! getent group nestegg >/dev/null; then printf 'nestegg:x:59000:\n' >> "$work/group"; fi
mount --bind "$work/group" /etc/group
# Copy release inputs into root-owned protected storage; user-owned downloads
# deliberately fail the privileged CLI's new provenance guard.
cp --reflink=auto "$iso" "$work/release.iso"
cp "$iso.sha256" "$work/release.iso.sha256"
chmod 0600 "$work/release.iso" "$work/release.iso.sha256"
iso=$work/release.iso
chmod 0700 "$work/runtime"
export NESTEGG_CONFIG="$work/config.toml" NESTEGG_STATE_DIR="$work/state" NESTEGG_RUNTIME_DIR="$work/runtime" HOME="$work/home"
# Root-owned private scratch directory; no physical media are ever accepted.
stick=$work/stick.img
truncate -s 8G "$stick"
truncate -s 16G "$work/slow.iso"
"$bin" stick create "$stick" --test-image --image "$work/slow.iso" --sha256 "$(printf '%064d' 0)" > "$work/interrupted-iso.log" 2>&1 &
update_pid=$!
for ((i=0;i<2000;i++)); do
    compgen -G "$work/state/stick/iso-*/verified.iso" >/dev/null && break
    kill -0 "$update_pid"; sleep .005
done
compgen -G "$work/state/stick/iso-*/verified.iso" >/dev/null
kill -STOP "$update_pid"; kill -INT "$update_pid"; kill -CONT "$update_pid"
if wait "$update_pid"; then echo 'ERROR: interrupted ISO copy succeeded' >&2; exit 1; fi
update_pid=
if compgen -G "$work/state/stick/iso-*" >/dev/null; then echo 'ERROR: interrupted ISO copy retained' >&2; exit 1; fi
rm "$work/slow.iso"
# Ctrl-C at the confirmation prompt must also unwind the verified ISO copy.
if python3 "$here/confirm.py" --interrupt "$bin" stick create "$stick" --test-image --image "$iso" > "$work/interrupted-confirm.log" 2>&1; then exit 1; fi
grep -q 'stick operation interrupted' "$work/interrupted-confirm.log"
if compgen -G "$work/state/stick/iso-*" >/dev/null; then echo 'ERROR: confirmation retained ISO copy' >&2; exit 1; fi
# The Rust test mode allocates its own loop and still applies the real disk guard.
python3 "$here/confirm.py" "$bin" stick create "$stick" --test-image --image "$iso" >"$work/create.log" 2>&1
"$bin" stick info "$stick" --test-image >"$work/info-a.json"
grep -q '"active_slot": "a"' "$work/info-a.json"
"$bin" stick add "$stick" --test-image --image "$iso" >"$work/add.log" 2>&1
grep -q data_uuids "$NESTEGG_CONFIG"
[[ -s $work/state/stick/pbs.squashfs ]]
[[ $(stat -c '%a:%U:%G' "$NESTEGG_CONFIG") == 640:root:nestegg ]]
# Data UUID and partition layout must remain byte-for-byte stable after updates.
attach() { loop=$(losetup --find --show --partscan "$stick"); udevadm settle --timeout=30 || true; }
detach() { losetup -d "$loop"; loop=; udevadm settle --timeout=30 || true; }
attach
sfdisk --dump "$loop" | sed "s#$loop#DISK#g" >"$work/layout-before"
mount -o nodev,nosuid,noexec "${loop}p3" "$work/data"
printf 'data survives both updates\n' >"$work/data/retained.txt"
# Extract only the official client from the desktop-trusted image for host tests.
unsquashfs -no-progress -d "$work/client" "$work/state/stick/pbs.squashfs" usr/bin/proxmox-backup-client >"$work/extract.log" 2>&1
export NESTEGG_STICK_DATA="$work/data" NESTEGG_STICK_CLIENT="$work/client/usr/bin/proxmox-backup-client" NESTEGG_STICK_RUNTIME="$work/runtime"
"$probe" --ignored --nocapture >"$work/launcher.log" 2>&1
# An actual encrypted PBS datastore now exists on the partition.
[[ -d $work/data/nestegg/store/.chunks && -d $work/data/nestegg/store/host/stick-test ]]
find "$work/data/nestegg" -type f -print0 | sort -z | xargs -0 sha256sum | sed "s#$work/data/##g" >"$work/snapshots-before"
umount "$work/data"
detach
cat >"$work/steps" <<'STEPS'
expect root@.*# 240
send test "$(findmnt -rn -o SOURCE /run/live/medium)" = /dev/vda2 && grep -qw 'live-media-path=/live/ACTIVE' /proc/cmdline && test -s /run/live/medium/live/ACTIVE/filesystem.squashfs && printf 'STICK-%s\n' BOOT-OK
expect STICK-BOOT-OK 30
send for u in proxmox-backup.service proxmox-backup-proxy.service ssh.service ssh.socket exim4.service; do test "$(systemctl is-enabled "$u" 2>/dev/null)" = masked || exit 1; done; test -z "$(ss -H -lnt 'sport = :8007 or sport = :22 or sport = :25')" && printf 'LIVE-%s\n' MASKS-OK
expect LIVE-MASKS-OK 30
send poweroff -f
STEPS
boot() {
    local label=$1 active=$2; shift 2
    sed "s/ACTIVE/$active/g" "$work/steps" >"$work/steps-$label"
    qemu-system-x86_64 -enable-kvm -cpu host -m 2048 -smp 2 -display none -monitor none -nic none \
      -boot strict=on -cdrom "$iso" -drive "file=$stick,format=raw,if=none,id=stick" -device virtio-blk-pci,drive=stick,bootindex=1 -serial "unix:$work/console.sock,server=on,wait=on" "$@" >"$work/qemu-$label.log" 2>&1 &
    qemu_pid=$!
    timeout 300 python3 "$repo/tests/rescue/console.py" "$work/console.sock" "$work/boot-$label.log" "$work/steps-$label"
    wait "$qemu_pid"; qemu_pid=
    grep -q 'STICK-BOOT-OK' "$work/boot-$label.log"
    grep -q 'LIVE-MASKS-OK' "$work/boot-$label.log"
    rm -f "$work/console.sock"
    echo "PASS $label: pinned live medium, correct slot, PBS/SSH/mail masked"
}
both() {
    local label=$1 active=$2
    boot "$label-bios" "$active"
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars.fd"
    boot "$label-uefi" "$active" -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd -drive "if=pflash,format=raw,file=$work/vars.fd"
}
both created a
# A committed old-format slot remains recognisable; updating it must actually
# boot the new slot, including when the old parser added an earlier menu entry.
"$here/legacy-slot.sh" "$bin" "$iso" "$stick" "$work/legacy.img"
original_stick=$stick
stick=$work/legacy.img
both legacy-upgraded b
stick=$original_stick
rm "$work/legacy.img"
# Observe actual fsync barriers in the real helper; do not replace their work.
cat > "$work/tools/sync" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
for path in "$@"; do
    [[ $path != -* ]] || { echo 'syncfs/option is forbidden' >&2; exit 1; }
    if [[ $path == */esp/live/b/* ]]; then
        dir=${path%/*}
        printf '%s %s %s\n' "${path##*/}" "$([[ -f $dir/boot.cfg ]] && echo cfg || echo no-cfg)" "$([[ -f $dir/commit.sha256 ]] && echo commit || echo no-commit)" >> "$NESTEGG_TEST_SYNC_LOG"
    fi
done
exec /usr/bin/sync "$@"
WRAPPER
chmod +x "$work/tools/sync"
PATH="$work/tools:$PATH" NESTEGG_TEST_SYNC_LOG="$work/sync-order" "$bin" stick update "$stick" --test-image --image "$iso" >"$work/update.log" 2>&1
python3 - "$work/sync-order" <<'CHECK'
import sys
rows = [line.split() for line in open(sys.argv[1])]
for payload in ('filesystem.squashfs', 'payload.sha256'):
    assert [payload, 'no-cfg', 'no-commit'] in rows, rows
cfg = rows.index(['boot.cfg', 'cfg', 'no-commit'])
commit = rows.index(['commit.sha256', 'cfg', 'commit'])
assert cfg < commit
CHECK
# Check coreutils really calls fsync, including for directories.
strace -o "$work/fsync.log" -e fsync,fdatasync,syncfs /usr/bin/sync "$work" "$work/release.iso"
[[ $(grep -c '^fsync(.*= 0$' "$work/fsync.log") == 2 ]]
if grep -q '^syncfs(' "$work/fsync.log"; then echo 'ERROR: syncfs used instead of fsync' >&2; exit 1; fi
rm "$work/tools/sync"
"$bin" stick info "$stick" --test-image >"$work/info-b.json"
grep -q '"active_slot": "b"' "$work/info-b.json"
both updated b
# shellcheck source=tests/stick/adversarial.sh
source "$here/adversarial.sh"
# Stop the REAL update helper after copying HALF of the inactive squashfs. PATH
# interposes only that copy; all earlier checks and the update itself are real.
# tools directory was created with the private fixture
cat >"$work/tools/cp" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
last=${!#}
if [[ $last == */esp/live/a/ ]]; then
    src=${@: -2:1}
    bytes=$(stat -c %s "$src/filesystem.squashfs")
    dd if="$src/filesystem.squashfs" of="$last/filesystem.squashfs" bs=1M count=$((bytes / 2097152)) status=none
    touch "$NESTEGG_TEST_HALF_COPY"
    exec sleep 600
fi
exec /usr/bin/cp "$@"
WRAPPER
chmod +x "$work/tools/cp"
PATH="$work/tools:$PATH" NESTEGG_TEST_HALF_COPY="$work/half-copied" "$bin" stick update "$stick" --test-image --image "$iso" >"$work/interrupted.log" 2>&1 &
update_pid=$!
for ((i=0;i<600;i++)); do [[ -f $work/half-copied ]] && break; kill -0 "$update_pid"; sleep .1; done
[[ -f $work/half-copied ]]
kill -TERM "$update_pid"
if wait "$update_pid"; then echo 'ERROR: interrupted update succeeded' >&2; exit 1; fi
update_pid=
"$bin" stick info "$stick" --test-image >"$work/info-interrupted.json"
cmp "$work/info-b.json" "$work/info-interrupted.json"
both interrupted b
attach
sfdisk --dump "$loop" | sed "s#$loop#DISK#g" >"$work/layout-after"
cmp "$work/layout-before" "$work/layout-after"
mount -o nodev,nosuid,noexec "${loop}p3" "$work/data"
grep -qx 'data survives both updates' "$work/data/retained.txt"
find "$work/data/nestegg" -type f -print0 | sort -z | xargs -0 sha256sum | sed "s#$work/data/##g" >"$work/snapshots-after"
cmp "$work/snapshots-before" "$work/snapshots-after"
umount "$work/data"
detach
# Repair the incomplete INACTIVE slot with a subsequent normal update.
"$bin" stick update "$stick" --test-image --image "$iso" >"$work/retry.log" 2>&1
both retried a
# Only after all preservation checks: deliberately recreate the owned image.
# This exercises the second ERASE confirmation and replacing an existing ext4.
python3 "$here/confirm.py" "$bin" stick create "$stick" --test-image --image "$iso" >"$work/recreate.log" 2>&1
"$bin" stick info "$stick" --test-image >"$work/info-recreated.json"
grep -q '"active_slot": "a"' "$work/info-recreated.json"
attach
mount -o nodev,nosuid,noexec "${loop}p3" "$work/data"
[[ -d $work/data/nestegg && ! -e $work/data/retained.txt && ! -e $work/data/nestegg/store ]]
dumpe2fs -h "${loop}p3" 2>/dev/null | grep -Eq '^Reserved block count:[[:space:]]+0$'
umount "$work/data"
detach
echo 'PASS create/add/info/update, half-written update, retry, retained encrypted snapshots, nonempty recreation, BIOS and UEFI; no physical USB device used'
