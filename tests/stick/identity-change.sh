#!/usr/bin/env bash
# Reinspect the real CLI target after confirmation, without releasing a loop
# number that another concurrent test could allocate. No physical disk input.
set -Eeuo pipefail
[[ $EUID == 0 ]] || exit 2
bin=$(realpath "${1:?binary required}")
iso=$(realpath "${2:?ISO required}")
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d /tmp/nestegg-stick-identity.XXXXXX)
cleanup() {
    local status=$?
    trap - EXIT
    if losetup -ln -O BACK-FILE | grep -Fq "$work/"; then
        echo "ERROR: identity test retained an owned loop: $work" >&2; exit 1
    fi
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
cp --reflink=auto "$iso" "$work/release.iso"
cp "$iso.sha256" "$work/release.iso.sha256"
chmod 0600 "$work/release.iso" "$work/release.iso.sha256"
mkdir -m 0700 "$work/state" "$work/runtime"
export NESTEGG_STATE_DIR="$work/state" NESTEGG_RUNTIME_DIR="$work/runtime"
truncate -s 8G "$work/stick.img"
if python3 "$here/confirm.py" --resize-confirmed "$bin" stick create "$work/stick.img" --test-image --image "$work/release.iso" > "$work/result.log" 2>&1; then
    echo 'ERROR: CLI accepted target identity change after confirmation' >&2; exit 1
fi
grep -q 'identity or content changed after inspection/confirmation' "$work/result.log"
[[ $(stat -c %s "$work/stick.img") == 4294967296 ]]
# No GPT, filesystem or boot sector may have been written.
cmp -n 4096 "$work/stick.img" /dev/zero
tail -c 4096 "$work/stick.img" | cmp -n 4096 - /dev/zero
if compgen -G "$work/state/stick/iso-*" >/dev/null; then echo 'ERROR: verified ISO retained after identity refusal' >&2; exit 1; fi
echo 'PASS real CLI refuses changed capacity after typed confirmation; no partition writes or owned loops remain'
