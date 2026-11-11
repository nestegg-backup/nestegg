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
