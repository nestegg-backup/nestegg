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

Update initially mounts the ESP and both slots read-only, parses descriptors
as data and checks the inactive partition's available/reclaimable space. It
remounts **only the inactive partition** read-write. It removes and fsyncs any
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
`stick add` still installs it only from the trusted release ISO. The JSON CLI
fields are unchanged; `boot_uuid` continues to identify the ESP.

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
