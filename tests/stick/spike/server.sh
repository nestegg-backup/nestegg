#!/usr/bin/env bash
set -euo pipefail
umask 022
lib=/usr/lib/x86_64-linux-gnu/proxmox-backup
mkdir -p /run/proxmox-backup /var/log/proxmox-backup /etc/proxmox-backup /root/.cache
chown -R backup:backup /run/proxmox-backup /var/log/proxmox-backup /etc/proxmox-backup
chmod 0700 /etc/proxmox-backup
"$lib/proxmox-backup-api" &
api=$!
trap 'kill "$api" "${proxy:-}" 2>/dev/null || true; wait || true' EXIT
for ((i=0;i<120;i++)); do
  [[ -s /etc/proxmox-backup/proxy.pem && -s /etc/proxmox-backup/proxy.key ]] && break
  sleep .5
done
sleep 1
setpriv --reuid=backup --regid=backup --init-groups "$lib/proxmox-backup-proxy" &
proxy=$!
# A new secret and certificate are intentionally generated on EVERY start.
proxmox-backup-manager user create nestegg@pbs
(umask 077; proxmox-backup-manager user generate-token nestegg@pbs stick > /run/token.json)
sed -n 's/.*"value": *"\([^"]*\)".*/\1/p' /run/token.json > /run/token-secret
[[ -s /run/token-secret ]]
chmod 0600 /run/token-secret
rm /run/token.json
proxmox-backup-manager acl update /datastore/stick DatastoreAdmin --auth-id nestegg@pbs
proxmox-backup-manager acl update /datastore/stick DatastoreAdmin --auth-id 'nestegg@pbs!stick'
if [[ -d /mnt/stick/nestegg/.chunks ]]; then
  proxmox-backup-manager datastore create stick /mnt/stick/nestegg --reuse-datastore true --tuning gc-atime-cutoff=1
else
  proxmox-backup-manager datastore create stick /mnt/stick/nestegg --tuning gc-atime-cutoff=1
fi
openssl x509 -in /etc/proxmox-backup/proxy.pem -noout -fingerprint -sha256 | cut -d= -f2 > /run/fingerprint
for ((i=0;i<120;i++)); do
  if curl -skf https://127.0.0.1:8007/api2/json/ping >/dev/null; then
    touch /run/spike-ready
    break
  fi
  kill -0 "$proxy" || exit 1
  sleep .5
done
wait -n "$api" "$proxy"
