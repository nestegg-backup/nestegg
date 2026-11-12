#!/usr/bin/env bash
# Root-owned Btrfs files on a loop still have anonymous (major 0) mount numbers.
# Usage: sudo -n btrfs-input.sh BINARY /path/to/mkfs.btrfs
set -Eeuo pipefail
[[ $EUID == 0 ]] || exit 2
bin=$(realpath "${1:?binary required}")
mkfs=$(realpath "${2:?mkfs.btrfs required}")
if [[ ${NESTEGG_BTRFS_PRIVATE:-0} != 1 ]]; then
    exec unshare --mount --propagation private env NESTEGG_BTRFS_PRIVATE=1 "$0" "$@"
fi
work=$(mktemp -d /tmp/nestegg-stick-btrfs.XXXXXX)
loop=
cleanup() {
    local status=$? failed=0
    trap - EXIT; trap '' INT TERM
    if mountpoint -q "$work/source"; then umount "$work/source" || failed=1; fi
    [[ -z $loop ]] || losetup -d "$loop" || failed=1
    if losetup -ln -O BACK-FILE | grep -Fq "$work/"; then failed=1; fi
    if ((failed)); then echo "ERROR: Btrfs test resources retained: $work" >&2; exit 1; fi
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$work/source" "$work/state" "$work/runtime"
chmod 0700 "$work/state" "$work/runtime"
truncate -s 256M "$work/source.img"
loop=$(losetup --find --show "$work/source.img")
"$mkfs" -f "$loop" > "$work/mkfs.log" 2>&1
mount -t btrfs -o nodev,nosuid,noexec "$loop" "$work/source"
printf 'untrusted filesystem image\n' > "$work/source/release.iso"
chmod 0600 "$work/source/release.iso"
[[ $(findmnt -rn -T "$work/source/release.iso" -o MAJ:MIN) == 0:* ]]
truncate -s 4G "$work/target.img"
export NESTEGG_STATE_DIR="$work/state" NESTEGG_RUNTIME_DIR="$work/runtime"
pin=$(sha256sum "$work/source/release.iso" | cut -d' ' -f1)
if "$bin" stick create "$work/target.img" --test-image --image "$work/source/release.iso" --sha256 "$pin" </dev/null > "$work/result.log" 2>&1; then
    echo 'ERROR: root-owned Btrfs loop input accepted' >&2; exit 1
fi
grep -q 'removable, USB or loop-backed storage' "$work/result.log"
cmp -n 4096 "$work/target.img" /dev/zero
echo 'PASS root-owned major-0 Btrfs input resolves its real loop backing and is refused'
