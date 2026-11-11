# 0028 – Desktop stick execution and retention hardening

- Status: accepted
- Date: 2026-11-11
- Extends: 0024–0026

## Process and mount safety

The key passphrase helper uses `/usr/bin/python3 -I`, starts in `/`, and
removes inherited `PYTHON*` variables. A root command must not import Python
modules from the caller's working directory or environment.

The desktop run's mount namespace is recursively slave: host automounts and
later source mounts propagate into it, while its stick mounts never propagate
back. The data mount explicitly uses `errors=remount-ro`, overriding an
untrusted ext4 superblock's error policy.

The read-only `e2fsck -f -n` runs through util-linux `setpriv` with no Linux
capabilities and no privilege gains on exec. It still runs as the device's
owner (root) to open root-owned block devices. This is privilege reduction,
not a complete filesystem-parser sandbox. A separate systemd transient unit
would escape the calling test/desktop mount namespace and is not available
in all supported invocation environments; adding another sandbox dependency
is deferred. Filesystem repair remains explicit and offline.

Opening a stick distinguishes mounted media, ambiguous cloned UUIDs, a busy
device lock, missing media, filesystem damage, and internal helper failures.
Only evidence of filesystem corruption is labelled `filesystem_damaged`.

## Validation

Unit tests exercise isolated imports and error classification. A privileged
temporary-tmpfs test proves incoming mount propagation and the absence of
outgoing propagation. Desktop and device-bound VM acceptance cover the real
mount, filesystem check and backup pipeline.

## Presence probes and compatibility

An hourly timer tick records `last_checked_at` before probing for media. An
absent stick does not create a run, rotate history, or alter retry evidence.
Manual absent-stick attempts remain visible. Plug-in events respect the backup
interval but bypass the growing retry pause: reattachment is a deliberate
opportunity to retry.

Legacy GET and POST `/runs` describe PBS when configured, otherwise the sole
stick target. Explicit `/targets/all/runs` remains the way to request both.

## Verification, interruption and recovery of space

The new snapshot is verified independently through the official
`proxmox-backup-debug api create /admin/datastore/stick/verify` interface,
scoped by backup type, ID and time. It authenticates against the ephemeral
local PBS and waits for the worker; the manager CLI cannot select a snapshot.
Inherited `PROXMOX_DEBUG_API_CODE` is removed so the caller cannot switch it
into the direct-code mode that lacks worker initialization. See [the upstream CLI implementation](https://github.com/proxmox/proxmox-backup/blob/master/src/bin/proxmox_backup_debug/api.rs).

Only the monthly pass verifies the entire datastore. Its failure reports
`stick_damaged` and all failed snapshots in `server_verify_failed`, including
other machines. It does not invalidate the verified new snapshot or prevent
this machine's retention and GC. A completed monthly attempt is remembered
separately from the most recent successful full verification, so unrelated
damage does not repeat a complete scan on every backup. Both timestamps are
committed with a successful final run record, after cleanup; interrupted or
failed runs preserve the preceding evidence.

Every stick failure path checks cancellation and physical presence before
classifying a failure. Removal is `stick_not_plugged_in`, a cancellation is
`cancelled`; neither counts towards the retry pause. Queries, group-key checks,
pruning and verification honour cancellation. Failed uploads and `stick_full`
may run GC alone, preserving every snapshot and the PBS atime cutoff.
