#!/usr/bin/env bash
# End-to-end test of the bare-metal restore, entirely in disposable VMs:
#
#   1. a disposable PBS in Docker (same as tests/e2e),
#   2. a freshly installed "desktop" VM backs itself up with nestegg,
#   3. the rescue ISO boots in a second VM with an EMPTY disk and restores
#      the backup onto it (driven over the serial console),
#   4. the restored disk boots on its own and checks itself.
#
# Usage: sudo tests/rescue/run.sh <nestegg-binary> <rescue.iso> [variant]
# Variants: debian-btrfs-uefi (default), ubuntu-ext4-bios-luks
# Needs: root, KVM, Docker, qemu-system-x86, OVMF. Touches nothing outside
# its temporary directory and the containers/VMs it starts. KEEP=1 keeps
# the work directory for inspection.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
bin="$(realpath "${1:?nestegg binary}")"
iso="$(realpath "${2:?rescue ISO}")"
variant="${3:-debian-btrfs-uefi}"
[[ $EUID -eq 0 ]] || { echo "run as root (loop devices, KVM)"; exit 2; }
[[ -e /dev/kvm ]] || { echo "KVM is required"; exit 2; }

case "$variant" in
  debian-btrfs-uefi) source_variant=debian-btrfs-uefi; builder=debian:trixie; uefi=1; luks=0 ;;
  ubuntu-ext4-bios-luks) source_variant=ubuntu-ext4-bios; builder=ubuntu:24.04; uefi=0; luks=1 ;;
  *) echo "unknown variant $variant"; exit 2 ;;
esac

net="nestegg-rescue-$$"
pbs="nestegg-rescue-pbs-$$"
client="unused"
work="$(mktemp -d /var/tmp/nestegg-rescue-e2e.XXXXXX)"
qemu_pids=()
# shellcheck source=../e2e/lib.sh
source "$here/../e2e/lib.sh"
e2e="$(cd "$here/../e2e" && pwd)"
build_pbs_image() {
  DOCKER_BUILDKIT=0 docker build -q -t nestegg-e2e-pbs -f "$e2e/pbs.Dockerfile" "$e2e" >/dev/null
}

cleanup() {
  set +e
  for p in "${qemu_pids[@]}"; do kill "$p" 2>/dev/null; done
  docker rm -f "$pbs" >/dev/null 2>&1
  docker network rm "$net" >/dev/null 2>&1
  if [[ "${KEEP:-0}" == 1 ]]; then echo "work directory kept: $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT

ovmf_code=/usr/share/OVMF/OVMF_CODE_4M.fd
ovmf_vars=/usr/share/OVMF/OVMF_VARS_4M.fd
firmware() { # $1 = name of a fresh NVRAM copy
  if [[ $uefi == 1 ]]; then
    cp "$ovmf_vars" "$work/$1.fd"
    echo "-drive if=pflash,format=raw,readonly=on,file=$ovmf_code -drive if=pflash,format=raw,file=$work/$1.fd"
  fi
}
qemu() { # $1 = time limit in seconds, rest = qemu arguments
  local limit="$1"; shift
  timeout "$limit" qemu-system-x86_64 -enable-kvm -cpu host -m 2048 -smp 2 -display none -monitor none \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 "$@"
}

# Later stages make no sense once an earlier one failed.
stop_on_failure() { if [[ $fail -gt 0 ]]; then result; exit 1; fi; }

echo "== variant $variant"
build_pbs_image
start_pbs
pbs_ip="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$pbs")"
echo "PBS at $pbs_ip"

echo "== building the source system ($source_variant)"
cp "$bin" "$work/nestegg"
docker run --rm --privileged -v /dev:/dev -v "$work:/work" -v "$here:/t:ro" \
  -e VARIANT="$source_variant" -e IMAGE=/work/source.img -e PBS_IP="$pbs_ip" -e FP="$fp" \
  -e TOKEN_SECRET="$token_secret" "$builder" bash /t/make-source.sh

echo "== source VM: first start runs the backup"
# shellcheck disable=SC2046
qemu 1200 $(firmware src) -drive "file=$work/source.img,if=virtio,format=raw" \
  -serial "file:$work/source-console.log" || true
check "source system backed up" grep -q E2E-BACKUP-OK "$work/source-console.log"
check "layout recorded" grep -q '"bootloader": "grub"' "$work/source-console.log"
snaps="$(docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$pbs" \
  proxmox-backup-client snapshot list --ns desktops --repository admin@pbs@127.0.0.1:store1 --output-format json)"
check "snapshot contains system.pxar" grep -q 'system.pxar' <<<"$snaps"
stop_on_failure

echo "== rescue VM: restore onto an empty disk"
truncate -s 12G "$work/target.img"
printf '%s' "$token_secret" > "$work/password"
printf 'correct horse battery staple' > "$work/luks"
docker run --rm -v "$work:/work" debian:trixie bash -euo pipefail -c '
  apt-get update -qq && apt-get install -y -qq dosfstools mtools >/dev/null
  mkfs.vfat -C /work/media.img 16384 >/dev/null
  mcopy -i /work/media.img /work/nestegg-rescue.toml /work/nestegg-key.json /work/password /work/luks ::/
'
encrypt_arg=""
[[ $luks == 1 ]] && encrypt_arg="--encrypt-passphrase-file /media/t/luks"
cat > "$work/rescue.script" <<EOF
expect root@nestegg-rescue[^#]*# 600
send mkdir -p /media/t && mount -o ro /dev/vdb /media/t && nestegg rescue restore --settings /media/t/nestegg-rescue.toml --password-file /media/t/password --keyfile /media/t/nestegg-key.json --disk /dev/vda --wipe /dev/vda $encrypt_arg; echo RESTORE-EXIT=\$?
expect RESTORE-EXIT=[0-9]+ 2400
send poweroff
EOF
# shellcheck disable=SC2046
qemu 3600 $(firmware rescue) -cdrom "$iso" -boot d \
  -drive "file=$work/target.img,if=virtio,format=raw" \
  -drive "file=$work/media.img,if=virtio,format=raw" \
  -serial "unix:$work/rescue.sock,server=on,wait=off" &
qemu_pids+=($!)
python3 "$here/console.py" "$work/rescue.sock" "$work/rescue-console.log" "$work/rescue.script" || true
wait "${qemu_pids[-1]}" || true
check "rescue restore succeeded" grep -q 'RESTORE-EXIT=0' "$work/rescue-console.log"
check "restore finished message" grep -q 'Restore finished' "$work/rescue-console.log"
stop_on_failure

echo "== restored disk boots on its own (fresh firmware, no rescue stick)"
cat > "$work/boot.script" <<EOF
EOF
if [[ $luks == 1 ]]; then
  printf 'expect (Please unlock disk|passphrase|Passphrase)[^\\n]* 600\nsendfile %s\n' "$work/luks" > "$work/boot.script"
fi
echo "expect E2E-DONE 900" >> "$work/boot.script"
# shellcheck disable=SC2046
qemu 1200 $(firmware restored) -drive "file=$work/target.img,if=virtio,format=raw" \
  -serial "unix:$work/boot.sock,server=on,wait=off" &
qemu_pids+=($!)
python3 "$here/console.py" "$work/boot.sock" "$work/boot-console.log" "$work/boot.script" || true
wait "${qemu_pids[-1]}" || true
log="$work/boot-console.log"
val() { grep -a "^$1 " "$log" | tail -1 | cut -d' ' -f2- | tr -d '\r'; }
check "restored system booted" grep -aq E2E-RESTORED-BOOT "$log"
check "user data intact (sha256)" [ "$(val E2E-SHA)" == "$(cat "$work/data.sha256")" ]
check "owner preserved" [ "$(val E2E-OWNER)" == alice ]
check "file capability preserved" grep -q cap_net_raw <<<"$(val E2E-CAP)"
check "system file restored" [ "$(val E2E-MARKER)" == "system marker" ]
check "/tmp content excluded" [ "$(val E2E-TMPJUNK)" == 0 ]
check "no failed units" [ -z "$(val E2E-FAILED-UNITS | tr -d ' ')" ]
if [[ $variant == debian-btrfs-uefi ]]; then
  check "root on btrfs subvolume @" grep -Eq 'subvol=/@(,|$)' <<<"$(val E2E-ROOT)"
  check "/home on subvolume @home" grep -q 'subvol=/@home' <<<"$(val E2E-HOME)"
  check "/.snapshots subvolume recreated" grep -q 'subvol=/@snapshots' <<<"$(val E2E-SNAPSHOTS)"
  check "EFI system partition mounted" [ "$(val E2E-ESP)" == vfat ]
  check "swap partition active" grep -q partition <<<"$(val E2E-SWAP)"
else
  check "root on ext4" grep -q '^ext4' <<<"$(val E2E-ROOT)"
  check "root is encrypted" [ "$(val E2E-ROOTDEV)" == /dev/mapper/nestegg_root ]
  check "separate /boot" [ "$(val E2E-BOOTPART)" == ext4 ]
  check "old /home partition merged into /" [ -z "$(val E2E-HOME)" ]
  check "swap file active" grep -q file <<<"$(val E2E-SWAP)"
fi
result
