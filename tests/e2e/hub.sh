#!/usr/bin/env bash
# Disposable containers only. No host package/systemd/configuration changes.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="${1:-$repo/target/debug/nestegg}"
[[ -x "$bin" ]] || { echo 'Build nestegg first (cargo build).'; exit 2; }
exec python3 "$here/hub_test.py" "$(realpath "$bin")"
