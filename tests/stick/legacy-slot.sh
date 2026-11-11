#!/usr/bin/env bash
# Convert ONLY a disposable copy to the exact legacy descriptor, then update it.
set -Eeuo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=$1; iso=$2; source_image=$3; legacy=$4
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
cp --reflink=auto --sparse=always "$source_image" "$legacy"
loop=$(losetup --find --show --partscan "$legacy")
mkdir "$work/esp"
mount -o nodev,nosuid,noexec "${loop}p2" "$work/esp"
python3 - "$here/../../crates/nestegg/src/stick/media.sh" "$work/esp" <<'PY'
import hashlib
from pathlib import Path
import re
import subprocess
import sys
source = Path(sys.argv[1]).read_text()
renderer = source[source.index('render_cfg_legacy() {'):source.index('small_file()')]
esp = Path(sys.argv[2])
config = esp / 'live/a/boot.cfg'
text = config.read_text()
boot_uuid = re.search(r'boot_uuid=([^\n]+)', (esp / 'nestegg-stick').read_text())[1]
kernel = re.search(r'^# nestegg-kernel=(.+)$', text, re.M)[1]
initrd = re.search(r'^# nestegg-initrd=(.+)$', text, re.M)[1]
output = subprocess.run(['bash', '-c', renderer + '\nboot_uuid=$1\nrender_cfg_legacy a 1 "$2" "$3"', '--', boot_uuid, kernel, initrd], check=True, capture_output=True).stdout
config.write_bytes(output)
(config.parent / 'commit.sha256').write_text(hashlib.sha256(output).hexdigest() + '  /live/a/boot.cfg\n')
PY
umount "$work/esp"
losetup -d "$loop"; loop=
"$bin" stick update "$legacy" --test-image --image "$iso" > "$work/update.log" 2>&1
