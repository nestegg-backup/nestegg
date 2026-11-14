#!/usr/bin/env bash
# Real one-command onboarding; disposable PBS/hub and fresh systemd desktops.
# Uses exactly the systemd container permissions documented in lib.sh.
# Usage: tests/e2e/hub-join.sh [path/to/nestegg_*.deb]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
deb="${1:-$(find "$repo/target/debian" -maxdepth 1 -name 'nestegg_*_amd64.deb' 2>/dev/null | sort | tail -1)}"
[[ -f "$deb" ]] || { echo 'Build UI, then cargo build --release && cargo deb -p nestegg --no-build.'; exit 2; }
echo "== package sha256 $(sha256sum "$deb" | cut -d ' ' -f1)"
prefix="nestegg-hub-join-$$-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
cleanup() {
  for suffix in desktop portable hub pbs; do docker rm -f "$prefix-$suffix" >/dev/null 2>&1 || true; done
  docker network rm "$prefix-net" >/dev/null 2>&1 || true
  for suffix in pbs hub client; do docker image rm "$prefix-$suffix" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT
# Do not retag, replace or delete another test's images.
echo '== building isolated images'
docker build -q -t "$prefix-pbs" -f "$here/pbs.Dockerfile" "$here" >/dev/null
docker build -q -t "$prefix-hub" --build-arg BASE=debian:trixie -f "$here/client.Dockerfile" "$here" >/dev/null
docker build -q -t "$prefix-client" -f "$here/hub-join-client.Dockerfile" "$here" >/dev/null
python3 "$here/hub_join_test.py" "$(realpath "$deb")" "$prefix"
