#!/usr/bin/env bash
# End-to-end test: real Proxmox Backup Server + real proxmox-backup-client in
# disposable containers. Nothing outside these containers is touched.
#
# Usage: tests/e2e/run.sh [path/to/nestegg-binary] [client-base-image]
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="${1:-$repo/target/debug/nestegg}"
base="${2:-ubuntu:24.04}"
net="nestegg-e2e-$$"
pbs="nestegg-e2e-pbs-$$"
client="nestegg-e2e-client-$$"
client_image="nestegg-e2e-client:$(echo "$base" | tr ':/' '--')"
[[ -x "$bin" ]] || { echo "nestegg binary not found: $bin (run cargo build first)"; exit 2; }
bin="$(realpath "$bin")"
# shellcheck source=lib.sh
source "$here/lib.sh"

echo "== building images (client base: $base)"
build_pbs_image
DOCKER_BUILDKIT=0 docker build -q --build-arg BASE="$base" -t "$client_image" -f "$here/client.Dockerfile" "$here" >/dev/null

start_pbs

echo "== starting client ($base)"
docker run -d --name "$client" --network "$net" \
  -v "$bin:/usr/local/bin/nestegg:ro" "$client_image" sleep infinity >/dev/null
on_client bash -c '
  set -e
  useradd -m alice
  mkdir -p /home/alice/Documents /home/alice/.cache/big /home/alice/project/node_modules/x
  echo "quarterly report" > /home/alice/Documents/report.txt
  head -c 3000000 /dev/urandom > /home/alice/Documents/photo.bin
  ln -s Documents/report.txt /home/alice/link-to-report
  echo "cache" > /home/alice/.cache/big/blob
  echo "dep" > /home/alice/project/node_modules/x/index.js
  chmod 640 /home/alice/Documents/report.txt
  chown -R alice:alice /home/alice
'

echo "== setup with the nestegg CLI"
on_client nestegg init --server pbs.e2e.test --datastore store1 --namespace desktops \
  --auth-id 'backup@pbs!desktop' --fingerprint "$fp" --backup-id e2e-desktop >/dev/null
docker exec -i "$client" nestegg credentials set-password --allow-plain <<<"$token_secret" >/dev/null
on_client nestegg key create --allow-plain >/dev/null
check "backup refused while key not exported" \
  bash -c "! docker exec $client nestegg backup >/dev/null 2>&1"
on_client nestegg key export /root/nestegg-key.json >/dev/null
check "paper key contains a QR code" \
  bash -c "docker exec $client nestegg key paperkey | grep -q 'BEGIN PROXMOX BACKUP KEY\|-----BEGIN'"
on_client nestegg key confirm-exported >/dev/null
check "config contains no secret" \
  bash -c "! docker exec $client grep -q '$token_secret' /etc/nestegg/config.toml"
on_client nestegg check || true

echo "== first backup"
check "backup succeeds" on_client nestegg backup
check "status is ok" bash -c "docker exec $client nestegg status --json | grep -q '\"health\": \"ok\"'"
snaps="$(on_client nestegg snapshots --json)"
check "one snapshot listed" bash -c "[[ \$(grep -c '\"snapshot\"' <<<'$snaps') == 1 ]]"
check "snapshot is encrypted" grep -q '"encrypted": true' <<<"$snaps"
check "snapshot has nestegg notes" grep -q '"comment": "nestegg' <<<"$snaps"
check "uploaded bytes recorded" bash -c "docker exec $client nestegg status --json | grep -Eq '\"uploaded_bytes\": [1-9]'"
check "archive sizes recorded" bash -c "docker exec $client nestegg status --json | grep -Eq '\"size_bytes\": [1-9]'"
check "secret not in run logs" bash -c "! docker exec $client grep -rq '$token_secret' /var/lib/nestegg"
snapshot="$(sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' <<<"$snaps" | head -1)"
admin_list="$(docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$pbs" \
  proxmox-backup-client snapshot list --ns desktops --repository admin@pbs@127.0.0.1:store1 --output-format json)"
check "PBS stores archives encrypted" grep -q '"crypt-mode":"encrypt"' <<<"$admin_list"
check "inventory archive uploaded" grep -q 'inventory.pxar' <<<"$admin_list"

echo "== restore"
on_client nestegg restore "$snapshot" home.pxar /root/restore-home >/dev/null 2>&1
check "restored file content matches" on_client cmp /home/alice/Documents/photo.bin /root/restore-home/alice/Documents/photo.bin
check "symlink restored" bash -c "[[ \$(docker exec $client readlink /root/restore-home/alice/link-to-report) == Documents/report.txt ]]"
check "owner and mode restored" bash -c "[[ \$(docker exec $client stat -c '%U %a' /root/restore-home/alice/Documents/report.txt) == 'alice 640' ]]"
check "excluded cache not in backup" bash -c "! docker exec $client test -e /root/restore-home/alice/.cache/big/blob"
check "excluded node_modules not in backup" bash -c "! docker exec $client test -e /root/restore-home/alice/project/node_modules"
check "restore refuses existing target" bash -c "! docker exec $client nestegg restore $snapshot home.pxar /root/restore-home >/dev/null 2>&1"
on_client nestegg restore "$snapshot" home.pxar /root/restore-docs --pattern 'alice/Documents/report.txt' >/dev/null 2>&1 || true
check "pattern restore limits files" bash -c "docker exec $client test -e /root/restore-docs/alice/Documents/report.txt && ! docker exec $client test -e /root/restore-docs/alice/Documents/photo.bin"
check "restore with plain client and exported key" docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$client" \
  bash -c "proxmox-backup-client restore '$snapshot' etc.pxar /root/restore-etc --ns desktops --keyfile /root/nestegg-key.json --repository admin@pbs@pbs.e2e.test:store1 >/dev/null 2>&1 && test -e /root/restore-etc/hostname"

echo "== second backup and timer behaviour"
check "manual second backup succeeds" on_client nestegg run --trigger manual
check "timer run is skipped when not due" bash -c "docker exec $client nestegg run --trigger timer 2>&1 | grep -q 'not due'"
check "two snapshots listed" bash -c "[[ \$(docker exec $client nestegg snapshots --json | grep -c '\"snapshot\"') == 2 ]]"

echo "== daemon API"
on_client bash -c 'nestegg daemon >/var/log/nestegg-daemon.log 2>&1 &'
sleep 1
check "daemon status endpoint" bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock http://localhost/api/v1/status | grep -q '\"health\":\"ok\"'"
check "daemon snapshots endpoint" bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock http://localhost/api/v1/snapshots | grep -q e2e-desktop"
check "socket mode is 0660" bash -c "[[ \$(docker exec $client stat -c '%a' /run/nestegg/api.sock) == 660 ]]"

echo "== token cannot delete backups"
check "token is not allowed to forget snapshots" bash -c "! docker exec -e PBS_PASSWORD='$token_secret' -e PBS_FINGERPRINT='$fp' $client \
  proxmox-backup-client snapshot forget '$snapshot' --ns desktops --repository 'backup@pbs!desktop@pbs.e2e.test:store1' >/dev/null 2>&1"

echo "== PBS unreachable"
docker stop "$pbs" >/dev/null
set +e
on_client nestegg run --trigger manual >/dev/null 2>&1
code=$?
set -e
check "exit code 75 when PBS is down" test "$code" -eq 75
check "state is pbs_unreachable" bash -c "docker exec $client nestegg status --json | grep -q '\"state\": \"pbs_unreachable\"'"
check "health stays ok after a recent success" bash -c "docker exec $client nestegg status --json | grep -q '\"health\": \"ok\"'"

result
