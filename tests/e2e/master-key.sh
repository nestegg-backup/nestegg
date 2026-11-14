#!/usr/bin/env bash
# Real-client master-key recovery against a disposable PBS. No host services,
# credentials, mounts or network servers outside this test are touched.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="$(realpath "${1:-$repo/target/debug/nestegg}")"
e2e_id="$$-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
net="nestegg-master-e2e-$e2e_id"
pbs="nestegg-master-e2e-pbs-$e2e_id"
client="nestegg-master-e2e-client-$e2e_id"
client_image="$net-client-image"
export NESTEGG_E2E_PBS_IMAGE="$net-pbs-image"
cleanup() {
  docker rm -f "$client" "$pbs" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
  docker image rm "$client_image" "$NESTEGG_E2E_PBS_IMAGE" >/dev/null 2>&1 || true
}
trap cleanup EXIT
source "$here/lib.sh"
[[ -x "$bin" ]] || { echo 'Run cargo build first'; exit 2; }
build_pbs_image
DOCKER_BUILDKIT=0 docker build -q -t "$client_image" -f "$here/client.Dockerfile" "$here" >/dev/null
start_pbs
docker run -d --name "$client" --hostname desktop --network "$net" \
  -v "$bin:/usr/local/bin/nestegg:ro" "$client_image" sleep infinity >/dev/null

echo '== official client master-key commands'
on_client proxmox-backup-client help key create-master-key
on_client proxmox-backup-client help key import-with-master-key

echo '== configure the computer and create the master pair with the official client'
on_client nestegg init --server pbs.e2e.test --datastore store1 --namespace desktops \
  --auth-id 'backup@pbs!desktop' --fingerprint "$fp" --backup-id desktop --no-system >/dev/null
printf '%s\n' "$token_secret" | docker exec -i "$client" nestegg credentials set-password --allow-plain >/dev/null
on_client nestegg key create --allow-plain >/dev/null
on_client mkdir -p /root/master /home/alice
on_client bash -c 'printf "restored through the household master key\n" > /home/alice/report.txt'
master_password="$(head -c 24 /dev/urandom | base64)"
printf '%s\n%s\n' "$master_password" "$master_password" | docker exec -i "$client" \
  timeout 60 script -qec 'cd /root/master && proxmox-backup-client key create-master-key' /dev/null >/dev/null
on_client cp /root/master/master-public.pem /etc/nestegg/master-public.pem
on_client sed -i '/^\[backup\]$/a master_public_key = "/etc/nestegg/master-public.pem"' /etc/nestegg/config.toml
# Stand in for the join response's public proof. No hub is contacted by a
# backup: the hub is deliberately absent from this test's network.
master_fingerprint="$(on_client bash -c 'openssl pkey -pubin -in /etc/nestegg/master-public.pem -outform DER | openssl dgst -sha256 -c' | sed 's/^.*= //')"
proof_time="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
on_client cat /var/lib/nestegg/state.json | python3 -c '
import json,sys
s=json.load(sys.stdin)
s["master_key"]={"fingerprint":sys.argv[1],"proven_at":sys.argv[2]}
print(json.dumps(s))' "$master_fingerprint" "$proof_time" | docker exec -i "$client" tee /var/lib/nestegg/state.json.new >/dev/null
on_client mv /var/lib/nestegg/state.json.new /var/lib/nestegg/state.json
docker exec -i "$client" tee -a /etc/nestegg/config.toml >/dev/null <<'HUB'

[hub]
name = "Disposable test household"
url = "https://hub.example.com:8443"
pin = "sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
machine_id = "fixture-machine"
HUB
check 'own key copy remains unproven' on_client bash -c 'nestegg status --json | grep -q '"'"'"key_exported": false'"'"''
check 'proven master key is reported' on_client bash -c 'nestegg key show | grep -q "proven household master key"'
check 'backup passes the proof gate with the master key alone' on_client nestegg run --trigger manual
snapshot="$(on_client nestegg snapshots --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["snapshot"])')"
on_client cat /var/lib/nestegg/state.json | python3 -c 'import json,sys; s=json.load(sys.stdin); assert s["last_run"]["state"]=="succeeded"'
files="$(docker exec -e PBS_PASSWORD="$token_secret" -e PBS_FINGERPRINT="$fp" "$client" \
  proxmox-backup-client snapshot files "$snapshot" --ns desktops --repository 'backup@pbs!desktop@pbs.e2e.test:store1' --output-format json)"
check 'RSA recovery blob is in the snapshot' grep -q 'rsa-encrypted.key.blob' <<<"$files"
on_client nestegg key export /root/original.json >/dev/null
original_fingerprint="$(on_client proxmox-backup-client key show /root/original.json --output-format json | python3 -c 'import json,sys; print(json.load(sys.stdin)["fingerprint"])')"
# Lose only the computer's test credential; recover must not read it.
on_client rm -f /etc/nestegg/credentials/encryption-key /etc/nestegg/credentials/encryption-key.cred
recover() {
  local output="$1"; shift
  local destination="--output '$output'"
  [[ "$output" != --install ]] || destination=--install
  local diagnostic status=0
  diagnostic="$(printf '%s\n' "$master_password" | docker exec -i "$client" timeout 60 script -qec \
    "nestegg key recover --master-key /root/master/master-private.pem $destination $*" /dev/null)" || status=$?
  if [[ $status -ne 0 ]]; then printf '%s\n' "$diagnostic" | sed -n '/^error:/p'; return "$status"; fi
}
check 'recover newest snapshot with terminal password prompt' recover /root/recovered.json
check 'recover explicit snapshot time' recover /root/recovered-explicit.json --snapshot "${snapshot##*/}"
recovered_fingerprint="$(on_client proxmox-backup-client key show /root/recovered.json --output-format json | python3 -c 'import json,sys; print(json.load(sys.stdin)["fingerprint"])')"
check 'recovered key has the original fingerprint' test "$recovered_fingerprint" = "$original_fingerprint"
check 'recovered key has no password and is importable' on_client nestegg key import /root/recovered.json --allow-plain
check 'restore with the recovered key works' on_client nestegg restore "$snapshot" home.pxar /root/restored --keyfile /root/recovered.json
check 'restored content matches the source' on_client cmp /home/alice/report.txt /root/restored/alice/report.txt
check 'recovery never overwrites an existing output' fails_with 1 'already exists' \
  on_client nestegg key recover --master-key /root/master/master-private.pem --output /root/recovered.json
# Read-back must refuse a snapshot listing that hides the recovery envelope.
docker exec -i "$client" tee /root/client-without-rsa-listing >/dev/null <<'WRAPPER'
#!/bin/bash
if [[ $1 == snapshot && $2 == files ]]; then
  /usr/bin/proxmox-backup-client "$@" | sed 's/rsa-encrypted.key.blob/hidden.key.blob/g'
else
  exec /usr/bin/proxmox-backup-client "$@"
fi
WRAPPER
on_client chmod 700 /root/client-without-rsa-listing
check 'read-back requires the RSA recovery blob' fails_with 1 'rsa-encrypted.key is missing' \
  docker exec -e NESTEGG_PBS_CLIENT=/root/client-without-rsa-listing "$client" nestegg run --trigger manual
# Changing/removing the public key revokes master coverage immediately.
on_client mv /etc/nestegg/master-public.pem /etc/nestegg/master-public.saved
check 'missing master public key blocks backups' fails_with 1 'master public key file.*missing' on_client nestegg run --trigger manual
check 'missing master key is null in the API status' on_client bash -c 'nestegg status --json | grep -q '"'"'"master_key": null'"'"''
on_client mv /etc/nestegg/master-public.saved /etc/nestegg/master-public.pem
# A deliberately new key generation in the same disposable group must not
# silently hide the old key behind the newest snapshot.
on_client nestegg key create --force --allow-plain >/dev/null
check 'a new key generation can back up with master coverage' on_client nestegg run --trigger manual
check 'mixed key generations require an explicit snapshot' fails_with 1 'more than one key.*--snapshot' \
  on_client nestegg key recover --master-key /root/master/master-private.pem --output /root/ambiguous.json
check 'mixed-key refusal writes no recovered key' on_client test ! -e /root/ambiguous.json

installed_before="$(on_client sha256sum /etc/nestegg/credentials/encryption-key)"
check 'install refuses a different existing encryption key' fails_with 1 'different encryption key already exists' \
  recover --install --snapshot "${snapshot##*/}"
check 'refused install leaves the existing key unchanged' test "$installed_before" = "$(on_client sha256sum /etc/nestegg/credentials/encryption-key)"

check 'an explicit old snapshot still recovers its original key' recover /root/old-again.json --snapshot "${snapshot##*/}"
old_again_fingerprint="$(on_client proxmox-backup-client key show /root/old-again.json --output-format json | python3 -c 'import json,sys; print(json.load(sys.stdin)["fingerprint"])')"
check 'explicit recovery names the original fingerprint' test "$old_again_fingerprint" = "$original_fingerprint"

result
