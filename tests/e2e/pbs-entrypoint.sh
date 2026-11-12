#!/bin/bash
# Start the PBS daemons without systemd.
set -e
LIB=/usr/lib/x86_64-linux-gnu/proxmox-backup
mkdir -p /run/proxmox-backup /var/log/proxmox-backup /etc/proxmox-backup
chown -R backup:backup /run/proxmox-backup /var/log/proxmox-backup
chown backup:backup /etc/proxmox-backup && chmod 0700 /etc/proxmox-backup
"$LIB/proxmox-backup-api" &
# The API creates keys and certificates on first start; wait until both the
# certificate, authentication and CSRF keys are complete and the API accepts
# requests before the proxy reads them. A fixed sleep races slower hosts.
for i in $(seq 1 120); do
  if [ -s /etc/proxmox-backup/proxy.pem ] && [ -s /etc/proxmox-backup/proxy.key ] \
     && [ -s /etc/proxmox-backup/authkey.pub ] && [ -s /etc/proxmox-backup/authkey.key ] \
     && [ -s /etc/proxmox-backup/csrf.key ] \
     && { : <>/dev/tcp/127.0.0.1/82; } 2>/dev/null; then
    break
  fi
  sleep 0.5
done
setpriv --reuid=backup --regid=backup --init-groups "$LIB/proxmox-backup-proxy" &
wait -n
