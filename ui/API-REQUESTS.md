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
- **Contract and daemon disagree on cancelling.** `api/openapi.yaml` lists
  `409 cannot_cancel` ("still starting or already finishing") for
  `POST /runs/{id}/cancel`. The daemon (`cancel_run` in
  `crates/nestegg/src/daemon.rs`) never sends it: whenever it has nothing to
  signal (run over, no `run.pid` yet, other run id) it answers
  `404 not_found` "no running backup with this id". The UI and the mock follow
  the daemon: 404 and 409 both end in the same text ("no longer running, or
  only just starting or already finishing; try again in a few seconds").
  Either remove the 409 from the contract or let the daemon send it.
- **`POST /pbs/check` says "ok" for a server that is off.** In
  `check::run` the item "PBS reachable" is `ok` when the server answers *or*
  Wake-on-LAN is configured (detail "not now (Wake-on-LAN configured; a run
  will wake it)"). With Wake-on-LAN the whole check therefore reads
  "everything is fine" although the login was never tried. The UI shows the
  detail text of every item, but it cannot tell the two cases apart without
  parsing English text. A separate machine-readable field (for example
  `reachable: false` next to `checks`) would let the page say "the server is
  off" in plain words.
- **Who may cancel:** `POST /runs/{id}/cancel` does not mention permissions.
  The UI offers the button to every user of the page and shows the daemon's
  message for any other error.

## 7. Integration notes for the core side (not API)

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
  `wake=true` only when Wake-on-LAN is switched on) and `GET /pbs` (datastore
  usage on the server view).
- `POST /runs/{id}/cancel` ("Cancel backup" with a confirmation; the daemon
  answers `404` for every case it cannot cancel, the contract's `409
  cannot_cancel` is handled like it).
- Run progress: `expected_bytes` and `processed_bytes` (a bar under "Upload
  changed files").
- `interval_hours` in `Status.schedule` and `ConfigView`; the UI no longer
  parses the text `schedule` unless a daemon sends neither number.
- New states: `health = incomplete` with `Run.incomplete`, `Run.verified`,
  `RunError.kind = verification_failed` and `cancelled`,
  `Status.server_verify_failed`, `Snapshot.verified = failed`.
- Settings: `PATCH /config` with `editable` and `limits` (decision 0013).
- `status.pbs.credentials_stored` and `status.encryption.key_present` (the
  setup guide uses them to show the steps after `nestegg init`; null or absent
  means unknown and never counts as missing).

Not used by the UI: `MachinesResponse.complete` (the list simply shows who is
visible), `PbsInfo.overview_configured` and `SessionInfo.uid` / `root`.
