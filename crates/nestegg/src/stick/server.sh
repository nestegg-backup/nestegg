#!/usr/bin/env bash
# Namespace PID 1: no systemd and no network interfaces except loopback.
set -euo pipefail
umask 022
exec 3>&1
exec 1>&2
lib=/usr/lib/x86_64-linux-gnu/proxmox-backup
mkdir -p /run/proxmox-backup /var/log/proxmox-backup /root/.cache
# Resolve the image's own identities, never the desktop's backup UID/GID.
chown -R backup:backup /run/proxmox-backup /var/log/proxmox-backup /etc/proxmox-backup
chmod 0700 /etc/proxmox-backup
"$lib/proxmox-backup-api" 3>&- &
api=$!
trap 'kill "$api" "${proxy:-}" 2>/dev/null || true; wait || true' EXIT
api_ready=0
for ((i=0;i<120;i++)); do
  # The API creates its certificate BEFORE its authentication and CSRF keys.
  # The proxy reads all of them at startup; a fixed sleep races slow hosts.
  if [[ -s /etc/proxmox-backup/proxy.pem && -s /etc/proxmox-backup/proxy.key &&
        -s /etc/proxmox-backup/authkey.pub && -s /etc/proxmox-backup/authkey.key &&
        -s /etc/proxmox-backup/csrf.key ]] &&
     { : <>/dev/tcp/127.0.0.1/82; } 2>/dev/null; then
    api_ready=1
    break
  fi
  kill -0 "$api"
  sleep .5
done
[[ $api_ready == 1 ]]
setpriv --reuid=backup --regid=backup --init-groups "$lib/proxmox-backup-proxy" 3>&- &
proxy=$!
proxmox-backup-manager user create nestegg@pbs
(umask 077; proxmox-backup-manager user generate-token nestegg@pbs stick > /run/token.json)
secret=$(sed -n 's/.*"value": *"\([^"]*\)".*/\1/p' /run/token.json)
rm /run/token.json
[[ $secret =~ ^[a-zA-Z0-9-]+$ ]]
proxmox-backup-manager acl update /datastore/stick DatastoreAdmin --auth-id nestegg@pbs
proxmox-backup-manager acl update /datastore/stick DatastoreAdmin --auth-id 'nestegg@pbs!stick'
reuse=()
[[ ! -d /mnt/stick/nestegg/.chunks ]] || reuse=(--reuse-datastore true)
proxmox-backup-manager datastore create stick /mnt/stick/nestegg "${reuse[@]}" --tuning gc-atime-cutoff=1
fingerprint=$(openssl x509 -in /etc/proxmox-backup/proxy.pem -noout -fingerprint -sha256 | cut -d= -f2)
for ((i=0;i<120;i++)); do
  if curl -skf https://127.0.0.1:8007/api2/json/ping >/dev/null; then
    printf '{"fingerprint":"%s","secret":"%s"}\n' "$fingerprint" "$secret" >&3
    unset secret
    exec 3>&-
    wait -n "$api" "$proxy"
    exit 1
  fi
  kill -0 "$api" "$proxy"
  sleep .5
done
exit 1
