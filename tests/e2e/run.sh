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
docker run -d --name "$client" --network "$net" --tmpfs /mnt/small:size=2m,mode=1777 \
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
  cp /bin/true /home/alice/Documents/tool-with-capability
  chown -R alice:alice /home/alice
  # Many sockets: tar reports each as skipped; stderr exceeds a pipe buffer.
  mkdir -p /home/alice/sockets
  perl -MIO::Socket::UNIX - <<"PERL"
for (1..3000) {
  my $s = IO::Socket::UNIX->new(Type => SOCK_STREAM(), Local => "/home/alice/sockets/socket-with-a-long-name-$_", Listen => 1) or die $!;
  close $s;
}
PERL
  chown -R alice: /home/alice/sockets
  mkdir -p /home/alice/Archive/frozen && echo old > /home/alice/Archive/frozen/note.txt
  chmod 0555 /home/alice/Archive/frozen
  truncate -s 64M /home/alice/Documents/sparse.img && chown alice: /home/alice/Documents/sparse.img
  # after chown: chown clears file capabilities
  setcap cap_net_raw+ep /home/alice/Documents/tool-with-capability
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

echo "== review findings: export, restore as user, key handling"
alice_uid="$(on_client id -u alice)"
alice_gid="$(on_client id -g alice)"
as_sudo_alice() { docker exec -e SUDO_UID="$alice_uid" -e SUDO_GID="$alice_gid" "$client" "$@"; }
on_client bash -c 'echo keep > /root/victim && ln -s /root/victim /home/alice/evil-key.json'
check "export refuses a planted symlink" bash -c "! docker exec -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg key export /home/alice/evil-key.json >/dev/null 2>&1"
check "symlink target untouched" bash -c "[[ \$(docker exec $client cat /root/victim) == keep ]]"
as_sudo_alice nestegg key export /home/alice/key-copy.json >/dev/null
check "confirm after export of the same key works" on_client nestegg key confirm-exported
check "export as sudo user: owned by user, mode 600" bash -c "[[ \$(docker exec $client stat -c '%U %a' /home/alice/key-copy.json) == 'alice 600' ]]"
check "export cannot write where the user may not" bash -c "! docker exec -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg key export /etc/nestegg-key.json >/dev/null 2>&1"
check "restore into user dir succeeds (exit code)" bash -c "docker exec -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg restore $snapshot home.pxar /home/alice/restore-home > /tmp/nestegg-user-restore.log 2>&1"
check "user restore: full content matches" on_client diff -r /home/alice/Documents /home/alice/restore-home/alice/Documents
check "user restore: files owned by the user" bash -c "[[ -z \$(docker exec $client find /home/alice/restore-home ! -user alice) ]]"
check "user restore with 3000 sockets did not hang (warnings shown)" bash -c "[[ \$(grep -c 'socket ignored' /tmp/nestegg-user-restore.log) -ge 3000 ]]"
check "user restore: read-only directory restored" on_client test -f /home/alice/restore-home/alice/Archive/frozen/note.txt
long_name="$(printf 'r%.0s' $(seq 1 240))"
check "user restore into a 240-character target name" bash -c "docker exec -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg restore $snapshot etc.pxar /home/alice/$long_name >/dev/null 2>&1 && docker exec $client test -f /home/alice/$long_name/hostname"
check "failing user restore (target full) reports an error" bash -c "! docker exec -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg restore $snapshot home.pxar /mnt/small/target --pattern 'alice/Documents/photo.bin' >/tmp/nestegg-fail.log 2>&1"
check "failing user restore: error names the cause" grep -qi "no space" /tmp/nestegg-fail.log
check "failing user restore: no partial copy left" bash -c "[[ -z \$(docker exec $client ls -A /mnt/small) ]]"
check "failing user restore: staging cleaned up" bash -c "[[ -z \$(docker exec $client ls -A /var/lib/nestegg/restore-staging) ]]"
check "restore into missing parent fails before any work" bash -c "docker exec -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg restore $snapshot etc.pxar /home/alice/missing/target 2>&1 | grep -q 'does not exist'"
check "user restore: sparse file stays sparse" bash -c "[[ \$(docker exec $client du -k /home/alice/restore-home/alice/Documents/sparse.img | cut -f1) -lt 1024 ]]"
check "user restore: no partial directory left" bash -c "[[ -z \$(docker exec $client find /home/alice -maxdepth 1 -name '.*nestegg-partial*') ]]"
check "user restore: staging cleaned up" bash -c "[[ -z \$(docker exec $client ls -A /var/lib/nestegg/restore-staging) ]]"
check "relative export path as sudo user" bash -c "docker exec -w /home/alice -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg key export rel-key.json >/dev/null && docker exec $client test -f /home/alice/rel-key.json"
check "relative --keyfile as sudo user" bash -c "docker exec -w /home/alice -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg restore $snapshot etc.pxar /root/restore-relkey --keyfile rel-key.json >/dev/null 2>&1"
check "invalid SUDO_UID is rejected" bash -c "! docker exec -e SUDO_UID=abc $client nestegg key export /root/x.json >/dev/null 2>&1"
check "user restore: capability not applied" bash -c "[[ -z \$(docker exec $client getcap /home/alice/restore-home/alice/Documents/tool-with-capability) ]]"
check "root restore keeps the capability" bash -c "docker exec $client getcap /root/restore-home/alice/Documents/tool-with-capability | grep -q cap_net_raw"
check "relative restore target is resolved" bash -c "docker exec -w /root $client nestegg restore $snapshot etc.pxar relative-restore >/dev/null 2>&1 && docker exec $client test -e /root/relative-restore/hostname"
check "restore target with .. is refused" bash -c "! docker exec $client nestegg restore $snapshot etc.pxar /root/missing/../../tmp/x >/dev/null 2>&1"
check "restore into /tmp without sudo user is refused" bash -c "! docker exec $client nestegg restore $snapshot etc.pxar /tmp/restore-etc >/dev/null 2>&1"

on_client sed -i 's/^encryption = true/encryption = false/' /etc/nestegg/config.toml
check "unencrypted backup succeeds" on_client nestegg run --trigger manual
plain_snapshot="$(on_client nestegg snapshots --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
check "unencrypted snapshot restores while a key exists" on_client nestegg restore "$plain_snapshot" etc.pxar /root/restore-plain
check "older encrypted snapshot still restores" on_client nestegg restore "$snapshot" etc.pxar /root/restore-enc
on_client sed -i 's/^encryption = false/encryption = true/' /etc/nestegg/config.toml

on_client nestegg key create --force --allow-plain >/dev/null
check "new key blocks backups until exported" bash -c "! docker exec $client nestegg backup >/dev/null 2>&1"
check "status shows key not exported" bash -c "docker exec $client nestegg status | grep -q 'KEY NOT EXPORTED'"
check "confirm before exporting the new key is refused" bash -c "! docker exec $client nestegg key confirm-exported >/dev/null 2>&1"
on_client nestegg key export /root/nestegg-key-2.json >/dev/null
on_client nestegg key confirm-exported >/dev/null
check "backup runs again after confirming the new key" on_client nestegg backup
check "old snapshot restores with old exported key" on_client nestegg restore "$snapshot" etc.pxar /root/restore-oldkey --keyfile /root/nestegg-key.json

echo "== second backup and timer behaviour"
check "manual second backup succeeds" on_client nestegg run --trigger manual
check "timer run is skipped when not due" bash -c "docker exec $client nestegg run --trigger timer 2>&1 | grep -q 'not due'"
check "four snapshots listed" bash -c "[[ \$(docker exec $client nestegg snapshots --json | grep -c '\"snapshot\"') == 4 ]]"

echo "== daemon API"
on_client bash -c 'nestegg daemon >/var/log/nestegg-daemon.log 2>&1 &'
sleep 1
check "daemon status endpoint" bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock http://localhost/api/v1/status | grep -q '\"health\":\"ok\"'"
check "daemon snapshots endpoint" bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock http://localhost/api/v1/snapshots | grep -q e2e-desktop"
check "socket mode is 0660" bash -c "[[ \$(docker exec $client stat -c '%a' /run/nestegg/api.sock) == 660 ]]"

echo "== API as unprivileged user: session, browse, restore, machines, cancel"
on_client usermod -aG nestegg alice
API="docker exec -u alice $client curl -s --unix-socket /run/nestegg/api.sock"
latest="$(on_client nestegg snapshots --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
ltime="${latest##*/}"
check "session identifies the caller" bash -c "$API http://localhost/api/v1/session | grep -q '\"user\":\"alice\"'"
check "browse root shows only the own home" bash -c "[[ \$($API 'http://localhost/api/v1/snapshots/$ltime/browse?archive=home.pxar&path=/' | grep -o '\"name\":\"[^\"]*\"' | sort -u | tr -d '\n') == '\"name\":\"alice\"' ]]"
check "browse inside the own home" bash -c "$API 'http://localhost/api/v1/snapshots/$ltime/browse?archive=home.pxar&path=/alice/Documents' | grep -q report.txt"
check "browse of another archive is refused" bash -c "$API -o /dev/null -w '%{http_code}' 'http://localhost/api/v1/snapshots/$ltime/browse?archive=etc.pxar&path=/' | grep -q 403"
restore_resp="$($API -X POST -H 'Content-Type: application/json' -d "{\"snapshot_time\":\"$ltime\",\"archive\":\"home.pxar\",\"paths\":[\"/alice/Documents\"],\"target_dir\":\"/home/alice/api-restore\"}" http://localhost/api/v1/restores)"
restore_id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$restore_resp")"
check "restore accepted" test -n "$restore_id"
for _ in $(seq 1 60); do
  rstate="$($API "http://localhost/api/v1/restores/$restore_id" | sed -n 's/.*"state":"\([^"]*\)".*/\1/p')"
  [[ "$rstate" != running ]] && break
  sleep 1
done
check "API restore succeeded" test "$rstate" = succeeded
check "API restore: content matches" on_client cmp /home/alice/Documents/report.txt /home/alice/api-restore/alice/Documents/report.txt
check "API restore: owned by the caller" bash -c "[[ -z \$(docker exec $client find /home/alice/api-restore ! -user alice) ]]"
check "API restore: only the selected folder" bash -c "! docker exec $client test -e /home/alice/api-restore/alice/.bashrc"
check "API restore outside the own home is refused" bash -c "$API -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{\"snapshot_time\":\"$ltime\",\"archive\":\"home.pxar\",\"paths\":[\"/root\"],\"target_dir\":\"/home/alice/x1\"}' http://localhost/api/v1/restores | grep -q 403"
check "API restore into an existing target is 409" bash -c "$API -X POST -H 'Content-Type: application/json' -d '{\"snapshot_time\":\"$ltime\",\"archive\":\"home.pxar\",\"paths\":[],\"target_dir\":\"/home/alice/api-restore\"}' http://localhost/api/v1/restores | grep -q target_exists"
check "machines lists this PC with its report" bash -c "$API http://localhost/api/v1/machines | grep -q '\"hostname\":'"
check "pbs info: reachable" bash -c "$API http://localhost/api/v1/pbs | grep -q '\"reachable\":true'"
check "connection check via API" bash -c "$API -X POST http://localhost/api/v1/pbs/check | grep -q 'PBS login and datastore access'"

# Cancel a running backup: throttle the upload and add new data.
on_client sed -i 's/^\[backup\]$/[backup]\nrate_limit = "100KiB"/' /etc/nestegg/config.toml
on_client bash -c 'head -c 20000000 /dev/urandom > /home/alice/big-new-file.bin'
run_resp="$($API -X POST http://localhost/api/v1/runs)"
run_id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$run_resp")"
for _ in $(seq 1 60); do
  $API http://localhost/api/v1/status | grep -q '"state":"uploading"' && break
  sleep 1
done
check "cancel request accepted" bash -c "$API -o /dev/null -w '%{http_code}' -X POST http://localhost/api/v1/runs/$run_id/cancel | grep -q 202"
for _ in $(seq 1 60); do
  $API "http://localhost/api/v1/runs/$run_id" | grep -q '"state":"cancelled"' && break
  sleep 1
done
check "cancelled run recorded as cancelled" bash -c "$API http://localhost/api/v1/runs/$run_id | grep -q '\"state\":\"cancelled\"'"
check "no backup client left running" bash -c "! docker exec $client pgrep -f 'proxmox-backup-client backup' >/dev/null"
on_client sed -i '/^rate_limit = "100KiB"$/d' /etc/nestegg/config.toml
on_client rm -f /home/alice/big-new-file.bin

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
