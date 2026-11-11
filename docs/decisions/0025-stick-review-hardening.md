# 0025 – Backup-stick review hardening

- Status: implemented; loop/KVM/replay acceptance passed
- Date: 2026-11-11
- Refines: [0024](0024-stick-writing-and-launcher.md)

## Disk identity (review finding 2)

Keep the complete `lsblk` identity/content tree from the inspection displayed
before confirmation, including WWN, partition-table UUID, partition sizes and
starts. Read signatures directly with `blkid -p` to avoid stale udev properties
after hotplug and sort children by path. After acquiring the device lock,
inspect again and refuse any difference.
The private helper inherits a snapshot of mounts; it cannot see host mounts
created after namespace creation. The device lock is advisory, not protection
against a privileged process deliberately bypassing it.

## Configuration registration (review finding 5)

Reuse the settings writer to preserve existing ownership and mode. New
registration documents use 0640 root:nestegg and require the installed group.
The daemon's original mutex cannot coordinate a CLI process; both settings
updates and registration now hold the same stable sidecar flock over the entire
read/modify/write, including settings rollback. Never lock the replaced config
inode itself.

## Release inputs and temporary copies (review findings 6 and 8)

Require regular, root-owned ISO/checksum files with protected path components,
on fixed local storage. Reject FAT, optical filesystems, loop devices, USB and
removable block ancestors, including non-removable-looking partitions whose
parent is removable. Btrfs uses anonymous device numbers even on block storage:
resolve its UUID through `/sys/fs/btrfs/UUID/devices`, check every member and
refuse missing/degraded members. Only tmpfs/ramfs bypass block ancestry;
unidentified anonymous filesystems are refused. Block devices require a known
local transport, or a known VM/RAM disk identity; NBD, iSCSI and unknown network
or storage drivers are refused. Stacked devices require complete local ancestry.
See the [Btrfs sysfs interface](https://btrfs.readthedocs.io/en/latest/ch-sysfs.html).
A supplied `--sha256` pin takes precedence and does not
read the adjacent checksum. A sidecar alone detects damage, not authenticity:
an attacker able to replace both inputs can supply any privileged code.
Copy into the root-only desktop cache, not `$TMPDIR`. Install signal handling
before opening the owned loop or copying the ISO; interruptible copying unwinds
the temporary directory before loop detachment. Poll confirmation input rather
than stdio read_line, whose automatic EINTR retries would ignore Ctrl-C while
waiting at the prompt. Both copy-time and confirmation-time interruption are
tested through the real CLI. SIGKILL cannot run cleanup.

## Launcher provenance and pinned datastore (review findings 3 and 4)

Remove `LauncherOptions.image`; `LocalPbs::start` uses only the public
`IMAGE_PATH` constant `/var/lib/nestegg/stick/pbs.squashfs`. Verify the recorded
SHA-256 of the opened image, reject removable/FAT backing storage, and mount
that same descriptor. All other public launcher methods/options stay stable.
Open the data mount with O_DIRECTORY|O_NOFOLLOW, open `nestegg/` relative to it,
and validate both fstat results as root-owned and not group/other-writable.
Lock the opened directory in the parent, then clone and attach its mount using
`open_tree(AT_EMPTY_PATH)` before unshare and `move_mount` in the child before
exec. The shell receives only a pinned mount below the root-private runtime
directory; this works for library callers whose executable is not nestegg.
Legacy MS_BIND of `/proc/self/fd/N` fails with EINVAL across mount namespaces
because the fd references the parent mount; the fd-only mount API avoids this.
A later path replacement cannot change the inode.

`nestegg/` is now a root-owned mount anchor; PBS owns its `store/` child. This
is necessary because PBS changes the owner of its datastore root and needs
write access there. Create `store/` with 0755: PBS requires that exact root
mode during reuse (and 0750 for `.chunks`), rather than a stricter 0750 root.
The [official chunk-store checks](https://git.proxmox.com/?p=proxmox-backup.git;a=blob;f=pbs-datastore/src/chunk_store.rs;hb=HEAD)
define these modes; restart/restore acceptance exercises them.
The launcher refuses older prototype anchors owned by PBS;
it does not silently migrate or chmod existing datastore trees. Preserve old
media and migrate explicitly, or recreate disposable phase-one test media.

## Durability, format validation and residual risk (findings 1, 7, 9–11)

Use GNU `sync FILE...` (fsync) on every copied file and directory, then fsync
boot.cfg and its directory before creating commit.sha256; fsync that commit and
its parent directories last. Never substitute `sync -f` (syncfs), which does not
issue the needed vfat file-cache flush. Both committed slots appear in GRUB's
menu; the newest is first. Hash kernel/initrd/squashfs in a slot manifest and
require exact generated descriptors before the privileged helper accepts them.
Older prototype descriptors without payload manifests are refused; no implicit
boot-format or datastore migration is performed.
The helper reports a freshly computed active squashfs hash, not the stick's
unverified `.nestegg-sha256`. Neither checksums nor GRUB authenticate an attacker-
controlled boot medium.

Keep the current shared ESP for this focused phase-one repair. Separate slot
partitions would change the partition format, live-boot device selection and
migration of existing media; that deserves a new format decision and acceptance
suite. A torn shared FAT sector can still damage the active slot or dispatcher.
Manual fallback helps payload/descriptor failures, not damaged shared metadata.
No physical power-loss or controller reliability guarantee is made.

Read the ESP only until format/payload validation and a conservative free-space
check succeed. Include the reclaimable inactive slot and allocation rounding;
remount read-write only after those checks. Find registered devices through the
complete lsblk tree so cloned UUIDs cannot be hidden by a by-uuid symlink.

A separate `tests/stick/durability.sh` uses dm-log-writes and an externally built
upstream replay tool pinned to commit
`7b70d8a6863c5de30933d42a7672d35d01d2dc6c`. Zero the entire inactive squashfs
before recording so old identical release bytes cannot hide a lost new write.
Check actual device flushes for payload, descriptor and commit, with a syncfs
negative control. Replay every flush/FUA boundary and both sides of each fsync
batch. Every valid new commit must have its complete payload; the previous slot
must remain intact. This is a block-layer ordering test, not a physical USB
controller failure or torn-sector simulation; that remains the hardware gate.


## Phase-two integration

[0026](0026-desktop-stick-target.md) uses the same hardened launcher without an
image argument. Desktop sessions cannot choose code through state-directory
overrides: only `IMAGE_PATH` and its recorded checksum supply PBS code. The
root-owned pinned `nestegg/` anchor contains `nestegg/store/`; desktop acceptance
fixtures use this format and supply the fixed cache inside a private mount
namespace (or the disposable guest). Desktop sessions also reject cloned UUIDs
using the same complete block-device scan as the media commands.

Registration preserves phase-two stick-only defaults while using the shared
settings lock and owner/mode-preserving writer. The removed general stick byte
writer is not restored: encrypted JSON key copies use the existing atomic file
writer with mode 0600. API startup readiness and pidfd-based namespace shutdown
waits from phase two remain in effect.

## Phase-one verification result (2026-11-11)

- `cargo test`: 493 passed; the two privileged launcher tests, normally ignored,
  also passed through the acceptance driver. All 16 stick unit tests passed as
  root, including ownership, checksum pin precedence and temporary-copy cleanup.
- Clippy with `--all-targets -- -D warnings`, rustfmt, ShellCheck and
  `scripts/privacy-check.sh tree` passed.
- `tests/stick/create.sh`: twelve KVM boots passed, each with SeaBIOS and OVMF
  for created, updated, missing-descriptor, truncated-payload, interrupted-update
  and retried media. Encrypted PBS backup/restore, fresh identity on restart,
  concurrent-handle refusal, unchanged host listeners/mount namespace and
  retained snapshots/layout passed. Full ESP, forged metadata, cloned UUID,
  mounted/held disk and copy/confirmation interruption checks passed.
- `tests/stick/durability.sh`: all 47 flush/FUA/fsync-boundary replay cuts passed;
  six contained a complete new commit. Every published payload was complete
  and the previous slot remained intact. Real vfat device flushes were observed;
  the syncfs negative control did not produce such a flush.
- `tests/stick/identity-change.sh`: the real CLI rejected a changed capacity
  after confirmation, before writing any partition/boot sectors.
- `tests/stick/btrfs-input.sh`: a root-owned file on a real Btrfs loop with an
  anonymous major-0 mount number was rejected through its actual backing loop.
- Owned loops, private mounts, device-mapper maps and test containers were
  removed; the additionally loaded dm-log-writes module was unloaded. No real
  USB stick was available. Physical controller power-loss and torn-sector
  behavior remain untested.
