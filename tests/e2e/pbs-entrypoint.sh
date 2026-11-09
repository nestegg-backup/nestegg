#!/bin/bash
# Start the PBS daemons without systemd.
set -e
LIB=/usr/lib/x86_64-linux-gnu/proxmox-backup
mkdir -p /run/proxmox-backup /var/log/proxmox-backup /etc/proxmox-backup
chown -R backup:backup /run/proxmox-backup /var/log/proxmox-backup
chown backup:backup /etc/proxmox-backup && chmod 0700 /etc/proxmox-backup
"$LIB/proxmox-backup-api" &
# The API creates keys and certificates on first start; wait until both the
# certificate and its key are complete before the proxy reads them.
for i in $(seq 1 120); do
  [ -s /etc/proxmox-backup/proxy.pem ] && [ -s /etc/proxmox-backup/proxy.key ] && break
  sleep 0.5
done
sleep 1
setpriv --reuid=backup --regid=backup --init-groups "$LIB/proxmox-backup-proxy" &
wait -n
