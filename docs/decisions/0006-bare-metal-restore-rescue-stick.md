# 0006 – Bare-metal restore with a rescue stick

- Status: accepted (target disk guard, point 5, changed by 0010)
- Date: 2026-11-08
- Changes: decision 0003 ("Disk images and bare-metal restore" out of scope)

## Context

Decision 0003 limited v0.1 to home directories, system configuration and a
package inventory ("reinstall the distribution, then restore"). In practice
that is a long manual job after a disk failure. Comparable home-lab projects
offer a bootable USB stick: it boots a small Linux, connects to the PBS and
puts the whole system back onto an empty disk. The maintainer wants the same
for nestegg, plus one place to download the client packages and the stick
image.

## Decision

1. **Full system backup, file level.** A new option `backup.system` (default
   for new installations via `nestegg init`, opt-out `--no-system`) backs up
   `/` as `system.pxar` with the system's own mounts included
   (`/home`, `/boot`, `/boot/efi`, `/var`, …, btrfs subvolumes of the root
   device). Data disks stay separate sources. No block images: file-level
   backups deduplicate well, work on a running desktop without snapshots, and
   stay restorable file by file. A source stays its own archive whenever it
   is not on a system file system: network shares, FUSE mounts, bind mounts
   and `require_mount` sources. Unmounted btrfs subvolumes inside the system
   (e.g. `~/Projects`) are included and come back as normal folders;
   snapshot and container subvolumes are left out.
2. **The disk layout is recorded with every backup** (`layout.json` in
   `inventory.pxar`): file systems, btrfs subvolumes, swap, boot mode, boot
   loader, encryption. The same code selects the mounts at backup time and
   recreates them at restore time.
3. **Rescue stick = Debian live ISO** (hybrid, UEFI and BIOS) built with
   `live-build`, containing nestegg, `proxmox-backup-client-static`, disk
   tools, GRUB and common network firmware. It starts a text wizard on the
   first console (`nestegg rescue`). The same restore runs non-interactively
   (`nestegg rescue restore …`) for automation and tests.
4. **Restore creates a simple, new layout** instead of cloning the old one:
   ESP (UEFI) or BIOS boot partition, optional swap partition, one root file
   system of the original type, btrfs subvolumes with their original names.
   Other separate system partitions are merged into `/`. Optional LUKS2
   encryption of the new root (with an unencrypted `/boot`). LVM is not
   recreated. fstab, crypttab, resume and GRUB settings are rewritten to the
   new UUIDs, entries for other disks get `nofail`. GRUB is reinstalled and
   the initramfs regenerated inside the restored system.
5. **Nothing is changed before the user confirms** by typing the disk name;
   disks that are mounted, in use by LVM/LUKS/RAID or hold the rescue system
   are refused. The plan shows the current content of the disk; a data disk
   of the backed-up computer is flagged and, without the wizard, refused
   unless `--erase-data-disk` is given. Searching attached media for the key
   never writes to them (read-only, no journal replay). Backups without a
   Debian package list are refused before anything is erased.
   New file systems use features that older targets (GRUB 2.06, e2fsprogs
   1.46, Linux 5.10) support.
6. **Scope:** Debian-based systems with GRUB (Ubuntu, Linux Mint, Debian),
   root on ext4, btrfs or xfs. Other boot loaders (systemd-boot) and root file
   systems are refused with a clear message before anything is erased.
7. **Downloads** are release artifacts: the `.deb` package and the rescue ISO
   (with SHA-256 checksum) are built by CI and attached to tagged releases.
   nestegg is Linux-only; there are no clients for other operating systems.
   Publishing a release needs an explicit go from the maintainer.

## Consequences

- Backups grow by the size of the operating system (typically 5–20 GB);
  deduplication on the PBS makes repeated runs and several machines with the
  same distribution cheap.
- The PBS token needs restore rights for the machine's own group
  (`DatastoreBackup` is enough when the same token made the backups).
- The ISO pulls the Proxmox client repository with the same pinned keyring
  as the end-to-end tests; its build needs network access and Docker with
  `--privileged`.
- The restore is tested end to end in VMs (`tests/rescue/run.sh`): a real
  desktop VM backs itself up, the ISO restores it onto an empty disk, and the
  restored disk must boot on its own.
