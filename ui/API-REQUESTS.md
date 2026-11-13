# API requests from the UI

What the UI still needs from `api/openapi.yaml` and the daemon, written by the
UI side. The core side decides and updates the contract. Every item says what
the UI does today without it. Checked against `api/openapi.yaml` (version
0.6.0); items that were requested earlier and are in the contract by now are
listed at the end.

## 1. Search inside a snapshot

The restore flow has no search field: finding a file means opening folders one
by one. Filtering only the folders that were opened so far would mislead (it
would look like "not found" for files that exist), so the UI offers no search.

Proposal:

```
GET /snapshots/{snapshotTime}/search?archive=home.pxar&q=report&limit=100
```

- `archive`: one archive of the snapshot (as in `browse`), required.
- `q`: case-insensitive substring of the file or folder name (not a glob, not
  a regular expression), at least 2 characters; 400 `bad_request` otherwise.
- `limit`: 1 to 500, default 100.
- Answer `200`: `{ entries: [{ path, name, type, size_bytes, modified }], truncated: bool }`.
  `path` is the full path inside the archive, in the same form `browse` and
  `RestoreRequest.paths` use (`/alice/Documents/report.odt`); the other fields
  are those of `DirEntry`, so a hit can be ticked and restored like any entry.
- Same permission rule as `browse` (non-root callers only see their home
  directory; paths outside it never appear), `403` and `503 pbs_unreachable`
  as for `browse`, and the same catalog cache (the first call for a snapshot
  may be slow).

## 2. Failed runs of the other computers

`GET /machines` takes the status report from the notes of the last successful
snapshot. A computer whose backups fail for days therefore shows only its
last success; the overview cannot say "failing" for it, only "overdue" once
the threshold has passed.

Proposal: let a computer also upload a small report when a run fails (after
it reached the server), and return the newest one as `report` with
`result: failed` and `error: { kind, message }`. The UI already understands
those fields (`MachineReport` in `src/lib/types.ts`) and shows a failing
computer first in the list.

## 3. Reasons and check names that can be translated

`Run.incomplete` entries and the `name` and `detail` of `POST /pbs/check` are
English sentences. The UI shows them as they are and says that they are
English; a German reader sees English text in the middle of a German page.

Proposal: add a stable `code` next to each text, e.g.
`incomplete: [{ code: "system_backup_skipped", detail: "<English text>" }]`
(or a parallel `incomplete_codes` list, if the list of strings must stay) and
`checks: [{ id: "pbs_reachable", name, ok, detail }]`. The UI would translate
the codes it knows and fall back to the English text for the others.

## 4. Whether the backup client is installed

Before setup, the guide has to say "install `proxmox-backup-client`" without
knowing whether that was done (`/status` answers `unconfigured`, and `nestegg
check` only runs in the terminal). Proposal: `Status.client: { installed:
bool, version: string | null }`, also while unconfigured. The guide could then
mark that part as done.

## 5. Retention on the server view

The server view says "set on the server" for the retention rules. If the Audit
role can read the prune and GC schedule of the datastore, add them to `GET
/pbs` (`keep_last`, `keep_daily`, ... as numbers or null); otherwise nothing
is needed.

## 6. Small points

- **`Run.verified = false`:** the contract says "read back and matched" and
  "absent in older records". Please say what `false` means (verification
  skipped or failed?). The UI shows only `true` and treats `false` and absent
  alike.
- **Cancelling:** after `202` the run has no state that says "cancelling"; the
  UI waits for `cancelled` (or `succeeded`) in `/status`. A `cancelling` flag on
  `Run` would let a second tab show the same thing. Optional.
- **Cancelling (settled):** the daemon follows the contract. `404 not_found`
  for a run id that does not exist, `409 cannot_cancel` for a run that exists
  but cannot be cancelled (still starting, or already over; the message says
  which). The UI shows its own text for 409 and keeps the broad text for a 404,
  because a daemon from before this change answered 404 for every case.
- **`POST /pbs/check` and checks that were not made (settled):** every item
  has `status` = `passed`, `failed` or `not_checked` (`ok` stays: `status ==
  passed`, so `false` for a check that was not made). A server that does not
  answer while Wake-on-LAN is on and no wake was requested is `not_checked`,
  and so is everything that needs the server. The UI therefore checks
  without `wake` first and offers "wake the server and check again". Items
  without `status` (older daemons) are read by `ok`.
- **The result of `POST /pbs/check` in `/status` (settled):** the daemon
  remembers what its last connection check found out (reachable = the item
  "PBS reachable" passed; failed and not checked both count as "did not
  answer") and `/status` reports it in `pbs.reachable`/`checked_at` while it
  is newer than what the last backup run recorded. It is kept in memory, so
  after a restart of the daemon the run's value is the newest again. The UI
  additionally keeps the result of its own check until `/status` reports a
  later `checked_at`, which also covers older daemons.
- **`unknown` run values (settled):** the contract has `unknown` for a run
  `state`, `trigger` and `error.kind` written by a newer version. The UI
  treats any word it does not know like `unknown` and says so ("not
  understood by this version of nestegg, update nestegg") in the history, the
  14-day strip and the headline. The daemon reports `health = failing` when
  the last run's state is unknown; a daemon that still says `ok` gets the same
  headline from the UI. `MachineReport.result` (other PCs) is read the same way.
- **Who may cancel:** `POST /runs/{id}/cancel` does not mention permissions.
  The UI offers the button to every user of the page and shows the daemon's
  message for any other error.

## 7. Stick target fields (implemented)

The daemon and `api/openapi.yaml` now provide all requested fields. The UI uses:

- `TargetStatus.plugged_in` for live presence, independent of a full/damaged
  state or old history. Null means unknown; absent fields retain the legacy fallback.
- `total_bytes` with `available_bytes` for the fill percentage in the existing
  free-space line. `space_checked_at` identifies the cached measurement, also
  refreshed after retention or failure-only GC.
- `ConfigView.stick_data_uuids` for commands even before the first run.
  `TargetStatus.password_reminders` maps each UUID needing attention to
  `key_copy_missing` or `password_check_due`, selecting key-copy or key-check
  and its explanation. The original boolean remains supported.
- `limits.stick_overdue_after_hours` for the stick's own cap, minimum and
  file value. Older daemons retain the original computed fallback.

`state = stick_damaged` reports a failed monthly datastore check separately
from the successful newly verified backup. Legacy GET/POST `/runs` select PBS
when configured, otherwise the stick; explicit target endpoints select both.

## 8. Restoring from the stick (implemented)

API version 2 echoes `target` and the resolved `stick_uuid` in snapshot lists,
browse responses and restore jobs. The UI refuses missing stick echoes and
mismatches, including a successful PBS response from an older daemon. PBS
responses without an echo remain supported for old daemons.

`TargetStatus.plugged_in_uuids` drives the medium selector. The UI sends
`stick_uuid` for list, browse and restore; the daemon pins an implicitly resolved
UUID across validation and job execution. `503 stick_unavailable` carries the
stable `reason` enum, translated by the UI in German and English.

`TargetStatus.sticks` reports each registered UUID's live `plugged_in` and
`in_use` (`backup`, `restore` or null). The aggregate `in_use` also covers
listing, validation and browsing, through shutdown, flush and unmount. The UI
requires explicit idle usage before saying safe to unplug; missing usage on an
older daemon does not count as idle.

Running byte counts for stick restores (implemented, API 0.6.0): the worker
reports `restored_bytes` while it copies, and `total_bytes` when whole archives
are restored, as for the backup server. The UI shows the same line and bar for
both sources: bytes as soon as they arrive, a bar only with a total. An older
daemon reports stick bytes only at completion; then only the elapsed time shows.

## 9. Integration notes for the core side (not API)

- **Websockets through `--dev-proxy`:** the forwarding in `nestegg ui` does
  not pass websocket upgrades on, so Vite's live reload would not work
  through it. The UI config points the HMR websocket straight at the Vite
  port; nothing is needed on the core side unless that should change.
- **Embedded build:** after the first UI build an existing `target/` may need
  `cargo clean -p nestegg` once, because the derive does not notice that
  `ui/dist` appeared. CI builds the UI before the binary.

## Implemented since the first version of this file

All of these are in `api/openapi.yaml` and the UI uses them:

- `GET /machines` (including `complete`) and `GET /session` (`user`, `home`,
  `uid`, `root`).
- Restores: `archives` for several archives in one restore (each into
  `<target>/<archive name>/`), the stable `409 target_exists`, and progress
  (`restored_bytes`, `total_bytes`, `warnings`).
- `POST /pbs/check` ("Check connection" on the server view; the UI passes
  `wake=true` only when the user asks to wake the server after a check found
  it asleep) and `GET /pbs` (datastore usage on the server view).
- `POST /runs/{id}/cancel` ("Cancel backup" with a confirmation; `404` for an
  unknown run, `409 cannot_cancel` for one that is starting or over; a `404`
  from an older daemon is handled like any not-cancellable case).
- Run progress: `expected_bytes` and `processed_bytes` (a bar under "Upload
  changed files").
- `interval_hours` in `Status.schedule` and `ConfigView`; the UI no longer
  parses the text `schedule` unless a daemon sends neither number.
- New states: `health = incomplete` with `Run.incomplete`, `Run.verified`,
  `RunError.kind = verification_failed` and `cancelled`,
  `Status.server_verify_failed`, `Snapshot.verified = failed`.
- Settings: `PATCH /config` with `editable` and `limits` (decision 0013). The
  limits of decision 0022 are in: "overdue after" is capped at 168 hours or the
  rhythm plus 72 hours (`limits.overdue_after_hours` has `base`,
  `per_interval_extra`, `file_minimum`, so the weekly rhythm is possible and the
  page computes the cap for the rhythm it is about to set), and the daemon alone
  decides whether a new exclude pattern would leave out a whole folder; the page
  shows its refusal.
- `status.pbs.credentials_stored` and `status.encryption.key_present` (the
  setup guide uses them to show the steps after `nestegg init`; null or absent
  means unknown and never counts as missing).

- Restoring from the stick: `target=stick` on `GET /snapshots` and
  `/snapshots/{time}/browse` and in the body of `POST /restores`, `503
  stick_unavailable`, `TargetStatus.plugged_in` (the source choice and "not
  plugged in" in the restore flow). Details: section 8.

- Uploaded amount (decision 0030): `Run.transferred_bytes`, the compressed
  amount actually sent or written. The history column "Uploaded"/"Written"
  and the live step text show only this field; `uploaded_bytes` (new data
  before compression) is never labelled as uploaded. Records of older
  versions have no `transferred_bytes` and show no amount.
- Stick names (decision 0030): `TargetStatus.sticks[uuid].name` and
  `ConfigView.stick_names`. The stick choice of the restore flow, the stick
  row of "Backup destinations" (with several sticks or a name) and the key
  reminder show the name, the UUID only as detail; without a name "Stick" and
  the first eight UUID characters. Names are set in the terminal (`nestegg
  stick add --name`, `nestegg stick name`); the UI does not edit them.

Not used by the UI: `MachinesResponse.complete` (the list simply shows who is
visible), `PbsInfo.overview_configured` and `SessionInfo.uid` / `root`.
