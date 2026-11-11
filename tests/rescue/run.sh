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
# key must be refused before the disk is touched, in both ways. Two newer
# backups of the same computer are pushed to the PBS as someone with write
# access to it could: an unencrypted "decoy" and one that is only signed with
# the right key (`--crypt-mode sign-only`, with a copy of the real disk
# layout). With a key at hand both must be refused unless explicitly
# accepted; a backup that can never be restored must lead to "choose another
# backup", not to "try again". (The real client makes one crypt mode per
# backup run, so a backup that mixes modes cannot be produced here; that case
# is covered by unit tests.)
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
# Path of the real (encrypted) backup, e.g. host/e2e-src/2026-11-09T20:54:47Z.
real_snap="$(python3 -c 'import json, sys, datetime
s = [x for x in json.load(sys.stdin) if x["backup-id"] == "e2e-src"][0]
t = datetime.datetime.fromtimestamp(s["backup-time"], datetime.timezone.utc)
print("host/e2e-src/" + t.strftime("%Y-%m-%dT%H:%M:%SZ"))' <<<"$snaps")"
echo "real backup: $real_snap"
echo "== pushing a backup that is only signed with the right key"
# Same disk layout as the real backup (restored from it with the key), but
# every archive signed instead of encrypted.
docker cp "$work/nestegg-key.json" "$pbs:/tmp/signkey.json"
docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$pbs" bash -c "
  proxmox-backup-client restore $real_snap inventory.pxar /tmp/signed-inventory \
    --keyfile /tmp/signkey.json --ns desktops --repository admin@pbs@127.0.0.1:store1" >/dev/null
docker exec -e PBS_PASSWORD="$token_secret" -e PBS_FINGERPRINT="$fp" "$pbs" bash -c '
  mkdir -p /tmp/signed-system && echo "signed, not encrypted" > /tmp/signed-system/README
  proxmox-backup-client backup system.pxar:/tmp/signed-system inventory.pxar:/tmp/signed-inventory \
    --ns desktops --backup-id e2e-src --keyfile /tmp/signkey.json --crypt-mode sign-only \
    --repository "backup@pbs!desktop@127.0.0.1:store1"' >/dev/null
sleep 2 # snapshot times have a resolution of one second
echo "== pushing an unencrypted decoy backup of the same computer"
docker exec -e PBS_PASSWORD="$token_secret" -e PBS_FINGERPRINT="$fp" "$pbs" bash -c '
  mkdir -p /tmp/decoy && echo "not the backed-up system" > /tmp/decoy/README
  proxmox-backup-client backup system.pxar:/tmp/decoy inventory.pxar:/tmp/decoy \
    --ns desktops --backup-id e2e-src --crypt-mode none \
    --repository "backup@pbs!desktop@127.0.0.1:store1"' >/dev/null
snaps="$(docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$pbs" \
  proxmox-backup-client snapshot list host/e2e-src --ns desktops --repository admin@pbs@127.0.0.1:store1 --output-format json)"
check "decoy is the newest backup and not encrypted; below it the signed one, then the real one" python3 -c 'import json, sys
s = sorted(json.load(sys.stdin), key=lambda x: -x["backup-time"])
assert len(s) == 3, s
assert all(f["crypt-mode"] == "none" for f in s[0]["files"]), s[0]
archives = [f for f in s[1]["files"] if f["filename"] in ("system.pxar.didx", "inventory.pxar.didx")]
assert len(archives) == 2 and all(f["crypt-mode"] == "sign-only" for f in archives), s[1]
assert any(f["crypt-mode"] == "encrypt" for f in s[2]["files"]), s[2]' <<<"$snaps"
stop_on_failure
# Path of the signed-only backup (the second newest).
signed_snap="$(python3 -c 'import json, sys, datetime
s = sorted(json.load(sys.stdin), key=lambda x: -x["backup-time"])[1]
t = datetime.datetime.fromtimestamp(s["backup-time"], datetime.timezone.utc)
print("host/e2e-src/" + t.strftime("%Y-%m-%dT%H:%M:%SZ"))' <<<"$snaps")"
echo "signed-only backup: $signed_snap"

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
# The settings file on the stick carries the certificate fingerprint, as
# `nestegg rescue export-settings` writes it. A second settings file one
# directory down has a different fingerprint (the PBS certificate was
# replaced, or it is not this PBS): the wizard must name both files and
# point out the difference.
grep -q '^fingerprint = ' "$work/nestegg-rescue.toml" || { echo "settings without fingerprint"; exit 1; }
other_fp="$(printf '00:%.0s' $(seq 31))00"
sed "s/^fingerprint = .*/fingerprint = \"$other_fp\"/" "$work/nestegg-rescue.toml" > "$work/settings-otherfp.toml"
# Only the paper key (key_mode "file") is in the top directory, where the
# wizard finds it by itself. Keys three levels down are not searched; they
# are reached by path.
docker run --rm -e KEY_MODE="$key_mode" -v "$work:/work" debian:trixie bash -euo pipefail -c '
  apt-get update -qq && apt-get install -y -qq dosfstools mtools >/dev/null
  mkfs.vfat -C /work/media.img 16384 >/dev/null
  mcopy -i /work/media.img /work/nestegg-rescue.toml ::nestegg-rescue.toml
  mcopy -i /work/media.img /work/password /work/luks /work/nestegg ::/
  mmd -i /work/media.img ::x ::x/y ::x/y/z
  mcopy -i /work/media.img /work/settings-otherfp.toml ::x/nestegg-rescue.toml
  mcopy -i /work/media.img /work/nestegg-key.json /work/wrong-paperkey.txt ::x/y/z/
  if [ "$KEY_MODE" = file ]; then
    mcopy -i /work/media.img /work/nestegg-paperkey.txt ::/
  fi
'
encrypt_arg=""
[[ $luks == 1 ]] && encrypt_arg="--encrypt-passphrase-file /media/t/luks"
settings=/media/t/nestegg-rescue.toml
other_settings=/media/t/x/nestegg-rescue.toml
# Writes the number of non-zero bytes in the first 16 MiB of the target disk
# after a marker (all zeros = untouched).
disk_check() { # $1 = marker
  printf '%s\n' "send head -c 16777216 /dev/vda | tr -d \"\\0\" | wc -c | sed \"s/^/$1=/\""
  echo "expect $1=[0-9]+ 120"
}
# A command whose output is checked on its own: START=42 and EXIT=<code>
# only appear in the output, never in the echo of the typed command line.
marked() { # $1 = marker, rest = command
  local m="$1"; shift
  echo "send echo $m-START=\$((40+2)); $*; echo $m-EXIT=\$?"
  echo "expect $m-EXIT=[0-9]+ 900"
}
if [[ "${WIZARD:-1}" == 1 ]]; then
  # Drive the interactive wizard (`nestegg rescue`) like a person would,
  # including typical mistakes. Before it, the non-interactive restore, with
  # permission to erase: a wrong key and the unencrypted decoy (newest) must
  # be refused with the disk untouched. Wizard pass 1: wrong key refused,
  # quit there, disk checked. Pass 2: the decoy declined, the wrong key
  # replaced, cancelled at the final confirmation (disk checked again).
  # Pass 3: complete restore.
  restore_args="--settings $settings --fingerprint $fp --password-file /media/t/password --disk /dev/vda --wipe /dev/vda $encrypt_arg"
  connection_defaults() { # $1 = answer to the certificate question, $2 = match|differs|trusted
    for q in "PBS address" "Port" "Datastore" "Namespace" "User or API token" "MAC address"; do
      printf 'expect %s 120\nsend\n' "$q"
    done
    case $2 in
      trusted) printf 'expect this is the certificate you trusted before 120\n'; return ;;
      match) printf 'expect It matches the fingerprint saved in %s 120\n' "$settings" ;;
      differs) printf 'expect NOT the certificate fingerprint saved in %s 120\n' "$other_settings" ;;
    esac
    printf 'expect Trust this certificate 30\nsend %s\n' "$1"
  }
  settings_choice() { # $1 = number to send at "Which settings" (empty = Enter)
    printf 'expect Looking for settings and keys on the attached drives 60\n'
    printf 'expect Found 2 settings files 120\nexpect Which settings 30\nsend %s\n' "$1"
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
  wrong_key() { # $1 = mistakes: also try typing errors
    if [[ $key_mode == file ]]; then
      printf 'expect Encryption key 60\nsend type\nexpect Key \\(empty line = back\\) 30\n'
      if [[ ${1:-} == mistakes ]]; then
        printf 'send abc_defghijkl\nexpect cannot be part of a key 30\n'
        printf 'send ABCDEFGH\nexpect got 8 characters 30\n'
      fi
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
  # The list is: 1 the unencrypted decoy (newest), 2 the signed-only
  # backup, 3 the real one. Enter at "Which backup" takes the decoy: it must
  # be refused by default, and the menu then offers another backup.
  decline_decoy() {
    printf 'expect NOT ENCRYPTED 300\nexpect Which backup 30\nsend\n'
    printf 'expect not protected by your key 300\nexpect Restore this backup anyway 30\nsend\n'
    printf 'expect This backup is not restored 30\nexpect Nothing was changed 30\nexpect What now 30\nsend\n'
    printf 'expect Which backup 30\n'
  }
  # With the right key at hand: the signed-only backup is refused by default
  # as well. Then the real one (number 3) is chosen.
  decline_unprotected() {
    decline_decoy
    printf 'send 2\nexpect signed with your key 300\nexpect Restore this backup anyway 30\nsend\n'
    printf 'expect This backup is not restored 30\nexpect Nothing was changed 30\nexpect What now 30\nsend\n'
    printf 'expect Which backup 30\nsend 3\n'
  }
  # The decoy has no disk layout: when the user accepts it anyway, the wizard
  # must say that no key and no second try helps, and the menu must default
  # to another backup (Enter), not to "try again".
  accept_decoy_then_leave() {
    printf 'expect NOT ENCRYPTED 300\nexpect Which backup 30\nsend\n'
    printf 'expect not protected by your key 300\nexpect Restore this backup anyway 30\nsend y\n'
    printf 'expect no disk layout 300\nexpect This backup cannot be restored 30\nexpect Nothing was changed 30\nexpect What now 30\nsend\n'
    printf 'expect Which backup 30\nsend 3\n'
  }
  {
    echo 'expect root@nestegg-rescue[^#]*# 600'
    echo 'send mkdir -p /media/t && mount -o ro /dev/vdb /media/t; echo MOUNT-EXIT=$?'
    echo 'expect MOUNT-EXIT=0 60'
    # Always exercise the supplied binary in both VMs. The ISO supplies the
    # immutable OS/PBS fixture; it may have been built before a code-only fix.
    echo 'send install -m 0755 /media/t/nestegg /usr/local/bin/nestegg; echo BINARY-EXIT=$?'
    echo 'expect BINARY-EXIT=0 60'
    echo '# A wrong key for the non-interactive restore, WITH permission to erase:'
    echo '# it must be refused before the disk is touched.'
    marked WRONGKEY nestegg rescue restore "$restore_args" --snapshot "$real_snap" --keyfile /media/t/x/y/z/wrong-paperkey.txt
    echo '# The right key, but the newest backup is the unencrypted decoy: refused.'
    marked DECOY nestegg rescue restore "$restore_args" --keyfile /media/t/x/y/z/nestegg-key.json
    echo '# The right key, and a backup that is only signed with it: refused as well.'
    marked SIGNED nestegg rescue restore "$restore_args" --snapshot "$signed_snap" --keyfile /media/t/x/y/z/nestegg-key.json
    echo '# Accepted explicitly (dry run, nothing is changed): the signature is checked'
    echo '# with the key when the layout is read.'
    marked SIGNEDOK nestegg rescue restore "$restore_args" --snapshot "$signed_snap" --keyfile /media/t/x/y/z/nestegg-key.json --allow-unprotected-backup --dry-run
    disk_check DISK-DIRTY-BYTES
    echo '# The right key as a plain JSON file, dry run: reads the backup, changes nothing.'
    echo "send nestegg rescue restore $restore_args --snapshot $real_snap --keyfile /media/t/x/y/z/nestegg-key.json --dry-run; echo DRYRUN-EXIT=\$?"
    echo 'expect DRYRUN-EXIT=[0-9]+ 900'
    echo '# Pass 1, with the settings file that has the right fingerprint.'
    echo 'send nestegg rescue; echo WIZARD-EXIT=$?'
    settings_choice ''
    echo '# Round 1: the fingerprint matches the settings file, but Enter must NOT trust it'
    connection_defaults '' match
    echo 'expect Password or API token secret 120'
    printf 'sendfile %s\n' "$work/password"
    echo 'expect Encryption key 60'
    echo 'send -'
    echo 'expect Login or listing failed 300'
    echo 'expect No certificate was trusted 30'
    echo '# Round 2: trusted explicitly. Mistake: just Enter at the password prompt -> asks again'
    connection_defaults y match
    echo 'expect Password or API token secret 120'
    echo 'send'
    echo 'expect Please enter a value 30'
    echo 'expect Password or API token secret 30'
    echo '# Mistake: wrong secret'
    echo 'send wrong-secret-for-the-test'
    echo '# Mistake: key file that does not exist -> must say so and ask again'
    echo 'expect Encryption key 60'
    echo 'send /nonexistent/nestegg-key.json'
    echo 'expect cannot read /nonexistent/nestegg-key.json 30'
    echo 'expect Encryption key 30'
    echo 'send -'
    echo 'expect Login or listing failed 300'
    echo '# Round 3: the wizard starts over; the trusted fingerprint is remembered'
    connection_defaults '' trusted
    echo 'expect Password or API token secret 120'
    printf 'sendfile %s\n' "$work/password"
    echo '# Mistake: the wrong key. It is refused when the backup is chosen.'
    wrong_key mistakes
    echo '# Mistake: number out of range'
    echo 'expect Which computer 300'
    echo 'send 9'
    echo 'expect Please enter a number between 30'
    echo 'expect Which computer 30'
    echo 'send 1'
    echo 'expect Which backup 300'
    echo 'send 3'
    echo 'expect Reading the disk layout 120'
    echo 'expect (wrong key|does not match provided key) 600'
    echo 'expect The encryption key does not fit this backup 30'
    echo 'expect Nothing was changed 30'
    echo 'expect What now 30'
    echo '# Quit right here, and look at the disk.'
    echo 'send 4'
    echo 'expect Stopped, nothing was changed 30'
    echo 'expect WIZARD-EXIT=1 60'
    disk_check DISK-AFTER-WRONGKEY
    echo '# Pass 2, with the settings file whose fingerprint differs. The decoy is accepted'
    echo '# (it has no disk layout), then the wrong key is refused.'
    echo 'send nestegg rescue; echo WIZARD-EXIT=$?'
    settings_choice 2
    connection_defaults y differs
    echo 'expect Password or API token secret 120'
    printf 'sendfile %s\n' "$work/password"
    wrong_key
    echo 'expect Which computer 300'
    echo 'send'
    accept_decoy_then_leave
    echo 'expect (wrong key|does not match provided key) 600'
    echo 'expect What now 30'
    echo '# Pick another key (the default of this menu after a key problem)'
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
    disk_check DISK-AFTER-CANCEL
    echo '# Pass 3: everything right (the decoy and the signed-only backup are declined first)'
    echo 'send nestegg rescue; echo WIZARD-EXIT=$?'
    settings_choice ''
    connection_defaults y match
    echo 'expect Password or API token secret 120'
    printf 'sendfile %s\n' "$work/password"
    right_key
    echo 'expect Which computer 300'
    echo 'send'
    decline_unprotected
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
  restore_args="--settings $settings --password-file /media/t/password --keyfile $key_arg --disk /dev/vda --wipe /dev/vda $encrypt_arg"
  {
    echo 'expect root@nestegg-rescue[^#]*# 600'
    echo 'send mkdir -p /media/t && mount -o ro /dev/vdb /media/t; echo MOUNT-EXIT=$?'
    echo 'expect MOUNT-EXIT=0 60'
    # Always exercise the supplied binary in both VMs. The ISO supplies the
    # immutable OS/PBS fixture; it may have been built before a code-only fix.
    echo 'send install -m 0755 /media/t/nestegg /usr/local/bin/nestegg; echo BINARY-EXIT=$?'
    echo 'expect BINARY-EXIT=0 60'
    echo '# The newest backup is the unencrypted decoy: refused, disk untouched.'
    marked DECOY nestegg rescue restore "$restore_args"
    echo '# The same for the backup that is only signed with the key.'
    marked SIGNED nestegg rescue restore "$restore_args" --snapshot "$signed_snap"
    disk_check DISK-DIRTY-BYTES
    echo "send nestegg rescue restore $restore_args --snapshot $real_snap; echo RESTORE-EXIT=\$?"
    echo 'expect RESTORE-EXIT=[0-9]+ 2400'
    echo 'send poweroff'
  } > "$work/rescue.script"
fi
# shellcheck disable=SC2046
qemu 3600 $(firmware rescue) -cdrom "$iso" -boot d \
  -drive "file=$work/target.img,if=virtio,format=raw" \
  -drive "file=$work/media.img,if=virtio,format=raw" \
  -serial "unix:$work/rescue.sock,server=on,wait=off" &
qemu_pids+=($!)
python3 "$here/console.py" "$work/rescue.sock" "$work/rescue-console.log" "$work/rescue.script" || true
wait "${qemu_pids[-1]}" || true
rlog="$work/rescue-console.log"
# Output of one marked command (see marked()), from its START line to its
# EXIT line, the first time only.
part() { awk -v m="$1" '$0 ~ m"-START=42" {on=1; next} on && $0 ~ m"-EXIT=[0-9]" {exit} on' "$rlog"; }
decoy_part="$(part DECOY)"
check "decoy (non-interactive): refused as not protected by the key" \
  grep -aq 'not protected by your key' <<<"$decoy_part"
check "decoy (non-interactive): nothing read from it" \
  exits_with 1 grep -aq 'Reading the disk layout' <<<"$decoy_part"
check "decoy (non-interactive): exit code is an error" grep -aq 'DECOY-EXIT=[1-9]' "$rlog"
signed_part="$(part SIGNED)"
check "signed-only backup (non-interactive): refused as signed but not encrypted" \
  grep -aq 'signed with your key (checked), but .*not encrypted' <<<"$signed_part"
check "signed-only backup (non-interactive): nothing read from it" \
  exits_with 1 grep -aq 'Reading the disk layout' <<<"$signed_part"
check "signed-only backup (non-interactive): exit code is an error" grep -aq 'SIGNED-EXIT=[1-9]' "$rlog"
check "refused restores left the disk untouched" grep -aq 'DISK-DIRTY-BYTES=0' "$rlog"
if [[ "${WIZARD:-1}" == 1 ]]; then
  wrong_part="$(part WRONGKEY)"
  check "wrong key (non-interactive): refused because of the key" \
    grep -aEq 'wrong key|does not match provided key' <<<"$wrong_part"
  check "wrong key (non-interactive): refused before the disk layout was read" \
    exits_with 1 grep -aq 'Reading the disk layout' <<<"$wrong_part"
  check "wrong key (non-interactive): exit code is an error" grep -aq 'WRONGKEY-EXIT=[1-9]' "$rlog"
  check "right key as JSON file: dry run works" grep -aq 'DRYRUN-EXIT=0' "$rlog"
  signedok_part="$(part SIGNEDOK)"
  check "signed-only backup, explicitly accepted: warns, reads it with the key, changes nothing" \
    grep -aq 'Restoring it anyway' <<<"$signedok_part"
  check "signed-only backup, explicitly accepted: dry run changes nothing" \
    grep -aq 'Dry run: nothing was changed' <<<"$signedok_part"
  check "signed-only backup, explicitly accepted: exit code is success" grep -aq 'SIGNEDOK-EXIT=0' "$rlog"
  check "wizard: progress line before the media search" grep -aq 'Looking for settings and keys on the attached drives' "$rlog"
  check "wizard: both settings files named" grep -aq 'Found 2 settings files' "$rlog"
  check "wizard: fingerprint from the settings file shown with its source" \
    grep -aq "It matches the fingerprint saved in $settings" "$rlog"
  check "wizard: Enter does not trust the certificate (despite the settings file)" grep -aq 'No certificate was trusted' "$rlog"
  check "wizard: trusted fingerprint is remembered" grep -aq 'this is the certificate you trusted before' "$rlog"
  check "wizard: a different saved fingerprint is pointed out" \
    grep -aq "NOT the certificate fingerprint saved in $other_settings" "$rlog"
  check "wizard: empty password input asks again" grep -aq 'Please enter a value' "$rlog"
  check "wizard: missing key file is reported and asked again" grep -aq 'cannot read /nonexistent/nestegg-key.json' "$rlog"
  check "wizard: wrong secret starts over" grep -aq 'Login or listing failed' "$rlog"
  check "wizard: invalid number asks again" grep -aq 'Please enter a number between' "$rlog"
  check "wizard: backup list shows the unencrypted decoy" grep -aq 'NOT ENCRYPTED' "$rlog"
  check "wizard: backup list shows the encrypted backup" grep -aEq '^ +[0-9]+\) .*  encrypted' "$rlog"
  check "wizard: backup list shows the signed-only backup" grep -aEq '^ +[0-9]+\) .*  signed, not encrypted' "$rlog"
  check "wizard: unprotected backup declined by default" grep -aq 'This backup is not restored' "$rlog"
  check "wizard: signed-only backup named as signed with the key, declined by default" \
    grep -aq 'signed with your key (checked)' "$rlog"
  check "wizard: a backup that cannot be restored is said to be unusable" \
    grep -aq 'This backup cannot be restored, with any key' "$rlog"
  check "wizard: that backup has no disk layout" grep -aq 'no disk layout' "$rlog"
  check "wizard: wrong key refused with a message" grep -aq 'The encryption key does not fit this backup' "$rlog"
  check "wizard: right key accepted" grep -aq 'The key fits this backup' "$rlog"
  # The wrong key is refused before the disk list (Step 3/4) is shown, and
  # the disk is still all zeros when the wizard is left at that point.
  check "wizard: wrong key refused before the disk step" \
    awk '/does not fit this backup/ {refused=1} /Step 3\/4/ && !refused {early=1} END {exit !(refused && !early)}' "$rlog"
  check "wizard: quit at the key menu" grep -aq 'Stopped, nothing was changed' "$rlog"
  check "wizard: disk untouched after the wrong key" grep -aq 'DISK-AFTER-WRONGKEY=0' "$rlog"
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
  check "wizard: disk untouched after the cancelled pass" grep -aq 'DISK-AFTER-CANCEL=0' "$rlog"
  check "wizard: both stopped passes ended with exit 1" [ "$(grep -ac 'WIZARD-EXIT=1' "$rlog")" -ge 2 ]
  check "wizard: last pass succeeded" grep -aq 'WIZARD-EXIT=0' "$rlog"
  check "wizard: token secret never shown" bash -c "! grep -aqF \"\$(cat '$work/password')\" '$rlog'"
else
  check "rescue restore succeeded" grep -aq 'RESTORE-EXIT=0' "$rlog"
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
