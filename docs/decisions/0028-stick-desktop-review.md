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

## UI contract

The public OpenAPI contract adds independent live `plugged_in` presence,
cached `available_bytes`/`total_bytes` with `space_checked_at`, registered
`stick_data_uuids`, per-UUID `password_reminders`, and the stick's own overdue
limits. Password reminders distinguish missing key-copy proof from the
periodic password check. The original boolean and all legacy status fields
remain. The UI prefers these fields and retains fallbacks for older daemons;
the existing capacity line gains a percentage without changing the layout.

## Acceptance result (2026-11-11)

- `cargo test`: 523 passed; three privileged tests are opt-in. The incoming
  mount propagation test was also run explicitly as root and passed.
- Clippy over all targets with warnings denied, format check, release build,
  privacy tree scan: passed.
- `tests/stick/desktop.sh`: passed on an owned loop/device-mapper medium,
  including mounted/busy/cloned-UUID errors, actual safe mount options,
  foreign snapshot corruption without retention starvation, new-snapshot
  corruption, GC-only reclamation of actual failed-upload orphan chunks,
  full media, key mismatch, injected I/O failure, offline repair and recovery.
- `tests/stick/hotplug-vm.sh` through `desktop-vm.sh`: passed with real udev,
  the shipped timer sandbox, live presence/capacity/config fields, and a
  device-bound upload stopped by virtual SCSI removal. The run ended as
  `stick_not_plugged_in`; a deliberately stubborn child exercised systemd's
  SIGTERM-to-SIGKILL escalation. No PBS process, current run or runtime leaked.
- End-to-end, packaged systemd and upgrade suites: 193, 155 and 97 checks
  passed. The upgrade used the actual `290c429` baseline. Its pre-existing
  snapshot metadata is preserved; the harness also supports older baselines
  without that metadata instead of assuming that all old packages lack it.
- `npm ci`, UI build, Svelte/TypeScript check and all 347 UI tests passed.
  Desktop/mobile browser inspection confirmed the percentage, distinct key
  copy/check notices, full-but-absent media and monthly damage wording, with
  no document overflow. A monthly damage notice never calls a verified new
  backup failed.

The local PBS proxy must answer its ping before manager/debug commands are
issued; merely spawning it raced the listening socket during acceptance.
Physical USB hardware remains the separate phase-six gate from decision 0023.
