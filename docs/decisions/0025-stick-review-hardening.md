# 0025 – Backup-stick review hardening

- Status: implementation in progress
- Date: 2026-11-11
- Refines: [0024](0024-stick-writing-and-launcher.md)

## Disk identity (review finding 2)

Keep the complete `lsblk` identity/content tree from the inspection displayed
before confirmation, including WWN, partition-table UUID, partition sizes and
starts. After acquiring the device lock, inspect again and refuse any difference.
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
parent is removable. A supplied `--sha256` pin takes precedence and does not
read the adjacent checksum. A sidecar alone detects damage, not authenticity:
an attacker able to replace both inputs can supply any privileged code.
Copy into the root-only desktop cache, not `$TMPDIR`. Install signal handling
before opening the owned loop or copying the ISO; interruptible copying unwinds
the temporary directory before loop detachment. SIGKILL cannot run cleanup.

## Launcher provenance and pinned datastore (review findings 3 and 4)

Remove `LauncherOptions.image`; `LocalPbs::start` uses only the public
`IMAGE_PATH` constant `/var/lib/nestegg/stick/pbs.squashfs`. Verify the recorded
SHA-256 of the opened image, reject removable/FAT backing storage, and mount
that same descriptor. All other public launcher methods/options stay stable.
Open the data mount with O_DIRECTORY|O_NOFOLLOW, open `nestegg/` relative to it,
and validate both fstat results as root-owned and not group/other-writable.
Lock and inherit the opened directory, then bind `/proc/self/fd/N` with mount
canonicalization disabled. A later path replacement cannot change the inode.

`nestegg/` is now a root-owned mount anchor; PBS owns its `store/` child. This
is necessary because PBS changes the owner of its datastore root and needs
write access there. The launcher refuses older prototype anchors owned by PBS;
it does not silently migrate or chmod existing datastore trees. Preserve old
media and migrate explicitly, or recreate disposable phase-one test media.
