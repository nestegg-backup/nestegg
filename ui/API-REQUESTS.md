# API requests from the UI

Proposals for `api/openapi.yaml` and the daemon, written by the UI side.
The core side decides and updates the contract. The UI already handles the
current state gracefully (noted per item). The mock in `mock/world.ts`
implements the proposed shapes, so they can be tried with `npm run dev`.

## 1. `GET /api/v1/machines` – other machines of the household (v0.3)

Read-only list of the backup groups in this PC's PBS namespace, as decided in
0003 (Audit role, snapshots plus the uploaded status report).

```yaml
MachinesResponse:
  type: object
  required: [machines]
  properties:
    read_at: { type: [string, "null"], format: date-time, description: When the PBS was last read. }
    machines:
      type: array
      items:
        type: object
        required: [backup_id]
        properties:
          backup_id: { type: string, examples: ["media"] }
          hostname: { type: [string, "null"], description: From the last status report. }
          os: { type: [string, "null"] }
          last_snapshot: { type: [string, "null"], format: date-time }
          recent_snapshots:
            type: array
            description: Snapshot times of the last 14 days, newest first (for the 14-day strip).
            items: { type: string, format: date-time }
          size_bytes: { type: [integer, "null"], description: Size of the last snapshot. }
          report:
            description: Contents of the last uploaded status report, or null.
            type: [object, "null"]
            required: [reported_at, result]
            properties:
              reported_at: { type: string, format: date-time }
              result: { type: string, enum: [succeeded, failed, pbs_unreachable] }
              run_started_at: { type: [string, "null"], format: date-time }
              error: { $ref: "#/components/schemas/RunError" }
              key_exported: { type: [boolean, "null"] }
              sources: { type: array, items: { type: string, examples: ["/home"] } }
              overdue_after_hours: { type: [integer, "null"] }
              nestegg_version: { type: [string, "null"] }
```

- This PC may be part of the list; the UI filters out the entry whose
  `backup_id` equals `status.machine.backup_id`.
- Errors: `503 pbs_unreachable` like `/snapshots`.
- Open question already noted in the prototype README: failed runs only show
  up if a report is uploaded for them too (today a report is uploaded only
  with a snapshot).
- **Today:** the daemon answers 404; the list then shows only "This
  computer", without an error.

## 2. `GET /api/v1/session` – who is using the UI

```yaml
SessionInfo:
  type: object
  required: [user, home]
  properties:
    user: { type: string, examples: ["alice"] }
    home: { type: string, examples: ["/home/alice"] }
```

The daemon can take this from the peer credentials (`SO_PEERCRED`) of the
socket connection opened by `nestegg ui`. The restore step "new folder" needs
the home directory to offer `/home/alice/<name>` and to say who will own the
restored files.

- **Today:** 404; the UI then asks for a full absolute path instead.

## 3. Restores

- **Several archives in one restore.** `RestoreRequest` has one `archive`, so
  the UI lets the user pick from one area (`/home` or `/etc` …) per restore
  and says so. Proposal: `archives: [{ archive, paths }]`, restored into
  `<target_dir>/<archive name>/` when more than one is given.
- **Stable error code when the target exists:** add `target_exists` to the
  `Error.code` enum and answer `409`. The UI already shows a specific message
  for `409` + `target_exists` (and for a `409` whose message contains
  "exist").
- **Progress:** `Restore` has no progress; add `restored_bytes` and, if
  cheap, `total_bytes`. The UI shows the elapsed time until then.
- **Search inside a snapshot** (the prototype has a search field): e.g.
  `GET /snapshots/{time}/search?archive=&q=` returning paths. Left out of the
  UI until it exists – filtering only the folders opened so far would
  mislead.
- **Today:** browse and `POST /restores` answer 501; the UI says that this
  version cannot restore from the interface yet and shows the matching
  `sudo nestegg restore <snapshot> <archive> <NEW-FOLDER>` command.

## 4. `POST /api/v1/pbs/check` – "Check connection" in settings

The prototype has a "Verbindung prüfen" button. Proposal: runs the
non-root parts of `nestegg check` (reachability, authentication, datastore
access, key present) and returns
`{ checks: [{ name, ok, detail }], checked_at }`. Must not wake the PBS
unless asked (`?wake=true`).

- **Today:** not in the API, so the button is left out. Settings show the
  terminal command `sudo nestegg check` instead.

## 5. Run progress and cancelling

- `Run` during `uploading`: add an estimate (`expected_bytes` from the last
  successful run is enough) so the UI can show a progress bar like the
  prototype. Today the UI shows "x GB uploaded" without a bar.
- `POST /runs/{id}/cancel` (the prototype has "Sicherung abbrechen").
  Left out of the UI until it exists.

## 6. Structured schedule and server facts

- `ConfigView.schedule` and `Status.schedule.calendar` are English free text
  ("every 24 h"). Please add `interval_hours: integer` to both; the UI parses
  `every N h` meanwhile and falls back to the raw text.
- Server view: datastore usage (`used_bytes`, `total_bytes`) and the
  retention settings, if readable with the Audit role. Left out until then.

## 7. Integration notes for the core side (not API)

- **Build order:** the binary embeds `ui/dist` at compile time
  (`rust-embed`, `allow_missing`). A `cargo build` before `npm run build`
  ships the placeholder page; the `.deb` job in `ci.yml` should build the UI
  first (Node 22: `npm ci && npm run build` in `ui/`). After the first UI
  build an existing `target/` may need `cargo clean -p nestegg` once, because
  the derive does not notice that the folder appeared.
- **Websockets through `--dev-proxy`:** the forwarding in `nestegg ui` does
  not pass websocket upgrades on, so Vite's live reload would not work
  through it. The UI config points the HMR websocket straight at the Vite
  port; nothing is needed on the core side unless that should change.
