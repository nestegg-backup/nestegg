# 0015 – A backup is "ok" only when it is complete and was read back

- Status: accepted
- Date: 2026-11-09

## Context

Until now a run succeeded when `proxmox-backup-client backup` exited with
0. Two things were hidden behind that:

- A run that left something out ended as a plain success with a warning:
  system mode switched on but `system.pxar` skipped (root file system the
  rescue stick cannot recreate), an `optional` source that was missing or
  failed its mount check, an inventory that could not be written (without
  `layout.json` the rescue stick cannot restore the snapshot). The
  dashboard said "ok" although the backup could not do what the
  configuration promised.
- Nothing was read back. A snapshot that the server did not store
  correctly, or that cannot be decrypted with the configured key, was only
  noticed at restore time.

## Decision

**Incomplete backups.** The run still uploads everything it can. Every
reason why the result does not contain everything the configuration asks
for goes into `Run::incomplete` (English, also in `warnings`); health is
`incomplete` while `last_success.incomplete` is not empty (`failing` and
`overdue` take precedence). Reasons:

- system mode is on, but the full system backup was skipped;
- an `optional` source does not exist, or failed its mount check;
- the inventory failed (in system mode with a note that the rescue stick
  cannot restore the snapshot), or in system mode `layout.json` could not
  be written.

Stay plain warnings: sockets and other skipped entries, notes that could
not be written, an `include_dev` path that does not exist (nothing to back
up), a `noauto` fstab entry that is not mounted, and separate file systems
below a source that are not part of the configuration (the warning says how
to add them). These describe the configuration or the system, not a backup
that fell short of it.

`nestegg status` prints the reasons under "Incomplete:", `nestegg history`
marks such runs `part` and lists the reasons. The snapshot notes report
`incomplete` and `verified`, and their first line says `incomplete` instead
of `ok`.

**Metadata blob.** Before the upload the run writes a small JSON file into
the root-only runtime directory (`/run/nestegg/tmp/<run id>.meta.json`,
mode 0600) and adds it to the backup as `nestegg-meta.conf:<file>`; the
server stores it as `nestegg-meta.conf.blob` in every snapshot. Content:
format version, nestegg version, backup id, hostname, backup time, the
snapshot path, `archive_sources` (archive file name → canonical source
path, only archives of this snapshot), configured `sources`, system mode
yes/no and the `incomplete` reasons. With encryption on, it is encrypted
like the data archives. Decision 0016 uses it for permissions.

**Read-back.** After a successful upload, before the run counts as
succeeded:

1. `snapshot files`: every uploaded archive (and the blob) must be listed,
   and with encryption on every one must be stored with crypt mode
   `encrypt`;
2. the metadata blob is restored with the configured key into a private
   temporary directory and compared byte for byte with the uploaded file;
3. if `inventory.pxar` is part of the snapshot, it is restored there too
   and compared file by file with `/var/lib/nestegg/inventory` (the
   `.pxarexclude-cli` file the client adds to every archive is ignored).

Data archives are not restored: that would double the run time and the
server's own verification covers their chunks. Any mismatch or failure ends
the run as `failed` with kind `verification_failed` and a message that names
what did not match. A server that becomes unreachable during the check is a
failed verification too, not `pbs_unreachable`: the snapshot may exist. On
success `Run::verified` is set. The snapshot path is recorded in both cases.

Cancellation is checked between the steps. A cancel that arrives after the
upload but before the check is complete ends the run as `cancelled` with a
message that the snapshot was uploaded but not verified; once verified, the
run completes.

**Server-side verification.** At the end of every run that reached the
server (verified or not), the run lists this machine's snapshots and stores
those whose verification state on the server is `failed` in
`State::server_verify_failed` (empty when none). `nestegg status` prints
them as "Server check: FAILED".

**Who cancelled.** Any member of group `nestegg` may still cancel a run.
The daemon logs `backup run <id> cancelled by <user> (uid <n>)` and writes
the name into a root-only note (`/run/nestegg/tmp/cancel-<id>`) before it
signals the run; the run appends "cancel requested by <user> (uid <n>)" to
its cancel message, so it is in the run record and the run log.

## Consequences

- The dashboard says "ok" only for a complete, read-back backup.
  Installations with system mode on and a root file system the rescue stick
  cannot recreate (containers, overlay roots, ...) show `incomplete` until
  system mode is switched off (`system = false`, or `nestegg init
  --no-system`). The end-to-end tests initialise with `--no-system` for that
  reason and check the system-mode case separately.
- Each run restores two small files (metadata, inventory) and lists the
  snapshot twice; this takes a few seconds and a few hundred KiB.
- A verification failure leaves the snapshot on the server; the run is
  `failed`, `last_success` stays at the previous verified run.
- Old run records have no `verified` field; they read as `false`.
- A user-wide exclude that matches inventory files (e.g. `*.txt`) makes the
  inventory comparison fail on every run. That is intended: the rescue stick
  needs those files.
- A first run that fails verification leaves health at `never` (no success
  yet); later failures show `failing`.

## Update 2026-11-10: the read-back cannot hang, and an unfinished run fails

- **Time limit and cancellation.** The read-back's client calls (`snapshot
  files`, the metadata-blob and inventory restores) now run with a time limit
  that fits their size — a couple of minutes for the listing and the blob,
  five minutes for the inventory, far below the hours an upload may take — and
  honour a cancel request (`crate::cancel`) while the client runs. Without
  this, a server that went to sleep right after the upload left the client
  hanging and held the run lock indefinitely. Hitting the limit ends the run
  as `verification_failed` with a message that names what timed out; a cancel
  during the read-back still ends it as `cancelled` (as above). The time limit
  applies only to the read-back, never to the upload. The limits can be
  overridden for testing with `NESTEGG_READBACK_TIMEOUT_SECS`.
- **An unfinished pipeline is a failure, not a success.** The run pipeline
  always decides the outcome itself (the success path finishes the run, and
  every failure path does too). As a safety net, a pipeline that returns
  without having finished the run is recorded as `failed` with kind
  `internal`, never silently as `succeeded`.

## Update 2026-11-10 (b): every read-back call is bounded, and an unavailable server does not pause the retries

- **No read-back call without a time limit and cancellation, whatever the
  configuration.** With encryption on, the read-back hands the key to the
  client, so the restores pass no second listing. With encryption off, the
  restores resolve the key from a `snapshot files` listing of their own; that
  listing is now bounded and cancellable too (the restore's own time limit is
  reused for it). Otherwise a server that answered the first listings but then
  fell asleep could still hang the read-back on this second listing, whatever
  the time limit on the restore itself.
- **A read-back that could not be completed is a *transient* failure.** When the
  read-back fails because the server stopped answering or a time limit was hit
  (as opposed to a mismatch — something was read back and differs, or an archive
  is missing), the run is still recorded as `failed` with kind
  `verification_failed` (the snapshot may exist, nothing was proven), but it is
  marked `transient`: it is a server-availability problem, not a persistent
  fault. Like `pbs_unreachable` it does **not** count towards the growing pause
  between automatic retries (decision 0021); the next hourly check tries again.
  A mismatch stays a real failure and counts as before. The distinction is made
  from the read-back error: a timeout or a transport/connection error (the
  client's own "unreachable" classification) is transient; a content mismatch
  comes back as a plain verification failure.

## Alternatives rejected

- **Restore and compare every data archive:** as expensive as the backup.
- **Check only `snapshot files`:** proves that indexes exist, not that the
  key can read the snapshot.
- **Warning only for incomplete backups:** that is what was there before;
  warnings are not visible enough to replace a red or yellow dashboard.
- **Treat an unreachable server during the check as `pbs_unreachable`:**
  that state means "nothing happened, try later", but the snapshot may be
  there.
- **A separate run state `verifying`:** changes the API enum for every
  client; the check runs in phase `finishing` and logs its steps.
