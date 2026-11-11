#!/usr/bin/env bash
# Run only inside the disposable boot VM; /dev/vda3 is its test data partition.
set -euo pipefail
base=/run/pbs-spike
scripts=/run/live/medium/live/spike
mkdir -p "$base"/{lower,runtime,data}
mount -t squashfs -o loop,ro /run/live/medium/live/spike/pbs.squashfs "$base/lower"
mount -o nodev,nosuid,noexec /dev/vda3 "$base/data"
mkdir -p "$base/data/nestegg"
chown 34:34 "$base/data/nestegg"
unshare --mount --pid --net --uts --fork --kill-child "$scripts/namespace.sh" "$base" "$base/data" "$scripts" normal >"$base/server.log" 2>&1 &
launcher=$!
leader=
ready=0
for ((i=0;i<120;i++)); do
  leader=$(awk '{print $1}' "/proc/$launcher/task/$launcher/children" 2>/dev/null || true)
  if [[ -n $leader ]] && nsenter -t "$leader" -m chroot "$base/runtime/root" test -f /run/spike-ready 2>/dev/null; then
    ready=1
    printf 'VM-PBS-%s\n' STARTED
    nsenter -t "$leader" -n ip -br link
    nsenter -t "$leader" -n ss -lnt
    break
  fi
  if ! kill -0 "$launcher" 2>/dev/null; then
    printf 'VM-PBS-%s\n' FAILED
    tail -n 16 "$base/server.log"
    break
  fi
  sleep .5
done
if [[ $ready == 1 && -x $scripts/proxmox-backup-client ]]; then
  root=$base/runtime/root
  nsenter -t "$leader" -m chroot "$root" cat /run/token-secret >"$base/token"
  fingerprint=$(nsenter -t "$leader" -m chroot "$root" cat /run/fingerprint)
  mkdir "$base/input"
  printf 'Debian VM fixture\n' >"$base/input/report.txt"
  "$scripts/proxmox-backup-client" key create "$base/key.json" --kdf none >/dev/null 2>&1
  epoch=$(date +%s)
  snapshot="host/vm-desktop/$(date -u -d "@$epoch" +%FT%TZ)"
  vm_client() {
    nsenter -t "$leader" -n env PBS_REPOSITORY='nestegg@pbs!stick@127.0.0.1:stick' \
      PBS_PASSWORD_FD=9 PBS_FINGERPRINT="$fingerprint" HOME="$base" \
      "$scripts/proxmox-backup-client" "$@" 9<"$base/token"
  }
  vm_client backup "files.pxar:$base/input" --backup-id vm-desktop --backup-time "$epoch" --keyfd 7 7<"$base/key.json" >"$base/backup.log" 2>&1
  vm_client restore "$snapshot" files.pxar "$base/restore" --keyfd 7 7<"$base/key.json" >"$base/restore.log" 2>&1
  diff -r "$base/input" "$base/restore"
  printf 'VM-BACKUP-%s\n' RESTORE-OK
fi
if ! grep -q 'ipv6.disable=1' /proc/cmdline; then
  echo 'Attempt actual PBS launch with nobody mapped to namespace root:'
  # The child shell expands $1 against its passed root path.
  # shellcheck disable=SC2016
  if runuser -u nobody -- unshare --user --map-root-user --mount --net --pid --fork /bin/bash -c '
    mount --make-rprivate /
    mount -t tmpfs tmpfs "$1/etc/proxmox-backup"
    mount -t tmpfs tmpfs "$1/run"
    mount -t tmpfs tmpfs "$1/var/log"
    mkdir "$1/run/proxmox-backup" "$1/var/log/proxmox-backup"
    chroot "$1" timeout 10 /usr/lib/x86_64-linux-gnu/proxmox-backup/proxmox-backup-api
    chroot "$1" setpriv --reuid=backup --regid=backup --init-groups /usr/lib/x86_64-linux-gnu/proxmox-backup/proxmox-backup-proxy
  ' sh "$base/lower"; then
    echo 'VM-USERNS-PBS-STARTED'
  else
    echo 'VM-USERNS-PBS-FAILED'
  fi
fi
[[ -z $leader ]] || kill -KILL "$leader" 2>/dev/null || true
wait "$launcher" 2>/dev/null || true
umount "$base/data" "$base/lower"
printf 'VM-PROBE-%s\n' DONE
