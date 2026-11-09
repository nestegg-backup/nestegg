# Rescue stick: restore a computer onto an empty disk

When the system disk has died, or a computer has to be set up again from
scratch, the nestegg rescue stick puts the complete system back from the
Proxmox Backup Server: partitions, files, users, settings, boot loader.

Decision and scope: [decisions/0006](decisions/0006-bare-metal-restore-rescue-stick.md),
target disk guard: [decisions/0010](decisions/0010-rescue-target-disk-guard.md).

## Before you need it

1. **Full system backups must be on.** New installations have
   `system = true` in `[backup]` (see `nestegg config`). Older
   configurations: add it and run one backup.
2. **Keep the key and the settings outside the computer**, e.g. on a second
   USB stick:

   ```sh
   sudo nestegg key export /media/usb/nestegg-key.json
   sudo nestegg rescue export-settings /media/usb/nestegg-rescue.toml
   ```

   The settings file contains the PBS address, datastore, namespace, token
   name and certificate fingerprint, never a password. You also need the
   token secret (password manager).
3. **Write the rescue image to a USB stick** (all data on the stick is lost):

   ```sh
   sha256sum -c nestegg-rescue-amd64.iso.sha256
   sudo dd if=nestegg-rescue-amd64.iso of=/dev/sdX bs=4M conv=fsync status=progress
   ```

   Any tool for writing ISO images works (GNOME Disks, balenaEtcher, Ventoy).

## Restoring

1. Boot the computer from the stick (boot menu key, often F12, F11 or Esc).
   Secure Boot may need to be switched off for the stick. Switching it on
   again afterwards is fine either way: nestegg's stored secrets do not
   depend on the Secure Boot state.
2. The wizard starts by itself. Plug in a network cable, or leave the wizard
   with Ctrl+C, run `nmtui` for Wi-Fi and start it again with `nestegg rescue`.
3. Plug in the stick with the key and settings; the wizard finds them. It
   looks at other disks read-only, without replaying journals, so nothing is
   written to them.
4. Answer the questions: server, token secret, key file, computer, backup,
   target disk, optional encryption. The disk list shows model, size and
   serial number of every disk and marks it `empty`, `NOT EMPTY` or
   `HOLDS DATA OF THE BACKED-UP COMPUTER`, with what was found on it (see
   below). A disk is pre-selected only if exactly one disk is empty and big
   enough; otherwise you have to type its number.
5. Check the plan and type the disk name (e.g. `nvme0n1`) to start. Only now
   is the disk erased. The plan lists what is currently on the disk. A disk
   that is not empty needs a second confirmation: type `ERASE <name>`
   (e.g. `ERASE sdb`). Disks that are mounted, used as swap, or in use by
   active LVM, encryption or RAID are refused.
6. Remove the stick and restart.

The restored system contains a report of what was changed:
`/var/lib/nestegg/rescue-report.txt`. The previous versions of modified
configuration files are kept below `/var/lib/nestegg/rescue-backup/`.

### After restoring onto new hardware

The restored system brings back nestegg's stored token secret and key, but
they are sealed to the TPM of the computer that made the backup. On the same
computer (new disk only) they keep working. On a new mainboard or another
computer they cannot be decrypted, and backups report "stored secrets
unreadable". Run `sudo nestegg backup` once to find out, and if it reports
that, store them again with the key file and token secret you used for the
restore:

```sh
sudo nestegg credentials set-password                  # token secret
sudo nestegg key import /media/usb/nestegg-key.json    # same key, no new upload of old data
sudo nestegg backup
```

Importing the same key keeps deduplication against the old snapshots and
keeps them restorable from the web UI. Do not use `nestegg key create
--force` for this: a new key means a full upload and old snapshots need the
old key file.

### Without the wizard

```sh
nestegg rescue disks
nestegg rescue restore --settings /media/usb/nestegg-rescue.toml \
  --password-file /root/token --keyfile /media/usb/nestegg-key.json \
  --disk /dev/nvme0n1 --wipe /dev/nvme0n1 [--snapshot <path>|latest] \
  [--backup-id <other machine>] [--encrypt-passphrase-file /root/pass] [--dry-run]
```

`--wipe` must repeat the disk; `--dry-run` only prints the plan. A disk that
is not empty is refused unless `--erase-data-disk` is given.

### When is a disk "not empty"?

A disk counts as not empty when it, or a partition on it, carries any
signature that `lsblk`/`blkid` recognise (a file system such as ext4, ntfs
or vfat, swap, a software RAID member, a ZFS pool member, an LVM physical
volume, a LUKS or BitLocker container), or when it has partitions without a
recognisable signature (unknown or encrypted content). A partition table
without partitions counts as empty.

It is additionally marked as holding data **of the backed-up computer** when
one of its file system, LUKS or partition UUIDs appears in the snapshot's
inventory: `layout.json` (mounted file systems and swap), `fstab` and
`crypttab` (also unmounted or `noauto` entries) and the `lsblk` listing
(RAID and ZFS members, ntfs-3g disks, ...). The message says what it was,
e.g. that it held `/home` (it comes back from the backup, but everything
written after the backup is lost) or a data disk that is not part of the
system backup at all. The stick with the key or settings file is marked as
well. Software RAID arrays are not assembled by the rescue system; their
members are recognised by their signature only.

## What the restore does

| Old system | Restored system |
|---|---|
| UEFI or BIOS | Boot mode of the rescue boot (a change is allowed and noted) |
| EFI system partition | New 1 GiB ESP at the same mount point |
| Root on ext4, btrfs or xfs | Same type on one partition (ext2/3 become ext4) |
| btrfs subvolumes (`@`, `@home`, …) | Recreated with the same names; snapshot subvolumes empty |
| Separate `/home`, `/boot`, `/var` partitions | Merged into `/` |
| Swap partition / swap file | Recreated (capped at 16 GiB); with encryption a swap partition becomes a swap file (on btrfs in its own subvolume `@swap`, so snapshots keep working) |
| LUKS encryption | Only if chosen in the wizard: new LUKS2 root, unencrypted `/boot` |
| LVM | Not recreated (plain partitions) |
| Bind mounts (`/srv/data` from `/home/alice/data`) | fstab line kept as it is |
| Data disks, other partitions in fstab | Untouched; fstab entry gets `nofail` |

Then fstab, crypttab, resume device and GRUB settings are pointed to the new
UUIDs, the initramfs is rebuilt and GRUB is installed (UEFI: also in the
fallback path `EFI/BOOT/BOOTX64.EFI`, so the disk boots even when the firmware
loses its boot entries). On BIOS computers the new disk is also recorded
for later GRUB package updates. New file systems are created without the
newest ext4/xfs features, so older systems (Ubuntu 22.04, Linux Mint 21,
Debian 12) can boot and check them.

Not part of the full system backup and therefore not restored:
`/tmp`, `/var/tmp`, downloaded `.deb` files, swap files, excluded caches
(`backup.excludes`), and data disks (restore those with `nestegg restore`).

## Limits

- Debian-based systems with GRUB. Systems with systemd-boot, ZFS or bcachefs
  root, and backups without a Debian package list (Fedora, Arch, …), are
  refused before anything is erased.
- The target disk must hold the used data plus about 15 % (at least 2 GiB).
  On btrfs the size of `system.pxar` counts, not the space used by
  snapshots.
- btrfs subvolumes that are not mounted (e.g. `~/Projects` created as a
  subvolume) are backed up and come back as normal folders. Docker's and
  container subvolumes and snapshots are left out.

## Building the stick yourself

```sh
cargo build --release
packaging/rescue/build.sh target/release/nestegg target/nestegg-rescue-amd64.iso
```

Needs Docker (privileged container with Debian `live-build`) and network
access. The end-to-end test boots everything in VMs:

```sh
sudo tests/rescue/run.sh target/release/nestegg target/nestegg-rescue-amd64.iso debian-btrfs-uefi
sudo tests/rescue/run.sh target/release/nestegg target/nestegg-rescue-amd64.iso ubuntu-ext4-bios-luks
```

CI runs both variants weekly, on pull requests that touch the rescue code
(`.github/workflows/rescue.yml`) and before every release.
