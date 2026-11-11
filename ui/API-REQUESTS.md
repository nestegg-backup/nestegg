# API requests from the UI

What the UI still needs from `api/openapi.yaml` and the daemon, written by the
UI side. The core side decides and updates the contract. Every item says what
the UI does today without it. Checked against `api/openapi.yaml` (version
0.3.0); items that were requested earlier and are in the contract by now are
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

## 7. Backup stick (phase five of decision 0023)

Checked against `api/openapi.yaml` 0.4.0. The UI shows both targets, the
per-target history, "Back up to stick now", the stick settings and target
removal with what the contract has. These pieces are missing:

- **Restore from a plugged-in stick.** `/snapshots`, `/browse` and `/restores`
  have no target. Without a server they answer `503 pbs_unreachable`. The UI
  therefore offers no restore in stick-only setups and says so ("cannot restore
  from the stick yet"); with both targets it restores from the server only.
  Proposal: an optional `target=stick` query on the three endpoints (or
  `/targets/stick/snapshots`), answered while a registered stick is plugged in,
  `409 stick_not_plugged_in` otherwise.
- **Plugged in, explicitly.** `TargetStatus.state` says `stick_not_plugged_in`,
  but `stick_full` hides it, and a failing `attached()` probe (cloned UUIDs)
  shows up as plain health. The UI reads "not `stick_not_plugged_in`" as
  plugged in and shows nothing about it while the state is `stick_full`.
  Proposal: `plugged_in: boolean | null` on the stick's `TargetStatus`.
- **Fill level.** Only `available_bytes` (at the last run) exists, so the UI
  shows free space "as of the last backup", not a percentage. Proposal:
  `total_bytes` next to it, and the time it was measured.
- **The stick's UUID.** `stick key-copy` and `key-check` need `--uuid`. The UI
  takes it from the newest stick run's `stick_uuid`; before any stick run the
  user has to type it (from `sudo nestegg stick info`). Proposal: the registered
  data UUIDs in `GET /config` (not secret), and per UUID whether the password
  proof is missing or only the 180-day re-check is due (`password_reminder`
  is one flag for both, so the notice shows both commands).
- **Stick limits.** `limits` has no entry for `stick_overdue_after_hours`; the
  UI computes the cap from `overdue_after_hours.base`/`per_interval_extra` and
  the stick's current value, like `settings::validate`.

## 8. Integration notes for the core side (not API)

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

Not used by the UI: `MachinesResponse.complete` (the list simply shows who is
visible), `PbsInfo.overview_configured` and `SessionInfo.uid` / `root`.
