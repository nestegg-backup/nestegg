# 0010 – Rescue wizard: any non-empty disk needs an explicit second confirmation

- Status: accepted
- Date: 2026-11-09
- Changes: decision 0006, point 5 ("a data disk of the backed-up computer is
  flagged")

## Context

The rescue wizard only warned about a target disk when it held the UUID of
a mount that `layout.json` classified as "not part of the system". That
missed the most likely accident: the system SSD dies, `/home` lived on a
second disk that still works, and the new SSD is too small, so the `/home`
disk is the only one that fits. `/home` counts as a system mount, so no
warning appeared. Software RAID members, ZFS members, ntfs-3g disks and
data disks that were unmounted during the backup were not recognised at
all, and the wizard pre-selected the first disk in the list.

## Decision

1. **Every disk is classified before it is offered** (`rescue/guard.rs`,
   pure functions with unit tests). A disk is *not empty* when it or a
   partition carries any signature known to `lsblk`/`blkid` (file systems,
   swap, md/firmware RAID, ZFS, LVM, LUKS, BitLocker) or has partitions
   without a recognisable signature. A partition table without partitions
   counts as empty.
2. **Matching against the backed-up computer** uses the information the
   snapshot already carries in `inventory.pxar`: `layout.json` (all mounts
   and swap, whatever their role), `fstab`, `crypttab` and `lsblk.json`.
   No new fields in `layout.json`, so older snapshots profit as well. The
   message says what the disk held and what that means: a system mount
   comes back from the backup but later changes are lost; a data disk is
   not in the system backup at all.
3. **No default for a disk that is not empty.** The wizard pre-selects a
   disk only if exactly one disk is empty and large enough for the plan;
   otherwise the user types a number.
4. **Second confirmation.** Choosing a non-empty disk shows model, size,
   serial, path and the findings, asks "Use this disk anyway?", and after
   the plan requires typing `ERASE <name>` in addition to the disk name.
   The non-interactive `rescue restore` refuses such a disk unless
   `--erase-data-disk` is given (previously only for data disks of the
   backed-up computer).
5. `mdadm` is not added to the rescue system: assembling arrays would make
   their members "in use" but also risks resyncs on a degraded array. The
   members are recognised by their signature instead.

## Consequences

- Restoring onto the old, still working system disk, or onto a disk with a
  preformatted vendor file system, now needs the extra confirmation (or
  `--erase-data-disk` in automation).
- A whole-disk encryption without any header (plain dm-crypt, VeraCrypt on
  the raw disk) still looks empty; partitions without a signature are
  caught.
