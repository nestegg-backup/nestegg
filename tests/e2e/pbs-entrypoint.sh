#!/bin/bash
# Start the PBS daemons without systemd.
set -e
LIB=/usr/lib/x86_64-linux-gnu/proxmox-backup
mkdir -p /run/proxmox-backup /var/log/proxmox-backup /etc/proxmox-backup
chown -R backup:backup /run/proxmox-backup /var/log/proxmox-backup
chown backup:backup /etc/proxmox-backup && chmod 0700 /etc/proxmox-backup
"$LIB/proxmox-backup-api" &
# The API creates keys and certificates on first start.
for i in $(seq 1 60); do [ -e /etc/proxmox-backup/proxy.pem ] && break; sleep 0.5; done
setpriv --reuid=backup --regid=backup --init-groups "$LIB/proxmox-backup-proxy" &
wait -n
