#!/usr/bin/env bash
# Phases 3/4 acceptance. ONLY disposable files, loops, containers and KVM VMs.
# Usage: sudo tests/stick/rescue.sh BINARY RESCUE_ISO [desktop|plain|luks-lvm|dirty|no-nestegg|btrfs|pending-merge|id-collision|all]
set -Eeuo pipefail
[[ $EUID == 0 && -e /dev/kvm ]] || { echo 'root and KVM required' >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
bin=$(realpath "${1:?binary}"); iso=$(realpath "${2:?ISO}"); modes=${3:-all}
if [[ ${NESTEGG_RESCUE_TEST_NS:-0} != 1 ]]; then
    exec unshare --mount --propagation private env NESTEGG_RESCUE_TEST_NS=1 "$0" "$@"
fi
work=$(mktemp -d /tmp/nestegg-stick-rescue.XXXXXX)
qemu_pid=; loop=; container=nestegg-stick-source-$$
cleanup() {
    local rc=$? bad=0
    trap - EXIT ERR; trap '' INT TERM
    set +e
    [[ -z $qemu_pid ]] || { kill "$qemu_pid" 2>/dev/null; wait "$qemu_pid" 2>/dev/null; }
    docker rm -f "$container" >/dev/null 2>&1
    for point in payload data iso; do
        mountpoint -q "$work/$point" && umount "$work/$point"
    done
    [[ -z $loop ]] || losetup -d "$loop" || bad=1
    if losetup -ln -O BACK-FILE | grep -Fq "$work/"; then bad=1; fi
    if [[ -n ${KEEP_LOGS:-} ]]; then
        mkdir -p "$KEEP_LOGS"
        cp "$work/"*.log "$KEEP_LOGS/" 2>/dev/null
    fi
    ((bad == 0)) || { echo "ERROR: retained owned resources: $work" >&2; exit 1; }
    if [[ ${KEEP:-0} == 1 ]]; then echo "work directory kept: $work"; else rm -rf "$work"; fi
    exit "$rc"
}
trap cleanup EXIT
trap 'echo "ERROR at line $LINENO" >&2; tail -40 "$work/"*console.log 2>/dev/null || true' ERR
trap 'exit 130' INT
trap 'exit 143' TERM
unset SUDO_UID SUDO_GID SUDO_USER CREDENTIALS_DIRECTORY
mkdir -p "$work/"{state,run,iso,payload,data}
cp "$bin" "$work/nestegg"; bin=$work/nestegg
cp --reflink=auto "$iso" "$work/release.iso"
cp "$iso.sha256" "$work/release.iso.sha256"
iso=$work/release.iso
mount -t tmpfs -o mode=0755 tmpfs /var/lib
mkdir /var/lib/nestegg
mount --bind "$work/state" /var/lib/nestegg
export NESTEGG_STATE_DIR="$work/state" NESTEGG_RUNTIME_DIR="$work/run" NESTEGG_CONFIG="$work/config.toml"
truncate -s 12G "$work/stick.img"
python3 "$here/confirm.py" "$bin" stick create "$work/stick.img" --test-image --image "$iso" > "$work/create.log" 2>&1
uuid=$(python3 -c 'import re,sys; print(re.search(r"\"data_uuid\":\s*\"([0-9a-fA-F-]{36})\"", open(sys.argv[1]).read()).group(1))' "$work/create.log")
mount -o loop,ro "$iso" "$work/iso"
cp "$work/iso/live/filesystem.squashfs" "$work/pbs.squashfs"
umount "$work/iso"
# Generated credentials never leave the disposable fixture directory.
{ openssl rand -base64 48; } | tr -d '\n' > "$work/key-password"
printf '!7aA\n' >> "$work/key-password"
openssl rand -base64 24 | tr -d '\n' > "$work/luks-password"
chmod 600 "$work/"*password
printf -v fingerprint '%s' "$(printf '00:%.0s' {1..31})00"
echo '== build powered-off source system'
docker run --rm --name "$container" --privileged -v /dev:/dev -v "$work:/work" -v "$repo/tests/rescue:/t:ro" \
    -e EXTRA_SOURCE_PACKAGES=iproute2 -e VARIANT=ubuntu-ext4-bios -e IMAGE=/work/source.img -e PBS_IP=192.0.2.10 \
    -e FP="$fingerprint" -e TOKEN_SECRET=fixture-only-not-used ubuntu:24.04 bash /t/make-source.sh > "$work/build-source.log" 2>&1
cp --reflink=auto --sparse=always "$work/source.img" "$work/template.img"
cp "$work/data.sha256" "$work/template-data.sha256"
# Payload is read-only in all guests. It supplies the exact tested binary and
# disposable passwords/paper key, never source disk code to the product.
truncate -s 128M "$work/payload.img"
mkfs.ext4 -q -F "$work/payload.img"
mount -o loop "$work/payload.img" "$work/payload"
cp "$bin" "$work/key-password" "$work/luks-password" "$work/nestegg-paperkey.txt" "$work/nestegg-key.json" "$here/desktop-restore.py" "$work/payload/"
umount "$work/payload"
firmware=(-drive "if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd")
boot_stick() {
    local label=$1 disk=$2 steps=$3
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars.fd"
    qemu-system-x86_64 -enable-kvm -cpu host -m 4096 -smp 2 -display none -monitor none -nic none \
        "${firmware[@]}" -drive "if=pflash,format=raw,file=$work/vars.fd" \
        -drive "file=$work/stick.img,format=raw,if=none,id=stick" -device virtio-blk-pci,drive=stick,bootindex=1 \
        -drive "file=$disk,format=raw,if=virtio" -drive "file=$work/payload.img,format=raw,if=virtio,readonly=on" \
        -serial "unix:$work/console.sock,server=on,wait=on" > "$work/$label-qemu.log" 2>&1 &
    qemu_pid=$!
    timeout 2400 python3 "$repo/tests/rescue/console.py" "$work/console.sock" "$work/$label-console.log" "$steps"
    wait "$qemu_pid"; qemu_pid=
    rm -f "$work/console.sock"
}
common_steps() {
    cat <<'EOF'
expect root@.*# 240
send mkdir -p /media/test; mount -o ro,noload /dev/vdc /media/test; cp /media/test/nestegg /usr/local/bin/nestegg; chmod 755 /usr/local/bin/nestegg
expect root@.*# 30
EOF
}
if [[ $modes == all ]]; then modes='desktop plain id-collision luks-lvm dirty no-nestegg btrfs pending-merge'; fi
for mode in $modes; do
    echo "== $mode"
    if [[ $mode == btrfs ]]; then
        rm -f "$work/source.img"
        docker run --rm --name "$container" --privileged -v /dev:/dev -v "$work:/work" -v "$repo/tests/rescue:/t:ro" \
            -e VARIANT=debian-btrfs-uefi -e IMAGE=/work/source.img -e PBS_IP=192.0.2.10 \
            -e FP="$fingerprint" -e TOKEN_SECRET=fixture-only-not-used debian:trixie bash /t/make-source.sh > "$work/build-btrfs.log" 2>&1
    else
        cp --reflink=auto --sparse=always "$work/template.img" "$work/source.img"
        cp "$work/template-data.sha256" "$work/data.sha256"
    fi
    docker run --rm --name "$container" --privileged -v /dev:/dev -v "$work:/work" -v "$here:/t:ro" \
        -e MODE="$mode" -e DATA_UUID="$uuid" ubuntu:24.04 bash /t/prepare-offline-source.sh > "$work/prepare-$mode.log" 2>&1
    if [[ $mode == pending-merge ]]; then
        mv "$work/pending.img" "$work/source.img"
        sha256sum "$work/source.img" > "$work/pending-before.sha256"
        {
            common_steps
            cat <<'EOF'
send nestegg rescue backup --disk /dev/vdb --backup-id pending-fixture --keyfile /media/test/nestegg-paperkey.txt --confirm-clock >/run/pending-refusal.log 2>&1; code=$?; cat /run/pending-refusal.log; test "$code" != 0 && grep -q 'snapshots, merges and hidden LVs are refused' /run/pending-refusal.log && ! dmsetup table | grep -q snapshot && test "$(blockdev --getro /dev/vdb)" = 0; printf 'REFUSAL-%s=%s\n' EXIT "$?"
expect REFUSAL-EXIT=0 240
send poweroff -f
EOF
        } > "$work/pending.steps"
        boot_stick pending-merge "$work/source.img" "$work/pending.steps"
        sha256sum --check "$work/pending-before.sha256"
        echo 'PASS pending merge: activation refused, no mappings, source flags restored, entire image SHA-256 unchanged'
        # Reproduce the former activation on a COPY, never the checked source.
        cp --reflink=auto --sparse=always "$work/source.img" "$work/unsafe.img"
        sha256sum "$work/unsafe.img" > "$work/unsafe-before.sha256"
        {
            common_steps
            cat <<'EOF'
send lvs --reportformat json --devices /dev/vdb -o vg_name,lv_name,segtype,lv_attr; blockdev --setro /dev/vdb; vgchange -ay --monitor n --devices /dev/vdb --config 'global { metadata_read_only=1 } activation { read_only_volume_list=["nestegg-pending-fixture"] monitoring=0 } backup { backup=0 archive=0 }' nestegg-pending-fixture; dmsetup table; sleep 5; vgchange -an --devices /dev/vdb --config 'global { metadata_read_only=1 } backup { backup=0 archive=0 }' nestegg-pending-fixture; blockdev --setrw /dev/vdb; echo UNSAFE-REPRO-DONE
expect UNSAFE-REPRO-DONE 120
send poweroff -f
EOF
        } > "$work/unsafe.steps"
        boot_stick unsafe-merge "$work/unsafe.img" "$work/unsafe.steps"
        if sha256sum --check "$work/unsafe-before.sha256" > "$work/unsafe-check.log" 2>&1; then
            echo 'OBSERVATION original activation: source checksum unchanged (inspect kernel/activation log)'
        else
            echo 'OBSERVATION original activation: SOURCE CHECKSUM CHANGED despite read-only flags'
        fi
        continue
    fi
    id=offline-$mode
    [[ $mode != id-collision ]] || id=offline-plain
    if [[ $mode == desktop ]]; then
        id=e2e-src
        timeout 1800 qemu-system-x86_64 -enable-kvm -cpu host -m 4096 -smp 2 -display none -monitor none \
            -nic user,model=virtio-net-pci -drive "file=$work/source.img,format=raw,if=virtio" \
            -drive "file=$work/stick.img,format=raw,if=virtio" -serial "file:$work/desktop-console.log"
        grep -q E2E-BACKUP-OK "$work/desktop-console.log"
        echo 'PASS desktop VM backed up to the attached stick'
    else
        if [[ $mode == luks-lvm ]]; then mv "$work/encrypted.img" "$work/source.img"; fi
        if [[ $mode == dirty ]]; then
            : > "$work/dirty-console.log"
            qemu-system-x86_64 -enable-kvm -cpu host -m 2048 -smp 2 -display none -monitor none \
                -nic user,model=virtio-net-pci -drive "file=$work/source.img,format=raw,if=virtio" -serial "file:$work/dirty-console.log" &
            qemu_pid=$!
            for _ in {1..1200}; do
                grep -q E2E-DIRTY-WRITING "$work/dirty-console.log" && break
                kill -0 "$qemu_pid"; sleep .2
            done
            grep -q E2E-DIRTY-WRITING "$work/dirty-console.log"
            kill -KILL "$qemu_pid"; wait "$qemu_pid" || true; qemu_pid=
        fi
        sha256sum "$work/source.img" > "$work/before.sha256"
        {
            common_steps
            if [[ $mode == no-nestegg ]]; then
                id=nestegg-e2e-src
                cat <<'EOF'
send nestegg rescue; printf 'BACKUP-%s=%s\n' EXIT "$?"
expect What would you like to do 60
send 2
expect Computer disk to back up 240
send 1
expect Confirm computer backup ID 120
send
expect Encryption key 30
send 2
expect Stick key password 60
EOF
                echo "sendfile $work/key-password"
                echo 'expect Repeat password 30'
                echo "sendfile $work/key-password"
                cat <<'EOF'
expect Show the paper key now 60
send n
expect Is the current UTC date and time correct 60
send y
expect Start the encrypted backup 30
send y
expect BACKUP-EXIT=0 1800
EOF
            else
                extra=
                [[ $mode != luks-lvm ]] || extra='--luks-passphrase-file /media/test/luks-password'
                if [[ $mode == id-collision ]]; then
                    cat <<'EOF'
send nestegg rescue backup --disk /dev/vdb --backup-id offline-plain --keyfile /media/test/nestegg-paperkey.txt --confirm-clock >/run/collision.log 2>&1; code=$?; cat /run/collision.log; test "$code" != 0 && grep -q 'backup ID collision:.*different machine identity' /run/collision.log; printf 'COLLISION-%s=%s\n' EXIT "$?"
expect COLLISION-EXIT=0 300
EOF
                    extra=--force-id
                fi
                printf '%s\n' "send nestegg rescue backup --disk /dev/vdb --backup-id $id --keyfile /media/test/nestegg-paperkey.txt --confirm-clock $extra; printf 'BACKUP-%s=%s\\n' EXIT \"\$?\""
                echo 'expect BACKUP-EXIT=0 1800'
            fi
            echo 'send poweroff -f'
        } > "$work/backup.steps"
        boot_stick "backup-$mode" "$work/source.img" "$work/backup.steps"
        sha256sum --check "$work/before.sha256"
        grep -q 'Offline backup verified:' "$work/backup-$mode-console.log"
        if [[ $mode == dirty ]]; then
            python3 - "$work/backup-$mode-console.log" <<'PYTEST'
import re,sys
log=open(sys.argv[1]).read()
assert 'Journal recovery in RAM:' in log
assert any(int(n) > 0 for n in re.findall(r'RAM journal snapshot:.*snapshot (\d+)/', log)), 'journal recovery did not write into RAM'
PYTEST
        fi
        echo "PASS $mode: encrypted offline backup verified and entire source disk SHA-256 unchanged"
    fi
    rm -f "$work/target.img"
    truncate -s 12G "$work/target.img"
    {
        common_steps
        if [[ $mode == desktop ]]; then
            cat <<EOF
send mkdir -p /var/lib/nestegg; mount -t tmpfs -o mode=0755 tmpfs /var/lib/nestegg; mkdir /var/lib/nestegg/stick; cp /run/live/medium/live/a/filesystem.squashfs /var/lib/nestegg/stick/pbs.squashfs; sha256sum /var/lib/nestegg/stick/pbs.squashfs | cut -d' ' -f1 > /var/lib/nestegg/stick/pbs.squashfs.sha256; mkdir -m700 /run/api-credentials; cp /media/test/nestegg-key.json /run/api-credentials/encryption-key; chmod 600 /run/api-credentials/encryption-key; printf 'backup_id = "e2e-src"\\n[stick]\\ndata_uuids = ["$uuid"]\\n' > /run/api-config.toml; groupadd -f nestegg; useradd -m -G nestegg alice; NESTEGG_CONFIG=/run/api-config.toml NESTEGG_RUNTIME_DIR=/run/desktop-api NESTEGG_STATE_DIR=/var/lib/nestegg NESTEGG_CREDENTIALS_DIR=/run/api-credentials nestegg daemon >/run/api-daemon.log 2>&1 & api_pid=\$!; for i in {1..100}; do test -S /run/desktop-api/api.sock && break; sleep .1; done; runuser -u alice -- python3 /media/test/desktop-restore.py; api_code=\$?; kill \$api_pid; wait \$api_pid; printf 'API-%s=%s\\n' EXIT \$api_code
expect API-EXIT=0 900
EOF
            cat <<'EOF'
send nestegg rescue; printf 'RESTORE-%s=%s\n' EXIT "$?"
expect What would you like to do 60
send 1
expect Backup source 30
send 1
expect Which computer 240
send 1
expect Encryption key 30
send 1
expect Stick key password 30
EOF
            echo "sendfile $work/key-password"
            cat <<'EOF'
expect Which backup 60
send 1
expect Which disk 120
send 1
expect Encrypt the restored disk 60
send n
expect Type 'vdb' to erase 60
send vdb
expect Remove the rescue stick and restart now 1800
send n
expect RESTORE-EXIT=0 60
EOF
        else
            key='--keyfile /media/test/nestegg-paperkey.txt'
            [[ $mode != no-nestegg ]] || key='--key-password-file /media/test/key-password'
            printf '%s\n' "send nestegg rescue restore --source stick --backup-id $id $key --disk /dev/vdb --wipe /dev/vdb; printf 'RESTORE-%s=%s\\n' EXIT \"\$?\""
            echo 'expect RESTORE-EXIT=0 1800'
        fi
        echo 'send poweroff -f'
    } > "$work/restore.steps"
    boot_stick "restore-$mode" "$work/target.img" "$work/restore.steps"
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$work/vars.fd"
    timeout 600 qemu-system-x86_64 -enable-kvm -cpu host -m 2048 -smp 2 -display none -monitor none \
        -nic user,model=virtio-net-pci "${firmware[@]}" -drive "if=pflash,format=raw,file=$work/vars.fd" \
        -drive "file=$work/target.img,format=raw,if=virtio" -serial "file:$work/boot-$mode-console.log"
    grep -q E2E-RESTORED-BOOT "$work/boot-$mode-console.log"
    grep -q "E2E-SHA $(cat "$work/data.sha256")" "$work/boot-$mode-console.log"
    grep -q 'E2E-OWNER alice' "$work/boot-$mode-console.log"
    grep -q E2E-DONE "$work/boot-$mode-console.log"
    if [[ $mode == btrfs ]]; then
        grep -q 'E2E-MYSQL offline database witness' "$work/boot-$mode-console.log"
        grep -q 'E2E-SNAPSHOTS btrfs' "$work/boot-$mode-console.log"
    fi
    echo "PASS $mode: restored onto an empty disk and booted with original file checksum and owner"
done
echo 'PASS backup-stick rescue acceptance (no real USB hardware)'
