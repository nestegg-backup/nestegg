#!/usr/bin/env bash
# Package and service test: install the .deb into a container running
# systemd, store secrets with systemd-creds, run backups through the timer
# unit and the daemon, and use the CLI and web UI as an unprivileged user.
#
# The systemd container needs the host cgroup namespace (--cgroupns=host with
# a writable /sys/fs/cgroup). Only run this on a disposable test machine.
#
# Usage: tests/e2e/systemd.sh [path/to/nestegg_*.deb]
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
deb="${1:-$(ls "$repo"/target/nestegg_*_amd64.deb 2>/dev/null | tail -1)}"
net="nestegg-e2e-$$"
pbs="nestegg-e2e-pbs-$$"
client="nestegg-e2e-sd-$$"

cleanup() {
  [[ -n "${KEEP:-}" ]] && { echo "(KEEP set: containers $pbs $client left running)"; return; }
  docker rm -f "$pbs" "$client" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
}
trap cleanup EXIT

[[ -f "$deb" ]] || { echo "package not found (run: cargo deb -p nestegg)"; exit 2; }
deb="$(realpath "$deb")"
# shellcheck source=lib.sh
source "$here/lib.sh"
as_alice() { docker exec -u alice "$client" "$@"; }

echo "== building images"
build_pbs_image
DOCKER_BUILDKIT=0 docker build -q -t nestegg-e2e-client:ubuntu-24.04 -f "$here/client.Dockerfile" "$here" >/dev/null
DOCKER_BUILDKIT=0 docker build -q -t nestegg-e2e-systemd -f "$here/systemd-client.Dockerfile" "$here" >/dev/null

start_pbs

echo "== starting systemd client and installing $(basename "$deb")"
docker run -d -t --name "$client" --network "$net" --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock \
  -v "$deb:/tmp/nestegg.deb:ro" nestegg-e2e-systemd >/dev/null
for _ in $(seq 1 30); do
  state="$(on_client systemctl is-system-running 2>/dev/null || true)"
  [[ "$state" == running || "$state" == degraded ]] && break
  sleep 1
done
on_client bash -c 'useradd -m alice && echo "hello" > /home/alice/notes.txt && chown -R alice: /home/alice'
check "package installs" on_client bash -c 'DEBIAN_FRONTEND=noninteractive apt-get install -y -q /tmp/nestegg.deb >/tmp/apt.log 2>&1'
check "group nestegg created" on_client getent group nestegg
check "runtime dir from tmpfiles" bash -c "[[ \$(docker exec $client stat -c '%a %G' /run/nestegg) == '750 nestegg' ]]"
check "units installed" on_client systemctl cat nestegg.service nestegg-backup.service nestegg-backup.timer

echo "== setup"
on_client nestegg init --server pbs.e2e.test --datastore store1 --namespace desktops \
  --auth-id 'backup@pbs!desktop' --fingerprint "$fp" --backup-id sd-desktop >/dev/null
docker exec -i "$client" nestegg credentials set-password <<<"$token_secret" >/dev/null
check "password stored with systemd-creds" on_client test -f /etc/nestegg/credentials/password.cred
check "no plain password file" bash -c "! docker exec $client test -e /etc/nestegg/credentials/password"
check "credential file is not plain text" bash -c "! docker exec $client grep -q '$token_secret' /etc/nestegg/credentials/password.cred"
on_client nestegg key create >/dev/null
check "key stored with systemd-creds" on_client test -f /etc/nestegg/credentials/encryption-key.cred
on_client nestegg key export /root/key.json >/dev/null
on_client nestegg key confirm-exported >/dev/null
on_client usermod -aG nestegg alice
check "services enable" on_client systemctl enable --now nestegg.service nestegg-backup.timer
sleep 1
check "daemon active" on_client systemctl is-active --quiet nestegg.service
check "timer active" on_client systemctl is-active --quiet nestegg-backup.timer
check "check passes" on_client nestegg check

echo "== backup through the systemd unit"
check "timer unit run succeeds" on_client systemctl start nestegg-backup.service
check "unit result success" bash -c "[[ \$(docker exec $client systemctl show -p Result --value nestegg-backup.service) == success ]]"
check "status ok for unprivileged user" bash -c "docker exec -u alice $client nestegg status | grep -q 'Health:       ok'"
check "history readable by group member" bash -c "docker exec -u alice $client nestegg history | grep -q '^ok'"

echo "== backup requested by the unprivileged user (daemon + systemd-run)"
check "user can start a backup" bash -c "docker exec -u alice $client nestegg backup | grep -q 'Backup started'"
for _ in $(seq 1 60); do
  runs="$(as_alice nestegg history --json | grep -c '"state": "succeeded"' || true)"
  [[ "$runs" -ge 2 ]] && break
  sleep 1
done
check "manual run finished" test "${runs:-0}" -ge 2
check "user can list snapshots via daemon" bash -c "[[ \$(docker exec -u alice $client nestegg snapshots --json | grep -c '\"snapshot\"') -ge 2 ]]"
check "non-member is refused" bash -c "docker exec $client useradd -m bob && ! docker exec -u bob $client nestegg snapshots >/dev/null 2>&1"

echo "== web UI as unprivileged user"
docker exec -d -u alice "$client" bash -c 'nestegg ui --no-browser --port 18080 > /tmp/ui.out 2>&1'
sleep 2
token="$(on_client sed -n 's/.*token=\([0-9a-f]*\).*/\1/p' /tmp/ui.out | head -1)"
check "ui prints a token" test -n "$token"
check "ui URL carries the token in the fragment" on_client grep -q '/#token=' /tmp/ui.out
check "ui rejects API requests without token" bash -c "[[ \$(docker exec $client curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18080/api/v1/status) == 401 ]]"
check "ui proxies the API with bearer token" bash -c "docker exec $client curl -sf -H 'Authorization: Bearer $token' http://127.0.0.1:18080/api/v1/status | grep -q '\"health\":\"ok\"'"
check "ui sets no cookies" bash -c "! docker exec $client curl -s -D - -o /dev/null http://127.0.0.1:18080/ | grep -qi 'set-cookie'"
check "ui only listens on loopback" bash -c "docker exec $client grep -qi ' 0100007F:46A0 ' /proc/net/tcp && ! docker exec $client grep -qi ' 00000000:46A0 ' /proc/net/tcp"

echo "== removal keeps credentials"
check "package removes cleanly" on_client bash -c 'DEBIAN_FRONTEND=noninteractive apt-get remove -y -q nestegg >/tmp/apt-rm.log 2>&1'
check "key survives removal" on_client test -f /etc/nestegg/credentials/encryption-key.cred

result
