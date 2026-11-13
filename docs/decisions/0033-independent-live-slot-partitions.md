# 0033 – Independent live-slot filesystems, immutable update-time ESP

- Status: implemented; disposable acceptance results recorded below
- Date: 2026-11-13
- Supersedes: layout in 0023 point 4 and 0024 points 3–4
- Resolves: 0025 finding 7 (shared FAT metadata can damage the active slot)

## Decision

The GPT contains five partitions: a 1 MiB BIOS boot partition, a 64 MiB FAT32
ESP containing GRUB, independent FAT32 slots `NESTEGG_A` and `NESTEGG_B`, and
ext4 `NESTEGG_DATA` taking the rest. Each slot is at least 2048 MiB, or 150% of
the release ISO size rounded to MiB plus 128 MiB when larger. This leaves ample
headroom above today's approximately 620 MB image. Data needs at least 2 GiB
and is made with `mkfs.ext4 -m 0`; its datastore stays under `nestegg/store/`.
Too-small media are refused before partitioning, without weakening the disk
guard, typed confirmation or post-confirmation identity check.

The ESP carries an exact `nestegg-stick-v2` marker binding its UUID, both slot
UUIDs and the data UUID. Each slot has an exact `nestegg-slot-v2` root marker
binding its own UUID to the sibling data UUID. The existing `/live/a` and
`/live/b` payload paths remain, each on its respective filesystem. There is no
migration: the unreleased `nestegg-stick-v1` shared-FAT layout is explicitly
refused by `info`, `update` and `add`, explaining that recreation erases backups
and needed backups must first be saved elsewhere. Legacy descriptor renderers
and their upgrade acceptance path are removed.

Both BIOS and UEFI GRUB use the same fixed dispatcher on the ESP. It searches
for each slot UUID, validates that slot's `commit.sha256` against `boot.cfg`,
then validates the kernel/initrd/squashfs manifest before accepting its
generation. It offers the newest committed slot first and the other valid slot
as a manual fallback. No mutable GRUB environment or ESP selector is needed.
Checksums protect completeness, not authenticity of a hostile boot medium.
GRUB itself is installed only at creation; a future bootloader or partition-
format change requires explicit recreation, not an ordinary live update.

Update initially mounts the ESP read-only and attempts both slots read-only,
parses descriptors as data and checks the inactive partition's
available/reclaimable space. It remounts **only the inactive partition**
read-write. It removes and fsyncs any
old inactive commit before replacing payload bytes, verifies copied files,
fsyncs every payload file and directory, writes and fsyncs the generated boot
descriptor, and publishes/fsyncs its checksum commit and parents last. Neither
the active slot nor the ESP is written, including FAT/FSInfo metadata. The GPT
and data partition are untouched. An inactive filesystem's torn metadata can
no longer corrupt the active filesystem or dispatcher through shared sectors.

GRUB passes the selected slot UUID as `live-media`, keeping the matching
`live-media-path=/live/a` or `/live/b`. Live-boot mounts that slot at
`/run/live/medium`. Booted PBS still uses
`/run/live/medium/live/{a,b}/filesystem.squashfs`, verifies live-boot's actual
squashfs loop backing file, and checks the data partition is the unique ext4
sibling on that same disk. An attached rescue ISO cannot supply its image or
data. Desktop PBS remains the independently verified fixed local cache;
`stick add` still installs it only from the trusted release ISO. Existing JSON CLI
fields retain their meaning; `boot_uuid` continues to identify the ESP.
`inactive_slot_damaged` additionally reports a damaged sibling filesystem
or marker.

## Verification

`tests/stick/create.sh` compares SHA-256 over the **whole previous active
partition and ESP**, before/after a successful update, a half-copy interruption,
and interruptions before payload, descriptor and commit fsyncs. It boots the
resulting selections with SeaBIOS and OVMF alongside a competing rescue ISO,
checks actual slot/device arguments, retains encrypted snapshots and tests
retry, format rejection, corruption, capacity, clone and disk guards.
`tests/stick/durability.sh` logs only the inactive partition with dm-log-writes,
replays every flush/FUA/fsync boundary, validates any published commit against
its payload and separately checks the entire active partition and ESP remain
unchanged. Their device-mapper mappings are read-only during the recorded
update, so accidental writes fail rather than merely changing a checksum. Real file/directory fsyncs and the syncfs negative control remain.

Verified on 2026-11-13 using disposable image files only:

- `cargo test`: 572 passed; three normally ignored privileged launcher tests
  also passed through the media driver. Clippy with all targets and warnings
  denied, rustfmt, ShellCheck on changed scripts and the privacy tree scan passed.
- `tests/stick/create.sh`: 20 SeaBIOS/OVMF boots passed, including explicit
  manual fallback, corrupt descriptors/payload, four interruption points and
  retry. Whole active-partition and ESP hashes stayed identical for both
  update directions and all four interruptions. Encrypted snapshots, GPT and
  data UUID survived; nonempty recreation and v1 rejection passed.
- `tests/stick/durability.sh`: 50 flush/FUA/fsync replay cuts; six exposed a
  complete new commit. ESP and previous active partition stayed byte-identical,
  with both mappings read-only throughout the recorded update.
- `tests/stick/desktop.sh` and `desktop-vm.sh`: encrypted backup/read-back,
  verification, retention/GC, key/UUID/media faults and real guest udev/systemd
  hotplug acceptance passed. The post-confirmation capacity-change CLI test
  also passed without any partition writes or retained loops.

- `tests/stick/rescue.sh all`: all eight scenarios passed. Desktop, plain,
  identity collision, LUKS/LVM, dirty ext4, no-nestegg and Btrfs each completed
  backup, restore onto an empty disk and boot with the original file checksum
  and owner. All six offline source images stayed byte-identical; dirty journal
  recovery wrote into RAM. The pending-merge case refused activation without
  acquiring mappings, restored read-only flags and retained the full source
  checksum. Its unsafe-activation control changed only a separate disposable
  copy, reproducing the old failure.

Physical flash endurance and controllers that corrupt sectors outside the
written partition or lose acknowledged writes remain hardware acceptance
concerns; partition isolation cannot repair such firmware failures.

## After review: damaged inactive slots

The exact ESP marker is the source of the recorded FAT volume identities,
including when the inactive filesystem no longer probes or mounts. The ESP
and data identities must still match the fixed sibling partitions on the
confirmed disk, and a slot is eligible as active only when its filesystem
identity, slot marker, descriptor, commit and payload validate. With one
valid active slot, `info` and `add` succeed and expose
`inactive_slot_damaged: true` for a damaged sibling filesystem or marker,
with a repair instruction. If no slot validates, operations still refuse.

`update` bounds the replacement payload against the inactive partition size
before writing, then rebuilds that partition with `mkfs.vfat -i` using its
recorded serial and restores its exact marker. Partition paths are always
fixed numbers on the confirmed disk, never values supplied by a marker.
The normal verified copy and real file/directory fsync barriers still publish
the commit last. The ESP, active partition, GPT and NESTEGG_DATA receive no
writes. No recreation, backup loss or disk-guard exception is required.

Acceptance adds destroyed inactive FAT metadata and a torn slot marker in
opposite update directions. Both `info` and `add` must report degradation;
BIOS/UEFI must boot the intact slot before repair and the new slot afterward.
Whole-partition active/ESP hashes, the GPT and encrypted snapshot hashes are
compared across both repairs.

Post-review verification on disposable images (2026-11-13):

- `tests/stick/create.sh` passed all 28 SeaBIOS/OVMF boots, both repair
  directions, truthful degraded `info`/`add`, whole active/ESP checksums,
  unchanged GPT and encrypted snapshots, and the crafted-marker update
  refusal with the entire disk image unchanged.
- `tests/stick/durability.sh` passed 50 replay cuts, six with a complete new
  commit; whole active/ESP checksums and their read-only mappings passed.
- `cargo test`, all-target Clippy with warnings denied, rustfmt, ShellCheck
  on changed scripts and the privacy tree check passed. The three privileged
  launcher tests also passed inside the media acceptance fixture.
- The first concurrent media run exceeded the existing 60-second half-copy
  probe wait under heavy I/O. The unchanged complete acceptance passed when
  rerun alone; no test deadline or assertion was weakened.
