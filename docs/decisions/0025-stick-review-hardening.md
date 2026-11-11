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
