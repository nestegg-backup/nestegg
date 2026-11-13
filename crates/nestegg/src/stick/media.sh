#!/usr/bin/env bash
# Embedded helper; invoked in a private mount namespace, never from the stick.
set -euo pipefail
umask 022
action=$1; disk=$2; image=$3; cache=$4; work=$5; slot_mib=$6
cache_tmp=
mount --make-rprivate /
exec 3>&1
exec 1>&2
mkdir "$work/iso" "$work/root" "$work/esp" "$work/data" "$work/slot_a" "$work/slot_b"
cleanup() {
    local status=$? failed=0 point
    trap - EXIT; trap '' INT TERM
    [[ -z $cache_tmp ]] || rm -f -- "$cache_tmp"
    for point in data slot_b slot_a esp root iso; do
        if mountpoint -q "$work/$point"; then umount "$work/$point" || failed=1; fi
    done
    if ((failed)); then echo 'ERROR: media helper could not unmount its private mounts' >&2; exit 1; fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
part() { if [[ $disk =~ [0-9]$ ]]; then printf '%sp%s' "$disk" "$1"; else printf '%s%s' "$disk" "$1"; fi; }
boot=$(part 2); slot_a=$(part 3); slot_b=$(part 4); data=$(part 5)
fail() { echo "ERROR: $*" >&2; exit 1; }
# GNU sync with operands uses fsync (including device cache flushes), not syncfs.
flush_tree() {
    while IFS= read -r -d '' entry; do sync "$entry"; done < <(find "$1" -depth \( -type f -o -type d \) -print0)
}
render_cfg() {
    local slot=$1 generation=$2 kernel=$3 initrd=$4
    local uuid_name=slot_${slot}_uuid
    local slot_uuid=${!uuid_name}
    cat <<CFG
# nestegg-slot=$slot
# nestegg-generation=$generation
# nestegg-kernel=$kernel
# nestegg-initrd=$initrd
function nestegg_boot_$slot {
  if [ -z "\$nestegg_selected" ]; then
    set default=nestegg-$slot
    set nestegg_selected=1
  fi
  menuentry 'nestegg rescue (slot $slot)' --id=nestegg-$slot {
    search --no-floppy --fs-uuid --set=root $slot_uuid
    linux /live/$slot/$kernel boot=live components live-media=/dev/disk/by-uuid/$slot_uuid live-media-path=/live/$slot hostname=nestegg-rescue locales=en_US.UTF-8 console=tty0 console=ttyS0,115200n8
    initrd /live/$slot/$initrd
  }
}
if hashsum --hash sha256 --check /live/$slot/payload.sha256; then
  set nestegg_${slot}_generation=$generation
fi
CFG
}
slot_device() { if [[ $1 == a ]]; then printf '%s' "$slot_a"; else printf '%s' "$slot_b"; fi; }
slot_marker() { printf 'nestegg-slot-v2\nslot_uuid=%s\ndata_uuid=%s\n' "$1" "$data_uuid"; }
stick_marker() {
    printf 'nestegg-stick-v2\nboot_uuid=%s\nslot_a_uuid=%s\nslot_b_uuid=%s\ndata_uuid=%s\n' "$boot_uuid" "$slot_a_uuid" "$slot_b_uuid" "$data_uuid"
}
small_file() { [[ -f $1 && -s $1 && $(stat -c %s "$1") -le $2 ]] || fail 'missing or oversized stick metadata'; }
if [[ $action == create ]]; then
    for tool in sgdisk blockdev mkfs.vfat mkfs.ext4 grub-install; do
        command -v "$tool" >/dev/null || fail "required host tool is missing: $tool"
    done
fi
# Explicit provenance: install the desktop root only from the verified ISO.
if [[ $action == create || $action == update || $action == add ]]; then
    mount -o loop,ro,nodev,nosuid,noexec "$image" "$work/iso"
    live=$work/iso/live
    [[ -f $live/filesystem.squashfs ]] || fail 'ISO has no live filesystem'
    while IFS= read -r -d '' file; do
        (( $(stat -c %s "$file") < 4294967296 )) || fail 'ISO contains a file too large for the FAT32 boot partition'
    done < <(find "$live" -type f -print0)
    mount -t squashfs -o loop,ro,nodev,nosuid,noexec "$live/filesystem.squashfs" "$work/root"
    [[ -f $work/root/usr/lib/x86_64-linux-gnu/proxmox-backup/proxmox-backup-proxy ]] || fail 'ISO does not contain PBS; use a backup-stick capable release'
    [[ -d $work/root/usr/lib/grub/i386-pc && -d $work/root/usr/lib/grub/x86_64-efi ]] || fail 'ISO lacks BIOS or UEFI GRUB modules'
    image_hash=$(sha256sum "$live/filesystem.squashfs" | cut -d' ' -f1)
    mkdir -p "$cache"
    # The Rust caller has checked ownership of every cache path component.
    cache_tmp=$(mktemp "$cache/.pbs.XXXXXX")
    tmp=$cache_tmp
    cp "$live/filesystem.squashfs" "$tmp"
    chmod 0600 "$tmp"
    sync "$tmp"
    [[ $(sha256sum "$tmp" | cut -d' ' -f1) == "$image_hash" ]] || fail 'desktop image copy checksum mismatch'
    mv -fT "$tmp" "$cache/pbs.squashfs"
    cache_tmp=
    printf '%s\n' "$image_hash" > "$cache/pbs.squashfs.sha256.new"
    sync "$cache/pbs.squashfs.sha256.new"
    mv -fT "$cache/pbs.squashfs.sha256.new" "$cache/pbs.squashfs.sha256"
    sync "$cache"
fi
if [[ $action == create ]]; then
    # This private namespace inherited a snapshot of host mounts. It cannot
    # observe later host automounts; the Rust caller also checks before unshare.
    # The advisory device lock coordinates cooperating udev probes only.
    [[ -z $(lsblk -nr -o MOUNTPOINTS "$disk" | tr -d '[:space:]') ]] || fail 'target became mounted or used as swap'
    while read -r kind; do [[ $kind == disk || $kind == part || $kind == loop ]] || fail 'target became active in LVM, encryption or RAID'; done < <(lsblk -nr -o TYPE "$disk")
    [[ $(blockdev --getro "$disk") == 0 ]] || fail 'target became read-only'
    sgdisk --zap-all "$disk" >/dev/null
    sgdisk --clear --new=1:2048:+1M --typecode=1:ef02 --change-name=1:BIOS \
      --new=2:0:+64M --typecode=2:ef00 --change-name=2:ESP \
      --new=3:0:+"${slot_mib}M" --typecode=3:0700 --change-name=3:NESTEGG_A \
      --new=4:0:+"${slot_mib}M" --typecode=4:0700 --change-name=4:NESTEGG_B \
      --new=5:0:0 --typecode=5:8300 --change-name=5:NESTEGG_DATA "$disk" >/dev/null
    blockdev --rereadpt "$disk"
    # Device nodes come from devtmpfs; waiting for udev here would deadlock
    # against the writer's advisory lock that deliberately delays its probes.
    for ((i=0;i<100;i++)); do [[ -b $boot && -b $slot_a && -b $slot_b && -b $data ]] && break; sleep .1; done
    [[ -b $boot && -b $slot_a && -b $slot_b && -b $data ]] || fail 'partition nodes did not appear'
    mkfs.vfat -F32 -n NESTEGGBOOT "$boot" >/dev/null
    mkfs.vfat -F32 -n NESTEGG_A "$slot_a" >/dev/null
    mkfs.vfat -F32 -n NESTEGG_B "$slot_b" >/dev/null
    mkfs.ext4 -q -F -m 0 -L NESTEGG_DATA "$data"
    mount -o nodev,nosuid,noexec "$data" "$work/data"
    mkdir "$work/data/nestegg"
    sync "$work/data/nestegg" "$work/data"
    umount "$work/data"
    boot_uuid=$(blkid -p -s UUID -o value "$boot")
    data_uuid=$(blkid -p -s UUID -o value "$data")
    mount -o nodev,nosuid,noexec "$boot" "$work/esp"
    slot_a_uuid=$(blkid -p -s UUID -o value "$slot_a")
    slot_b_uuid=$(blkid -p -s UUID -o value "$slot_b")
    stick_marker > "$work/esp/nestegg-stick"
    for candidate in a b; do
        mount -o nodev,nosuid,noexec "$(slot_device "$candidate")" "$work/slot_$candidate"
        uuid=$(blkid -p -s UUID -o value "$(slot_device "$candidate")")
        slot_marker "$uuid" > "$work/slot_$candidate/nestegg-stick"
        sync "$work/slot_$candidate/nestegg-stick" "$work/slot_$candidate"
    done
    slot=a
    generation=1
    grub-install --target=i386-pc --directory="$work/root/usr/lib/grub/i386-pc" --boot-directory="$work/esp/boot" --recheck "$disk"
    grub-install --target=x86_64-efi --directory="$work/root/usr/lib/grub/x86_64-efi" --efi-directory="$work/esp" --boot-directory="$work/esp/boot" --removable --no-nvram
else
    [[ $(blkid -p -s TYPE -o value "$boot") == vfat && $(blkid -p -s PTTYPE -o value "$disk") == gpt ]] || fail 'not a nestegg GPT stick'
    mount -o ro,nodev,nosuid,noexec "$boot" "$work/esp"
    small_file "$work/esp/nestegg-stick" 512
    if [[ $(head -n1 "$work/esp/nestegg-stick") == nestegg-stick-v1 ]]; then
        fail 'old single-FAT stick layout: create the stick again; this erases all backups on it (save them elsewhere first)'
    fi
    [[ $(head -n1 "$work/esp/nestegg-stick") == nestegg-stick-v2 ]] || fail 'unknown stick format'
    for name in boot slot_a slot_b data; do
        device=${!name}
        uuid=$(blkid -p -s UUID -o value "$device")
        [[ $uuid =~ ^[0-9A-Fa-f-]+$ ]] || fail 'invalid partition UUID'
        printf -v "${name}_uuid" '%s' "$uuid"
    done
    [[ $boot_uuid != "$slot_a_uuid" && $boot_uuid != "$slot_b_uuid" && $slot_a_uuid != "$slot_b_uuid" ]] || fail 'duplicate live/boot UUIDs'
    [[ $(blkid -p -s TYPE -o value "$data") == ext4 && $(blkid -p -s LABEL -o value "$data") == NESTEGG_DATA ]] || fail 'not a nestegg data partition'
    cmp -s "$work/esp/nestegg-stick" <(stick_marker) || fail 'invalid stick marker or partition UUIDs'
    for candidate in a b; do
        device=$(slot_device "$candidate")
        [[ $(blkid -p -s TYPE -o value "$device") == vfat ]] || fail 'not a nestegg live partition'
        mount -o ro,nodev,nosuid,noexec "$device" "$work/slot_$candidate"
        uuid=$(blkid -p -s UUID -o value "$device")
        cmp -s "$work/slot_$candidate/nestegg-stick" <(slot_marker "$uuid") || fail 'invalid slot marker'
    done
    # Inspect committed descriptors as data, never source medium code as root.
    slot=; generation=0
    for candidate in a b; do
        cfg=$work/slot_$candidate/live/$candidate/boot.cfg
        commit=$work/slot_$candidate/live/$candidate/commit.sha256
        [[ -f $cfg && -f $commit ]] || continue
        [[ -s $cfg && $(stat -c %s "$cfg") -le 4096 && -s $commit && $(stat -c %s "$commit") -le 256 ]] || continue
        expected=$(cat "$commit")
        actual="$(sha256sum "$cfg" | cut -d' ' -f1)  /live/$candidate/boot.cfg"
        [[ $expected == "$actual" ]] || continue
        candidate_generation=$(sed -n 's/^# nestegg-generation=\([1-9][0-9]*\)$/\1/p' "$cfg")
        [[ $candidate_generation =~ ^[1-9][0-9]{0,8}$ ]] || continue
        candidate_kernel=$(sed -n 's/^# nestegg-kernel=//p' "$cfg")
        candidate_initrd=$(sed -n 's/^# nestegg-initrd=//p' "$cfg")
        [[ $candidate_kernel =~ ^vmlinuz[a-zA-Z0-9._+-]*$ && $candidate_initrd =~ ^initrd[a-zA-Z0-9._+-]*$ ]] || continue
        if ! cmp -s "$cfg" <(render_cfg "$candidate" "$candidate_generation" "$candidate_kernel" "$candidate_initrd"); then continue; fi
        payload=$work/slot_$candidate/live/$candidate
        [[ -f $payload/payload.sha256 && $(stat -c %s "$payload/payload.sha256") -le 1024 ]] || continue
        # Validate exact names before passing this untrusted list to sha256sum.
        manifest=$(cd "$payload" && sha256sum filesystem.squashfs "$candidate_kernel" "$candidate_initrd" | sed "s#  #  /live/$candidate/#") || continue
        [[ $(cat "$payload/payload.sha256") == "$manifest" ]] || continue
        if ((candidate_generation > generation)); then slot=$candidate; generation=$candidate_generation; fi
    done
    [[ $slot == a || $slot == b ]] || fail 'no committed bootable live slot'
    if [[ $action == update ]]; then
        ((generation < 999999999)) || fail 'slot generation exhausted'
        generation=$((generation + 1))
        if [[ $slot == a ]]; then slot=b; else slot=a; fi
        target=$work/slot_$slot
        # Count available bytes plus the replaceable inactive directory. Include
        # allocation rounding for every file and metadata/GRUB overhead.
        cluster=$(stat -f -c %S "$target")
        needed=16777216
        while IFS= read -r -d '' file; do
            size=$(stat -c %s "$file")
            needed=$((needed + (size / cluster + 1) * cluster))
        done < <(find -L "$live" -type f -print0)
        available=$(( $(stat -f -c %a "$target") * cluster ))
        reclaim=0
        if [[ -d $target/live/$slot ]]; then reclaim=$(du -s -B1 "$target/live/$slot" | cut -f1); fi
        ((needed <= available + reclaim)) || fail 'not enough free space for replacement live slot'
        mount -o remount,rw,nodev,nosuid,noexec "$target"
    elif [[ $action != info && $action != add ]]; then fail 'unknown operation'; fi
fi
[[ $boot_uuid =~ ^[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}$ && $data_uuid =~ ^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$ ]] || fail 'invalid UUID'
if [[ $action == create || $action == update ]]; then
    target=$work/slot_$slot
    dest=$target/live/$slot
    # Invalidate a previous inactive commit durably before replacing its bytes.
    if [[ -d $dest ]]; then
        rm -f -- "$dest/commit.sha256"
        sync "$dest"
    fi
    # Work only below the inactive slot. Never interpret medium text as code.
    rm -rf -- "$dest"
    mkdir -p "$dest"
    echo "Writing inactive live slot $slot" >&2
    cp -rL "$live/." "$dest/"
    kernel=$(find "$dest" -maxdepth 1 -type f -name 'vmlinuz*' -printf '%f\n' | sort | head -1)
    initrd=$(find "$dest" -maxdepth 1 -type f -name 'initrd*' -printf '%f\n' | sort | head -1)
    [[ $kernel =~ ^[a-zA-Z0-9._+-]+$ && $initrd =~ ^[a-zA-Z0-9._+-]+$ ]] || fail 'invalid live kernel/initrd names'
    printf '%s\n' "$image_hash" > "$dest/.nestegg-sha256"
    # Verify what was written, then flush the WHOLE slot before publishing it.
    [[ $(sha256sum "$dest/filesystem.squashfs" | cut -d' ' -f1) == "$image_hash" ]] || fail 'written live image checksum mismatch'
    cmp "$live/$kernel" "$dest/$kernel"
    cmp "$live/$initrd" "$dest/$initrd"
    (cd "$dest" && sha256sum filesystem.squashfs "$kernel" "$initrd" | sed "s#  #  /live/$slot/#") > "$dest/payload.sha256"
    flush_tree "$dest"
    sync "$target/live" "$target"
    # Immutable dispatcher: updates never overwrite the working boot selector.
    # Both slot descriptors are independently checked; the newest committed one
    # wins. A torn or missing new descriptor leaves the other slot available.
    if [[ $action == create ]]; then
        cat >"$work/esp/boot/grub/grub.cfg" <<CFG
serial --speed=115200
terminal_input console serial
terminal_output console serial
set timeout=3
search --no-floppy --fs-uuid --set=root $boot_uuid
insmod hashsum
insmod gcry_sha256
set nestegg_a_generation=0
set nestegg_b_generation=0
search --no-floppy --fs-uuid --set=root $slot_a_uuid
if [ -f /live/a/commit.sha256 ]; then
  if hashsum --hash sha256 --check /live/a/commit.sha256; then
    source /live/a/boot.cfg
  fi
fi
search --no-floppy --fs-uuid --set=root $slot_b_uuid
if [ -f /live/b/commit.sha256 ]; then
  if hashsum --hash sha256 --check /live/b/commit.sha256; then
    source /live/b/boot.cfg
  fi
fi
if [ "\$nestegg_b_generation" -gt "\$nestegg_a_generation" ]; then
  nestegg_boot_b
  if [ "\$nestegg_a_generation" -gt 0 ]; then nestegg_boot_a; fi
elif [ "\$nestegg_a_generation" -gt 0 ]; then
  nestegg_boot_a
  if [ "\$nestegg_b_generation" -gt 0 ]; then nestegg_boot_b; fi
else
  echo 'No committed nestegg live slot; recreate the boot medium.'
fi
CFG
        # Create-only files must also be durable before the slot descriptor.
        sync "$work/esp/nestegg-stick"
        flush_tree "$work/esp/boot"
        flush_tree "$work/esp/EFI"
        sync "$work/esp"
    fi
    cfg=$dest/boot.cfg
    render_cfg "$slot" "$generation" "$kernel" "$initrd" > "$cfg"
    sync "$cfg" "$dest"
    sync "$target"
    # Publish ONLY after the entire payload and boot descriptor are durable.
    # The previous active descriptor is never replaced or renamed by update.
    cfg_hash=$(sha256sum "$cfg" | cut -d' ' -f1)
    printf '%s  /live/%s/boot.cfg\n' "$cfg_hash" "$slot" > "$dest/commit.sha256"
    sync "$dest/commit.sha256" "$dest" "$target/live" "$target"
else
    image_hash=$(sha256sum "$work/slot_$slot/live/$slot/filesystem.squashfs" | cut -d' ' -f1)
fi
[[ $image_hash =~ ^[0-9a-f]{64}$ ]] || fail 'invalid image hash'
printf '{"data_uuid":"%s","boot_uuid":"%s","active_slot":"%s","image_sha256":"%s"}\n' "$data_uuid" "$boot_uuid" "$slot" "$image_hash" >&3
