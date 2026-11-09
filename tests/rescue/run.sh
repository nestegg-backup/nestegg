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
# The interactive wizard is driven like a person would (WIZARD=0: the
# non-interactive `nestegg rescue restore` instead). The encryption key
# reaches the wizard as a saved paper-key file on the media
# (debian-btrfs-uefi) or typed in by hand (ubuntu-ext4-bios-luks); a wrong
# key must be refused before the disk is touched, in both ways.
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
  debian-btrfs-uefi) source_variant=debian-btrfs-uefi; builder=debian:trixie; uefi=1; luks=0; key_mode="file" ;;
  ubuntu-ext4-bios-luks) source_variant=ubuntu-ext4-bios; builder=ubuntu:24.04; uefi=0; luks=1; key_mode="typed" ;;
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
# Key material for the media. The right key is the one of the backup; the
# wrong one is a valid key of the same format that never saw this backup.
sed -n 's/.*"data": *"\([^"]*\)".*/\1/p' "$work/nestegg-key.json" > "$work/key-data.txt"
[[ "$(tr -d '\n' < "$work/key-data.txt" | wc -c)" -eq 44 ]] || { echo "unexpected key file"; exit 1; }
# Typed the way a person might: quotes, comma and blanks around it.
printf '  "%s",  \n' "$(cat "$work/key-data.txt")" > "$work/key-typed.txt"
head -c 32 /dev/urandom | base64 > "$work/wrong-data.txt"
{
  echo "nestegg test: a paper key of a different key (never used for a backup)"
  echo
  echo "-----BEGIN PROXMOX BACKUP KEY-----"
  printf '{\n  "kdf": null,\n  "created": "2026-01-01T00:00:00+00:00",\n  "modified": "2026-01-01T00:00:00+00:00",\n  "data": "%s"\n}\n' "$(cat "$work/wrong-data.txt")"
  echo "-----END PROXMOX BACKUP KEY-----"
  echo "(QR code)"
} > "$work/wrong-paperkey.txt"
# In wizard mode the settings carry no certificate fingerprint, so that the
# fingerprint question is asked (and Enter must not trust the certificate).
if [[ "${WIZARD:-1}" == 1 ]]; then
  grep -v '^fingerprint' "$work/nestegg-rescue.toml" > "$work/settings-nofp.toml"
else
  cp "$work/nestegg-rescue.toml" "$work/settings-nofp.toml"
fi
# Only the paper key (key_mode "file") is in the top directory, where the
# wizard finds it by itself. Keys three levels down are not searched; they
# are reached by path.
docker run --rm -e KEY_MODE="$key_mode" -v "$work:/work" debian:trixie bash -euo pipefail -c '
  apt-get update -qq && apt-get install -y -qq dosfstools mtools >/dev/null
  mkfs.vfat -C /work/media.img 16384 >/dev/null
  mcopy -i /work/media.img /work/settings-nofp.toml ::nestegg-rescue.toml
  mcopy -i /work/media.img /work/password /work/luks ::/
  mmd -i /work/media.img ::x ::x/y ::x/y/z
  mcopy -i /work/media.img /work/nestegg-key.json /work/wrong-paperkey.txt ::x/y/z/
  if [ "$KEY_MODE" = file ]; then
    mcopy -i /work/media.img /work/nestegg-paperkey.txt ::/
  fi
'
encrypt_arg=""
[[ $luks == 1 ]] && encrypt_arg="--encrypt-passphrase-file /media/t/luks"
if [[ "${WIZARD:-1}" == 1 ]]; then
  # Drive the interactive wizard (`nestegg rescue`) like a person would,
  # including typical mistakes. Before it: a wrong key given to the
  # non-interactive restore, with permission to erase, must be refused with
  # the disk untouched. First wizard pass: cancelled at the final
  # confirmation (nothing may change). Second pass: complete restore.
  restore_args="--settings /media/t/nestegg-rescue.toml --fingerprint $fp --password-file /media/t/password --disk /dev/vda --wipe /dev/vda $encrypt_arg"
  connection_defaults() { # $1 = answer to the certificate question, or "saved"
    for q in "PBS address" "Port" "Datastore" "Namespace" "User or API token" "MAC address"; do
      printf 'expect %s 120\nsend\n' "$q"
    done
    if [[ $1 == saved ]]; then
      printf 'expect matches the saved settings 120\n'
    else
      printf 'expect Trust this certificate 120\nsend %s\n' "$1"
    fi
  }
  key_from_media() { # the wizard offers the paper key it found; Enter takes it
    printf 'expect Key files and paper keys found on attached media 60\nexpect nestegg-paperkey.txt 30\nexpect Encryption key 30\nsend\n'
  }
  key_typed() {
    printf 'expect Encryption key 60\nsend type\nexpect Key \\(empty line = back\\) 30\nsendfile %s\nexpect Key accepted 30\n' "$work/key-typed.txt"
  }
  right_key() { if [[ $key_mode == file ]]; then key_from_media; else key_typed; fi; }
  # The wrong key is given the other way round: typed where the right one
  # is a file, as a file where the right one is typed.
  wrong_key() {
    if [[ $key_mode == file ]]; then
      printf 'expect Encryption key 60\nsend type\nexpect Key \\(empty line = back\\) 30\n'
      printf 'send abc_defghijkl\nexpect cannot be part of a key 30\n'
      printf 'send ABCDEFGH\nexpect got 8 characters 30\n'
      printf 'sendfile %s\nexpect Key accepted 30\n' "$work/wrong-data.txt"
    else
      printf 'expect Encryption key 60\nsend /media/t/x/y/z/wrong-paperkey.txt\n'
    fi
  }
  encryption_answers() { # $1 = 1: also try mismatching passphrases
    if [[ $luks == 1 ]]; then
      printf 'expect Encrypt the restored disk 60\nsend y\n'
      if [[ $1 == 1 ]]; then
        printf 'expect New disk passphrase 30\nsendfile %s\nexpect Repeat the passphrase 30\nsend not-the-same\nexpect passphrases differ 30\n' "$work/luks"
      fi
      printf 'expect New disk passphrase 30\nsendfile %s\nexpect Repeat the passphrase 30\nsendfile %s\n' "$work/luks" "$work/luks"
    else
      printf 'expect Encrypt the restored disk 60\nsend n\n'
    fi
  }
  {
    echo 'expect root@nestegg-rescue[^#]*# 600'
    echo 'send mkdir -p /media/t && mount -o ro /dev/vdb /media/t; echo MOUNT-EXIT=$?'
    echo 'expect MOUNT-EXIT=0 60'
    echo '# A wrong key for the non-interactive restore, WITH permission to erase:'
    echo '# it must be refused before the disk is touched.'
    echo "send nestegg rescue restore $restore_args --keyfile /media/t/x/y/z/wrong-paperkey.txt; echo WRONGKEY-EXIT=\$?"
    echo 'expect wrong key 900'
    echo 'expect WRONGKEY-EXIT=[0-9]+ 60'
    echo 'send head -c 16777216 /dev/vda | tr -d "\0" | wc -c | sed "s/^/DISK-DIRTY-BYTES=/"'
    echo 'expect DISK-DIRTY-BYTES=[0-9]+ 120'
    echo '# The right key as a plain JSON file, dry run: reads the backup, changes nothing.'
    echo "send nestegg rescue restore $restore_args --keyfile /media/t/x/y/z/nestegg-key.json --dry-run; echo DRYRUN-EXIT=\$?"
    echo 'expect DRYRUN-EXIT=[0-9]+ 900'
    echo 'send nestegg rescue; echo WIZARD-EXIT=$?'
    echo 'expect Found settings in /media/t 120'
    echo '# Round 1: Enter at the certificate question must NOT trust the certificate'
    connection_defaults ''
    echo 'expect Password or API token secret 120'
    printf 'sendfile %s\n' "$work/password"
    echo 'expect Encryption key 60'
    echo 'send -'
    echo 'expect Login or listing failed 300'
    echo 'expect No certificate was trusted 30'
    echo '# Round 2: trusted explicitly. Mistake: just Enter at the password prompt -> asks again'
    connection_defaults y
    echo 'expect Password or API token secret 120'
    echo 'send'
    echo 'expect Please enter a value 30'
    echo 'expect Password or API token secret 30'
    echo '# Mistake: wrong secret'
    echo 'send wrong-secret-for-the-test'
    echo '# Mistake: key file that does not exist -> must ask again'
    echo 'expect Encryption key 60'
    echo 'send /nonexistent/nestegg-key.json'
    echo 'expect Encryption key 30'
    echo 'send -'
    echo 'expect Login or listing failed 300'
    echo '# Round 3: the wizard starts over; the trusted fingerprint is remembered'
    connection_defaults saved
    echo 'expect Password or API token secret 120'
    printf 'sendfile %s\n' "$work/password"
    echo '# Mistake: the wrong key. It is refused when the backup is chosen.'
    wrong_key
    echo '# Mistake: number out of range'
    echo 'expect Which computer 300'
    echo 'send 9'
    echo 'expect Please enter a number between 30'
    echo 'expect Which computer 30'
    echo 'send 1'
    echo 'expect Which backup 300'
    echo 'send'
    echo 'expect Reading the disk layout 120'
    echo 'expect (wrong key|does not match provided key) 600'
    echo 'expect The encryption key does not fit this backup 30'
    echo 'expect Nothing was changed 30'
    echo 'expect What now 30'
    echo '# Pick another key (the default of this menu)'
    echo 'send'
    right_key
    echo 'expect The key fits this backup 600'
    echo 'expect Step 3/4 60'
    echo 'expect 1\) /dev/vda 120'
    echo 'expect Which disk 60'
    echo 'send'
    encryption_answers 1
    echo '# Mistake: wrong disk name at the final confirmation -> cancel'
    echo "expect Type 'vda' to erase 900"
    echo 'send sda'
    echo 'expect Cancelled, nothing was changed 60'
    echo 'expect WIZARD-EXIT=1 60'
    echo '# Second pass: everything right'
    echo 'send nestegg rescue; echo WIZARD-EXIT=$?'
    echo 'expect Found settings in /media/t 120'
    connection_defaults y
    echo 'expect Password or API token secret 120'
    printf 'sendfile %s\n' "$work/password"
    right_key
    echo 'expect Which computer 300'
    echo 'send'
    echo 'expect Which backup 300'
    echo 'send'
    echo 'expect The key fits this backup 600'
    echo 'expect Which disk 120'
    echo 'send'
    encryption_answers 0
    echo "expect Type 'vda' to erase 900"
    echo 'send vda'
    echo 'expect Restore finished 2400'
    echo 'expect Remove the rescue stick and restart now 60'
    echo 'send n'
    echo 'expect WIZARD-EXIT=0 60'
    echo 'send poweroff'
  } > "$work/rescue.script"
else
  key_arg=/media/t/x/y/z/nestegg-key.json
  [[ $key_mode == file ]] && key_arg=/media/t/nestegg-paperkey.txt
  cat > "$work/rescue.script" <<EOF
expect root@nestegg-rescue[^#]*# 600
send mkdir -p /media/t && mount -o ro /dev/vdb /media/t && nestegg rescue restore --settings /media/t/nestegg-rescue.toml --password-file /media/t/password --keyfile $key_arg --disk /dev/vda --wipe /dev/vda $encrypt_arg; echo RESTORE-EXIT=\$?
expect RESTORE-EXIT=[0-9]+ 2400
send poweroff
EOF
fi
# shellcheck disable=SC2046
qemu 3600 $(firmware rescue) -cdrom "$iso" -boot d \
  -drive "file=$work/target.img,if=virtio,format=raw" \
  -drive "file=$work/media.img,if=virtio,format=raw" \
  -serial "unix:$work/rescue.sock,server=on,wait=off" &
qemu_pids+=($!)
python3 "$here/console.py" "$work/rescue.sock" "$work/rescue-console.log" "$work/rescue.script" || true
wait "${qemu_pids[-1]}" || true
if [[ "${WIZARD:-1}" == 1 ]]; then
  rlog="$work/rescue-console.log"
  # Output of the non-interactive restore with the wrong key (up to its exit code).
  wrong_part="$work/wrong-key-part.log"
  sed -n '/restore --settings/,/WRONGKEY-EXIT=[0-9]/p' "$rlog" > "$wrong_part"
  check "wrong key (non-interactive): refused because of the key" grep -aq 'wrong key' "$wrong_part"
  check "wrong key (non-interactive): exit code is an error" grep -aq 'WRONGKEY-EXIT=[1-9]' "$rlog"
  check "wrong key (non-interactive): disk still untouched" grep -aq 'DISK-DIRTY-BYTES=0' "$rlog"
  check "right key as JSON file: dry run works" grep -aq 'DRYRUN-EXIT=0' "$rlog"
  check "wizard: Enter does not trust the certificate" grep -aq 'No certificate was trusted' "$rlog"
  check "wizard: trusted fingerprint is remembered" grep -aq 'matches the saved settings' "$rlog"
  check "wizard: empty password input asks again" grep -aq 'Please enter a value' "$rlog"
  check "wizard: missing key file asks again" grep -aq 'nonexistent/nestegg-key.json' "$rlog"
  check "wizard: wrong secret starts over" grep -aq 'Login or listing failed' "$rlog"
  check "wizard: invalid number asks again" grep -aq 'Please enter a number between' "$rlog"
  check "wizard: wrong key refused with a message" grep -aq 'The encryption key does not fit this backup' "$rlog"
  check "wizard: right key accepted" grep -aq 'The key fits this backup' "$rlog"
  # The wrong key is refused before the disk list (Step 3/4) is shown.
  check "wizard: wrong key refused before the disk step" \
    awk '/does not fit this backup/ {refused=1} /Step 3\/4/ && !refused {early=1} END {exit !(refused && !early)}' "$rlog"
  if [[ $key_mode == file ]]; then
    check "wizard: paper key found on the media" grep -aq 'found on attached media' "$rlog"
    check "wizard: typed key with a bad character asks again" grep -aq 'cannot be part of a key' "$rlog"
    check "wizard: typed key of the wrong length asks again" grep -aq 'got 8 characters' "$rlog"
  else
    check "wizard: key typed by hand accepted" grep -aq 'Key accepted' "$rlog"
  fi
  if [[ $luks == 1 ]]; then
    check "wizard: different passphrases ask again" grep -aq 'passphrases differ' "$rlog"
  fi
  check "wizard: wrong confirmation cancels" grep -aq 'Cancelled, nothing was changed' "$rlog"
  check "wizard: first pass ended without changes (exit 1)" grep -aq 'WIZARD-EXIT=1' "$rlog"
  check "wizard: second pass succeeded" grep -aq 'WIZARD-EXIT=0' "$rlog"
  check "wizard: token secret never shown" bash -c "! grep -aqF \"\$(cat '$work/password')\" '$rlog'"
else
  check "rescue restore succeeded" grep -q 'RESTORE-EXIT=0' "$work/rescue-console.log"
fi
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
