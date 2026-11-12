#!/usr/bin/env bash
# Parse the exact embedded slot renderer with GRUB, without touching a device.
set -Eeuo pipefail
here=$(cd "$(dirname "$0")" && pwd)
python3 - "$here/../../crates/nestegg/src/stick/media.sh" <<'PY'
from pathlib import Path
import subprocess
import sys
import tempfile
source = Path(sys.argv[1]).read_text()
start = source.index('render_cfg() {')
end = source.index('small_file()', start)
renderer = source[start:end]
with tempfile.TemporaryDirectory(prefix='nestegg-grub-config-') as directory:
    for slot, generation in [('a', 1), ('b', 2)]:
        script = renderer + '\nboot_uuid=1234-ABCD\nrender_cfg ' + slot + ' ' + str(generation) + ' vmlinuz initrd.img\n'
        output = subprocess.run(['bash', '-c', script], check=True, capture_output=True).stdout
        config = Path(directory) / (slot + '.cfg')
        config.write_bytes(output)
        subprocess.run(['grub-script-check', str(config)], check=True)
print('PASS slot descriptors parse with the official GRUB parser')
PY
