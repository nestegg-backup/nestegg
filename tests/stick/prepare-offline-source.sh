#!/usr/bin/env bash
# Container-only fixture transformer. Every block device belongs to /work.
set -Eeuo pipefail
: "${MODE:?}" "${DATA_UUID:?}"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends gdisk e2fsprogs dosfstools cryptsetup lvm2 rsync python3 parted udev >/dev/null
mkdir -p /tmp/nestegg-fixture-lvm
printf 'activation { udev_sync=0 udev_rules=0 } backup { backup=0 archive=0 }\n' > /tmp/nestegg-fixture-lvm/lvm.conf
export LVM_SYSTEM_DIR=/tmp/nestegg-fixture-lvm
loop=; target=; crypt=; vg=; mnt=/mnt/source; dst=/mnt/destination
cleanup() {
    local rc=$? bad=0
    trap - EXIT
    set +e
    mountpoint -q "$dst/boot" && umount "$dst/boot"
    mountpoint -q "$dst" && umount "$dst"
    mountpoint -q "$mnt/var/lib/mysql" && umount "$mnt/var/lib/mysql"
    mountpoint -q "$mnt/home" && umount "$mnt/home"
    mountpoint -q "$mnt" && umount "$mnt"
    [[ -z $vg ]] || vgchange -an "$vg" >/dev/null || bad=1
    [[ -z $crypt ]] || cryptsetup close "$crypt" || bad=1
    [[ -z $target ]] || losetup -d "$target" || bad=1
    [[ -z $loop ]] || losetup -d "$loop" || bad=1
    ((bad == 0)) || exit 1
    exit "$rc"
}
trap cleanup EXIT
mkdir -p "$mnt" "$dst"
loop=$(losetup --find --show --partscan /work/source.img)
if [[ $MODE == btrfs ]]; then
    apt-get install -y -qq --no-install-recommends btrfs-progs >/dev/null
    mount -o subvolid=5 "${loop}p3" "$dst"
    btrfs subvolume create "$dst/@mysql" >/dev/null
    printf 'offline database witness\n' > "$dst/@mysql/nestegg-db-marker"
    mount -o subvol=@ "${loop}p3" "$mnt"
    mount -o subvol=@home "${loop}p3" "$mnt/home"
    mkdir -p "$mnt/var/lib/mysql"
    printf 'UUID=%s /var/lib/mysql btrfs defaults,subvol=@mysql 0 0\n' "$(blkid -s UUID -o value "${loop}p3")" >> "$mnt/etc/fstab"
    # Keep the command substitution literal for the restored guest's boot.
    # shellcheck disable=SC2016
    sed -i '/echo "E2E-SHA /a\  echo "E2E-MYSQL $(cat /var/lib/mysql/nestegg-db-marker)"' "$mnt/usr/local/sbin/nestegg-e2e"
else
    mount "${loop}p2" "$mnt"
    mount "${loop}p3" "$mnt/home"
fi
# Stable synthetic identities let the collision scenario share hostname/key
# with plain while representing a different installed computer.
printf '11111111111111111111111111111111\n' > "$mnt/etc/machine-id"
if [[ $MODE == id-collision ]]; then
    printf '22222222222222222222222222222222\n' > "$mnt/etc/machine-id"
fi
case "$MODE" in
    desktop)
        mkdir -p "$mnt/var/lib/nestegg/stick"
        cp /work/pbs.squashfs "$mnt/var/lib/nestegg/stick/pbs.squashfs"
        sha256sum "$mnt/var/lib/nestegg/stick/pbs.squashfs" | cut -d' ' -f1 > "$mnt/var/lib/nestegg/stick/pbs.squashfs.sha256"
        cp /work/key-password "$mnt/root/key-password"
        cat > "$mnt/etc/nestegg/config.toml" <<EOF
backup_id = "e2e-src"
[stick]
data_uuids = ["$DATA_UUID"]
[backup]
system = true
EOF
        sed -i 's/if nestegg backup; then/if nestegg stick key-copy --uuid '"$DATA_UUID"' --password-fd 11 11<\/root\/key-password \&\& nestegg backup --target stick; then/' "$mnt/usr/local/sbin/nestegg-e2e"
        ;;
    no-nestegg)
        rm -rf "$mnt/etc/nestegg" "$mnt/var/lib/nestegg"
        rm -f "$mnt/usr/local/bin/nestegg"
        ;;
    dirty)
        python3 - "$mnt/usr/local/sbin/nestegg-e2e" <<'PY'
import pathlib,sys
p=pathlib.Path(sys.argv[1]);s=p.read_text();s=s.replace('  echo "E2E-SOURCE-BOOT"', '''  echo "E2E-SOURCE-BOOT"
  sync
  (while true; do mkdir -p /var/lib/dirty-fixture; dd if=/dev/urandom of=/var/lib/dirty-fixture/writing bs=1M count=64 conv=fsync status=none; done) &
  sleep 3
  echo "E2E-DIRTY-WRITING"
  wait
''');p.write_text(s)
PY
        ;;
    plain|btrfs|id-collision) ;;
    luks-lvm)
        rm -f /work/encrypted.img
        truncate -s 8G /work/encrypted.img
        target=$(losetup --find --show --partscan /work/encrypted.img)
        sgdisk -n1:0:+1M -t1:ef02 -n2:0:+768M -t2:8300 -n3:0:0 -t3:8309 "$target" >/dev/null
        partprobe "$target"
        for _ in {1..100}; do
            [[ -b ${target}p2 && -b ${target}p3 ]] && break
            sleep .1
        done
        [[ -b ${target}p2 && -b ${target}p3 ]]
        mkfs.ext4 -q "${target}p2"
        crypt=nestegg-test-crypt-${DATA_UUID%%-*}; vg=nestegg-test-vg-${DATA_UUID%%-*}
        cryptsetup luksFormat --batch-mode --type luks2 --pbkdf pbkdf2 --key-file /work/luks-password "${target}p3"
        cryptsetup open --key-file /work/luks-password "${target}p3" "$crypt"
        pvcreate -y "/dev/mapper/$crypt" >/dev/null
        vgcreate "$vg" "/dev/mapper/$crypt" >/dev/null
        lvcreate -l 100%FREE -n root "$vg" >/dev/null
        mkfs.ext4 -q "/dev/$vg/root"
        mount "/dev/$vg/root" "$dst"
        rsync -aHAX --numeric-ids --exclude=/boot/ "$mnt/" "$dst/"
        mkdir -p "$dst/boot"
        mount "${target}p2" "$dst/boot"
        rsync -aHAX --numeric-ids "$mnt/boot/" "$dst/boot/"
        printf 'UUID=%s / ext4 defaults 0 1\nUUID=%s /boot ext4 defaults 0 2\n' "$(blkid -s UUID -o value "/dev/$vg/root")" "$(blkid -s UUID -o value "${target}p2")" > "$dst/etc/fstab"
        printf '%s UUID=%s none luks\n' "$crypt" "$(blkid -s UUID -o value "${target}p3")" > "$dst/etc/crypttab"
        ;;
    *) exit 2 ;;
esac
