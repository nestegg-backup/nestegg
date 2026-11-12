#!/usr/bin/env bash
# Reuse a private ext4 partition image, never an arbitrary disk or live mount.
set -euo pipefail
[[ $EUID == 0 ]] || exit 2
image=$(realpath "${1:?usage: sudo reuse.sh PRIVATE_EXT4_IMAGE}")
[[ -f $image && ! -b $image ]] || exit 2
name="nestegg-stick-reuse-$$"
work=$(mktemp -d /tmp/nestegg-stick-reuse.XXXXXX)
loop=
cleanup() {
  local status=$?
  trap - EXIT
  trap '' INT TERM
  set +e
  if docker inspect "$name" >/dev/null 2>&1; then
    if ! docker rm -f "$name" >/dev/null; then
      echo "ERROR container removal failed; owned image retained at $work" >&2
      exit 1
    fi
  fi
  if [[ -n $loop ]] && ! losetup -d "$loop"; then
    echo "ERROR loop detach failed; owned image retained at $work" >&2
    exit 1
  fi
  rm -rf "$work" || exit 1
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
bytes=$(stat -c %s "$image")
[[ $((bytes % 512)) == 0 ]]
truncate -s "$((bytes + 4*1024*1024))" "$work/gpt.img"
sgdisk --clear --new="1:2048:$((2048 + bytes/512 - 1))" --typecode=1:8300 "$work/gpt.img" >/dev/null
dd if="$image" of="$work/gpt.img" bs=16M oflag=seek_bytes seek=1048576 conv=notrunc,sparse status=none
loop=$(losetup --find --show --partscan "$work/gpt.img")
device=${loop}p1
# Device nodes/udev can lag behind LOOP_SET_STATUS and partition discovery.
uuid=
for ((i=0;i<100;i++)); do
  if [[ -b $device ]] && uuid=$(blkid -p -s UUID -o value "$device") && [[ -n $uuid ]]; then break; fi
  sleep .1
done
[[ -n $uuid ]] || { echo 'ERROR owned partition not ready' >&2; exit 1; }
# Docker's /dev is private. Do not bind the host /dev or publish any ports.
docker run -d --rm --cap-add SYS_ADMIN --security-opt apparmor=unconfined --network none --tmpfs /run --hostname pbs.example.com \
  --name "$name" --device "$device" nestegg-e2e-pbs >/dev/null
for ((i=0;i<60;i++)); do
  docker exec "$name" curl -skf https://127.0.0.1:8007/api2/json/ping >/dev/null 2>&1 && break
  sleep .5
done
docker exec "$name" mkdir -p /dev/disk/by-uuid
docker exec "$name" ln -sf "$device" "/dev/disk/by-uuid/$uuid"
docker exec "$name" proxmox-backup-manager datastore create removable nestegg \
  --backing-device "$uuid" --reuse-datastore true
docker exec "$name" proxmox-backup-manager datastore show removable --output-format json
# Verify actual old snapshots, not just a successful configuration write.
docker exec "$name" proxmox-backup-manager verify-job create imported --store removable --ignore-verified false
docker exec "$name" proxmox-backup-manager verify-job run imported
docker exec "$name" proxmox-backup-manager datastore unmount removable
