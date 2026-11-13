# 0027 – Restore and offline backup from the booted backup stick

- Status: implemented; disposable VM acceptance passed; hardware acceptance pending
- Date: 2026-11-11
- Implements: phases three and four of [0023](0023-backup-stick.md)
- Extends: 0006, 0010, 0011, 0016, 0019 and 0024–0026

## Decision

1. Identify the booted backup medium through the kernel's UUID-pinned
   `live-media` and `/live/a` or `/live/b` argument, the read-only mounted slot partition (0030),
   its exact versioned marker, and its sibling ext4 data partition. Reject
   duplicate data UUIDs and a data partition on another disk. A volume label
   alone is never an identity. An ordinary rescue ISO retains its PBS flow.
2. The desktop launcher keeps its fixed `IMAGE_PATH`, protected provenance and
   checksum checks. A separate internal live entry point resolves the squashfs
   at `/run/live/medium/live/{a,b}/filesystem.squashfs` and checks that the loop
   mounted by live-boot at `/run/live/rootfs/filesystem.squashfs` uses exactly
   that backing file. The booted image is already trusted executable code;
   another mounted USB image is not. The same launcher supplies fresh RAM-only
   configuration, certificate, owner token and private network/PID/mount
   namespaces. No user token, certificate question or network is needed.
3. Restore uses the existing signed-manifest/key checks, computer/snapshot
   selection, target guard and restore execution. Ship `grub2-common` as the
   live fallback installer for firmware-mode changes, and check `grub-install`
   before erasing any target. GRUB function definitions must be outside its
   conditional blocks; gate their invocation with the verified generation.
   Media tests check the kernel's actual slot argument, rather than only the
   existence of that slot's payload, so an update cannot silently boot the old
   slot. Keys are opened with the
   official client's passphrase changer in a private scratch directory, then
   removed. The encrypted stick copy is read with O_NOFOLLOW and bounded in
   size. Paper keys remain an independent fallback.
4. The desktop daemon runs each stick file operation in a separate root child
   with its own mount namespace; the threaded daemon never changes namespaces.
   Only registered media and the fixed desktop cache are used. Queries and
   restores select `target=stick` explicitly, with an optional registered
   `stick_uuid`. Only the configured computer/key is exposed. Non-root callers
   need signed source metadata and remain confined to their own home directory;
   catalogs are private scratch files and are not reused across attachments.
   Restores recheck permissions in the worker and use the common staging and
   identity-dropping restore implementation. Workers hold the shared run lock.
5. Offline backup first refuses mounted sources, swap and existing holders,
   requires a RAM workspace before opening keys, then sets the whole source
   disk and its partitions kernel-read-only before
   opening LUKS or probing filesystems. This flag refuses writable opens and
   makes filesystems skip error-info writes, but it is not a block-layer write
   barrier: the kernel only warns when an in-kernel user such as a device-mapper
   target writes (reproduced below). Safety therefore rests on never activating
   anything that writes on top of the source. LUKS is opened with `--readonly`. LVM
   activation uses `read_only_volume_list`, not a metadata-changing LV permission
   edit, and `metadata_read_only=1`; ordinary linear LVs are supported. Automatic
   LVM activation is disabled in the live image. Restore source flags after
   unmounting and closing every acquired mapping. Never write the hardware clock.
6. Final source mounts use `ro,nodev,nosuid,noexec` and `noload` (ext3/4; ext2 must be clean),
   `norecovery` (xfs), or `rescue=nologreplay` (btrfs). ext filesystems needing
   journal recovery use a transient dm-snapshot whose COW loop resides on a
   dedicated `noswap` tmpfs. xfs/btrfs conservatively use the same layer. Replay
   occurs on a read-only mount of the writable RAM snapshot, followed by a
   no-replay read-only mount. Never use snapshot-merge or a writable origin.
   COW capacity is bounded; invalid/overflowed snapshots fail before pruning.
   Refuse external ext journals and multi-device btrfs before mounting: otherwise
   the kernel could replay a journal onto a second, unprotected source device.
7. System mounts are resolved from the source fstab on the selected disk.
   Missing system filesystems fail instead of silently making a partial backup.
   Apply the shared mount-role function to all fstab entries: include named
   sibling Btrfs system subvolumes (including application directories), record
   intentionally skipped root subvolumes for empty recreation, and exclude
   their contents. Refuse unsupported `subvolid=` selection before upload.
   Compare canonical identities on both sides for mapper aliases.
   Non-system data disks are not mounted. Inventory and layout are generated
   from the mounted computer. File reads use openat2 with RESOLVE_IN_ROOT to
   contain absolute symlinks. dpkg reads a private copy of the source status
   database with `--admindir`; systemctl uses `--root`. Snap/Flatpak deployment
   state is read from files, without invoking their live daemons or source code.
8. The backup ID is read from the source configuration or its hostname and is
   confirmed in the wizard. A new key must first get a strong-password scrypt
   copy on the stick, read back and compared; a paper key is offered. Group
   ownership is checked with the current key before upload and before pruning.
   Existing copies belonging to another key are never replaced.
9. Every offline run confirms UTC time and displays the newest computer snapshot.
   Bad RTC dates or times not later than the newest snapshot require correction.
   Non-interactive runs require explicit clock confirmation or a supplied time.
   Uploads are encrypted, include layout/inventory and signed source metadata,
   undergo the existing decrypted read-back and synchronous PBS verification,
   then use the phase-two retention and guarded GC. Without durable live monthly
   verification state, each live backup forces full datastore verification.

## Acceptance

Verified on 2026-11-11 with generated credentials and disposable files only:

- `cargo test`: 530 passed; the two normally ignored privileged launcher tests
  also passed inside `tests/stick/create.sh`. Clippy with all targets and
  warnings denied, formatting, shell checks, unique-key YAML validation and
  the repository privacy tree scan passed.
- `tests/stick/rescue.sh`: desktop backup to the stick, then network-free
  wizard restore and a successful boot with the original checksum and owner.
  Desktop API listing, browsing, refusal of another user's/system files and
  restoration of the requester's file passed as a normal group member.
- Offline plain ext4, LUKS plus linear LVM, killed-mid-write ext4, and a system
  without nestegg: encrypted backup, decrypted read-back, server verification,
  restore onto an empty disk and successful boot all passed. Full source-image
  SHA-256 values stayed identical in every case. Dirty journal recovery
  allocated sectors in the RAM COW device. The no-nestegg case exercised the
  wizard, a new key and a password-protected recovery copy.
- Additional Btrfs VM: source SHA-256 stayed identical; restore booted, retained
  the witness in the separate `/var/lib/mysql` subvolume and recreated skipped
  snapshot subvolumes. This exercises the independent review's mount-scope fix.
- All three existing rescue CI scenarios passed: Debian/Btrfs/UEFI wizard
  (61 checks), Ubuntu/ext4/BIOS/LUKS wizard (60), and its non-interactive variant
  (25). VM guests received the exact tested binary; the built ISO supplied the
  live operating system, GRUB fallback and official PBS implementation.
- `tests/stick/create.sh`: 14 BIOS/UEFI boots, including legacy-format upgrades,
  actual selected-slot assertions, torn updates and retry; retained encrypted
  snapshots and all existing media guards passed.

An independent read-only review identified dropped sibling Btrfs mounts,
unsupported subvolume-ID selection, mapper-alias comparison and a missing CI
checksum artifact. All four were addressed; unsupported `subvolid=` layouts
are explicitly rejected. Owned VM processes, containers, loop/mapping devices,
mounts and generated test credentials are removed after verification.

Physical USB hardware, flash endurance and real power loss remain phase six.
XFS and ext2/3 option/guard logic has unit coverage; this VM matrix exercises
ext4 and Btrfs, not a complete XFS or ext2/3 restore cycle.

## Implementation references

- [Kernel dm-snapshot documentation](https://www.kernel.org/doc/html/latest/admin-guide/device-mapper/snapshot.html)
- [LVM activation and metadata-read-only configuration](https://gitlab.com/lvmteam/lvm2/-/blob/main/conf/example.conf.in)

## Offline review hardening

All ext2/3/4 source mounts, including the RAM journal replay mount, explicitly
use `errors=continue`. The mount is already read-only, so remount-ro cannot add
protection; overriding the source superblock's `errors=panic` avoids a live-kernel
panic. The writeable RAM snapshot alone receives journal replay.

The phase-two integration audit also checked the shared Python key helper
(`/usr/bin/python3 -I`, root working directory and cleared PYTHON variables) and
`BootMedium::open`'s shared recursively slave mount namespace. Both fixes are
already reused by the offline flow. Cancellation handling is checked separately
alongside verification and retention; offline sessions have no daemon retry state.

Offline completion follows decision 0028: independently verify the exact new
snapshot and require its PBS `ok` state, then attempt full datastore verification.
Full verification failure emits `stick_damaged` but does not block this computer's
key-guarded prune or GC. Cancellation and media removal are checked between
maintenance steps, including before interpreting a failed full pass as damage.
A cancelled read-back is labelled cancelled. Unit tests cover foreign corruption,
new-snapshot failure, cancellation during the full pass and missing media.

The immutable encrypted metadata now optionally includes a normalized SHA-256
of `/etc/machine-id`, populated by desktop and offline backups. Before upload,
offline backup compares hostname and machine identity with the newest group's
signed metadata. A mismatch, missing old metadata or unavailable identity needs
an explicit default-no wizard confirmation or non-interactive `--force-id`.
The override never bypasses signature, snapshot binding or key ownership checks.
A distinct backup ID remains the recommended choice. Exact OS clones that also
share a machine-id cannot be distinguished by this identity check.

LVM inspection uses `lvs -a --segments` with `lv_attr` and `lv_role`. Only
inactive visible ordinary (`lv_attr[0] == '-'`) public linear LVs are accepted.
Origins, pending merges, snapshots, virtual/mirror/RAID volumes and hidden LVs
are refused before activating any VG. A separate read-only `vgcfgbackup` export
must contain only striped segment metadata (the LV report also requires the
single-stripe linear interpretation), rejecting hidden snapshot segments.
Activation uses `lvchange` with a public/linear/ordinary LV selection evaluated
under LVM's VG lock, read-only activation permissions, metadata-read-only and
no background polling. Polling control alone cannot prevent kernel merges.
The current LVM activation CLI exposes no unconditional no-merging switch;
the upstream internal `no_merging` option is not a CLI guarantee.
See [LVM device activation](https://github.com/lvmteam/lvm2/blob/main/lib/activate/dev_manager.c)
and [LV activation command](https://github.com/lvmteam/lvm2/blob/main/tools/lvchange.c).

The `pending-merge` VM scenario compares the entire image before/after refusal,
checks that no mappings were acquired and that source read-only flags were
restored, then tries the former activation on a separate disposable image copy.

Result on 2026-11-12 (Debian 13 rescue image, kernel 6.12.116, LVM 2.03.31,
dm driver 4.48.0): `nestegg rescue backup` refused the VG with "snapshots,
merges and hidden LVs are refused", created no mapping, restored the source
read-only flag, and the SHA-256 of the entire source image was unchanged. The
fixture's own LV report is asserted to contain a merging origin and snapshot;
in the reproduction VM, the byte-identical copy showed the origin as
`Owi---s---` and the hidden COW as `[saved]` with `Swi---s---`; the report check alone already refuses it, so the metadata
export and activation selection remain second and third lines of defence
(covered by unit tests). The former `vgchange -ay` activation, with
`read_only_volume_list`, `metadata_read_only=1`, `--monitor n` and
`blockdev --setro` on the whole disk, loaded a `snapshot-merge` table. The
kernel logged "Trying to write to read-only block-device" for `dm-0` and `vdb`
and the image copy's checksum changed: the original write behaviour was
reproduced. An earlier attempt of this scenario never reached the LVM check,
because its 512 MiB fixture was below the 1 GiB minimum for a computer disk;
the fixture is now 2 GiB (sparse) and the test asserts the exact LVM refusal.
The `id-collision` scenario follows `plain`, shares its key, hostname and backup
ID with a different machine-id, requires refusal, then exercises `--force-id`.
