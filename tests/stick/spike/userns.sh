#!/usr/bin/env bash
# Actual PBS launch under a single-ID unprivileged-root mapping, in a disposable
# privileged container so the host's AppArmor userns policy is not altered.
set -euo pipefail
if unshare --user --map-root-user true 2>/dev/null; then
  echo 'Host permits unprivileged single-ID user namespaces'
else
  echo 'Host denies unprivileged single-ID user namespaces'
fi
docker run --rm -i --privileged --network none --entrypoint /bin/bash nestegg-e2e-pbs -s <<'INNER'
set -eu
runuser -u nobody -- unshare --user --map-root-user --mount --pid --fork --net /bin/bash -c '
set -u
mount --make-rprivate /
for dir in /etc/proxmox-backup /run/proxmox-backup /var/log/proxmox-backup; do
  mount -t tmpfs tmpfs "$dir"
done
printf "Mapped namespace identity: "; id
printf "Mapped namespace uid_map: "; cat /proc/self/uid_map
printf "API launch: "
timeout 15 /usr/lib/x86_64-linux-gnu/proxmox-backup/proxmox-backup-api
printf "Proxy identity switch: "
setpriv --reuid=backup --regid=backup --init-groups /usr/lib/x86_64-linux-gnu/proxmox-backup/proxmox-backup-proxy
'
INNER
