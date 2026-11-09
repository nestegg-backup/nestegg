# nestegg web UI

The browser interface of nestegg. It is served locally by `nestegg ui`
(loopback only, random port, one-time token) and talks exclusively to the
local API described in [`../api/openapi.yaml`](../api/openapi.yaml).

## Scope

- **Single machine:** dashboard (health, last success, next run, current run
  with live state and a cancel action), "back up now", run history with
  details and log tail, snapshot list, settings (a few editable, decision
  0013).
- **States of a backup:** besides ok, running, overdue, failing and never
  there is "backed up, but incomplete" (`health = incomplete`, attention
  colour, the reasons are listed; also per run in the history and as an
  amber day in the 14-day strip), runs that were read back from the server
  (`verified`, shown once in "About this computer", not per row), the error
  `verification_failed` with its own explanation, cancelled runs, and damaged
  snapshots (`server_verify_failed` and `Snapshot.verified = failed`: a notice
  on the page of this PC, a red ring on the day, and a mark in the restore
  picker, which preselects the newest undamaged snapshot).
- **Setup and key guide:** a step list derived from `/status`, in this
  order: (1) connect (`health = unconfigured`: `nestegg init`), (2) store the
  token secret (`pbs.credentials_stored === false`), (3) create the key
  (encryption on and `encryption.key_present === false`; without that field:
  no fingerprint and no confirmed copy; a key that exists never leads to
  "create"), (4) keep the key safe (`key_exported === false`), (5) switch on
  automatic backups (`schedule.enabled === false`, with a button that does
  what the settings switch does, and the `systemctl` command as the
  alternative; only while the setup is not finished, so that nobody who
  switched the timer off on purpose is nagged), (6) first backup. Unknown
  values (null or absent) never count as missing. Everything happens in a
  terminal as root; the page never asks for a password, token secret or
  key. It composes the commands from non-secret entries (server, datastore,
  namespace, token ID, path of the USB stick, check code of the paper key)
  and moves on by itself while `/status` is polled. **A command that still
  has a placeholder (`<NAME>`) cannot be copied** (the copy button is
  `aria-disabled` with a short reason next to it) and is written so that a
  shell fails on it.
- **Server view:** "Check connection" (`POST /pbs/check`, with Wake-on-LAN it
  may take minutes: waiting state, then the list of checks and the time),
  datastore usage from `GET /pbs` when the daemon knows it.
- **Restore:** browse a snapshot, select files/folders in one or several
  areas (several `archives` go into one subfolder each), restore into a new
  directory with progress in bytes (`/snapshots/{time}/browse`,
  `/restores`).
- **Overview of several machines:** same UI, a list fed by a read-only PBS
  connection (`/machines`).

Read [`../docs/architecture.md`](../docs/architecture.md) and decision
records `docs/decisions/0003-*.md` first. The look and layout follow
decision 0014 (dense app layout: title bar, sidebar with mini strips,
status bar, settings in the pane), which replaces most of 0005.

## Hard requirements

1. **No network requests except to the same origin.** No CDNs, web fonts,
   analytics or external images – the UI must work fully offline and never
   leak that a machine is running nestegg.
2. **Build output in `ui/dist/`** (static files: `index.html` + assets). The
   Rust binary embeds this directory at compile time; there is no Node.js at
   runtime. Keep the bundle small.
3. **Auth:** `nestegg ui` opens `/#token=<token>`. On load, read the token
   from `location.hash`, store it in `sessionStorage`, remove it from the
   address bar (`history.replaceState`) and send it as
   `Authorization: Bearer <token>` with every `api/v1/*` request. No cookies;
   the token never goes to localStorage. A 401 means the session ended: show
   "open the address printed by `nestegg ui`". Relative URLs only
   (`api/v1/status`). The only value in localStorage is the theme choice
   (`nestegg.theme`, decision 0014); every access tolerates blocked storage.
4. **License:** all dependencies must be AGPL-3.0-compatible (MIT, BSD,
   Apache-2.0, ISC, MPL-2.0, GPL/AGPL are fine). Run a license check before
   adding dependencies.
5. **Privacy rules of the repository apply** (see `../AGENTS.md`): example
   data and screenshots use placeholders such as `desktop`,
   `pbs.example.com`, `/home/alice`.
6. **Accessible and responsive:** keyboard usable, readable at phone width
   (the overview will be opened from phones later), light and dark mode
   (System / Hell / Dunkel in the title bar, decision 0014).
7. **Language:** German and English from the start (decision 0005). The
   language follows the browser (`navigator.languages`: German if the first
   entry starts with `de`, English otherwise). All strings live in
   `src/i18n/de.ts` and `src/i18n/en.ts` with identical keys.
8. **No secrets in the browser.** Setup and the key export happen in a
   terminal as root. The page never asks for a password, a token secret or a
   key; the setup guide only asks for values that are not secret (server,
   datastore, namespace, token ID), keeps them in memory and writes them into
   a command to copy.

## Development

Requires Node.js 22 and npm.

```sh
cd ui
npm ci
npm run dev          # mock API; open http://127.0.0.1:5173/#token=dev
npm run check        # svelte-check / TypeScript
npm test             # vitest
npm run test:tz      # the same tests in Pacific/Auckland and America/Los_Angeles
npm run build        # writes dist/
npm run licenses     # licence check of all installed packages
```

### Mock API

`npm run dev` serves `/api/v1/*` from a mock (`mock/world.ts`, a Vite
dev-server middleware; it is not part of the build). It answers the
endpoints of `api/openapi.yaml` with placeholder data only.

Like `nestegg ui`, the mock answers `/api/` only with
`Authorization: Bearer <token>` and returns 401 otherwise. The page takes the
token from the address, so open `http://127.0.0.1:5173/#token=dev` (the dev
server prints this address; set `NESTEGG_MOCK_TOKEN` to use another token).
A tab opened without `#token=` shows "the session has ended", as with the
real daemon.

Pick a scenario with a query parameter in front of the fragment, e.g.
`http://127.0.0.1:5173/?scenario=failing#token=dev`. Loading a page with
`?scenario=` makes it the current scenario of the dev server (for all tabs)
and resets it:

| `?scenario=` | What it shows |
|---|---|
| `ok` (default) | 14 days of backups, one night without server, one day without backup |
| `running` | a backup in progress that finishes after about 15 s |
| `overdue` | last success 3 days ago, the server has not answered since |
| `failing` | last run failed (folder not mounted) |
| `never` | set up, no backup yet |
| `reinstalled` | set up again after a reinstall: no local history (`never`), older backups still on the server |
| `unconfigured` | setup not finished (`/config` answers 500 like the daemon) |
| `pbs_unreachable` | last attempts did not reach the server, still within the threshold |
| `key_not_exported` | backups blocked until the key export is confirmed |
| `credentials_unreadable` | last run failed because the stored secrets no longer decrypt (new mainboard) |
| `incomplete` | `health = incomplete`: the last three backups left out the root file system and an optional folder |
| `verification_failed` | last run failed with `verification_failed` (the upload worked, reading it back did not) |
| `damaged` | the server's own check named two damaged snapshots (`server_verify_failed`, `verified = failed` in the list) |
| `damaged_all` | every snapshot on the server is damaged (the restore picker has nothing better to offer) |
| `no_credentials` | `nestegg init` has run, the token secret is not stored yet (guide step 2) |
| `no_key` | set up, but `nestegg key create` has not run yet (guide step 3) |
| `no_timer` | everything ready, but the timer is off and there is no backup yet (guide step 5) |
| `cancelled` | the last run was cancelled by the user |
| `daemon_down` | every call answers 502, like `nestegg ui` without the daemon |
| `unauthorized` | every call answers 401 |

Add faults to any scenario with `&fault=a,b`:

| Fault | Effect |
|---|---|
| `down` | every call answers 502, like a daemon that stopped after the page loaded |
| `runs` | `/runs` answers 500 |
| `config` | `/config` answers 500 (also `PATCH /config`) |
| `schedule_off` | the timer is disabled (automatic backups off) |
| `no_run_details` | `/status` leaves out `last_run` and `last_success` (allowed by the contract) |
| `no_wake` | no `[wake]` section: Wake-on-LAN cannot be switched on (`PATCH` answers 400) |
| `timer_fails` | switching automatic backups answers 500, nothing changes |
| `unverified` | runs carry no `verified` flag (records of older versions) |
| `cancel_late` | cancelling a running backup answers `404 not_found` although it runs (the daemon answers 404 whenever it cannot cancel; it never sends the contract's `409 cannot_cancel`) |
| `check_slow` | `POST /pbs/check` takes 6 s (it does so anyway with `wake=true`, like a PBS that has to wake up) |
| `check_bad` | the connection check finds a problem (login refused) |
| `check_down` | the backup server is switched off: the connection check answers `200` as the daemon does (item "PBS reachable" `ok: false` / "not now" with Wake-on-LAN, no login item), never `503`; the scenarios `overdue` and `pbs_unreachable` behave the same |
| `no_encryption` | encryption is switched off (no key steps in the guide) |
| `unknown_stored` | `credentials_stored` and `key_present` are `null` (the daemon cannot tell) |
| `no_usage` | `GET /pbs` does not know the datastore usage |

`down` can also be switched while the page is open:
`curl -X POST 'http://127.0.0.1:5173/__mock/fault?down=1'` (and `down=0`); it
applies to the current scenario.

Add `&daemon=today` to get what an older daemon offers: no `/machines`,
`/session`, `/pbs` and `/pbs/check` (404), browse and restore answer 501.

"Back up now" in the mock runs through all phases in about 20 s; in
`pbs_unreachable` and `overdue` it ends with "server not reachable". While
it uploads, "Cancel backup" works; in the first seconds and in the last two
it answers `404` like the daemon (see `API-REQUESTS.md`), otherwise the run
ends as `cancelled` after about a second. `POST /pbs/check` answers like
`check::run` of the daemon (never fails, real item names) and does not
change `/status`.
Restoring into `/home/alice/Dokumente` (or another existing folder of the
mock) answers `409 target_exists`. A restore takes about 4 s and reports
`restored_bytes` and `total_bytes` while it runs.
`PATCH /config` changes the settings of the current scenario like the daemon
(same validation, `src/lib/settings.ts`); later `/config` and `/status` calls
show the new values. Reloading with `?scenario=` resets them.

To use another port, set `NESTEGG_UI_PORT` (not `--port`), so the HMR
websocket uses the same port.

### With the real daemon

The built UI is embedded into the binary by `cargo build` (see
`crates/nestegg/src/ui.rs`). Build the UI first; otherwise the binary serves
a placeholder page. If `target/` existed before `ui/dist/` did, run
`cargo clean -p nestegg` once.

To develop against a running daemon, start the dev server and let
`nestegg ui` forward everything except `/api/` to it:

```sh
npm run dev
nestegg ui --dev-proxy http://127.0.0.1:5173
```

Open the address printed by `nestegg ui` (`http://127.0.0.1:<port>/#token=…`).
The page comes from the dev server, API calls go to the daemon socket with
`nestegg ui`'s token, not to the mock.

### Structure

| Path | Content |
|---|---|
| `src/App.svelte` | Layout (title bar, sidebar, pane, status bar; two screens below 760 px) and hash routing |
| `src/components/` | Title bar, sidebar, status bar and the views: this PC, other PCs, server, settings, restore flow; `SetupGuide` (setup and key steps) and `CancelRun` |
| `public/theme.js` | Applies the stored theme choice before the first paint (same-origin script; the CSP forbids inline scripts) |
| `src/lib/theme.ts` | Theme choice System / Hell / Dunkel, kept in localStorage |
| `src/lib/settings.ts`, `src/lib/settingsForm.ts` | Settings rules (same as the daemon) and the texts and helpers of the settings page |
| `src/lib/session.ts` | Session token: taken from `#token=`, kept in `sessionStorage`, removed from the address bar |
| `src/lib/api.ts` | API client (relative URLs, `Authorization: Bearer` header) |
| `src/lib/store.svelte.ts` | Polling of `/status`; state of the cancel action and of the connection check |
| `src/lib/derive.ts`, `src/lib/text.ts` | State derivation (including the setup guide step and the `nestegg init` command) and status sentences (unit-tested) |
| `src/lib/format.ts` | Date, size and duration formatting |
| `src/i18n/` | German and English strings |
| `src/styles/tokens.css` | Colour tokens, dark and light (by system preference or by choice) |
| `mock/` | Dev-server mock of the API |
| `tests/` | Vitest: derivation, the new states and the guide, formatting, i18n key parity, contrast (WCAG AA, both themes), theme, settings, restore, mock |

## Working agreement between agents

- The UI agent works only inside `ui/` (plus UI-specific CI if needed), on
  its own branch in its own git worktree – never in the main checkout:

  ```sh
  git worktree add ../nestegg-ui -b ui main
  ```

- `api/openapi.yaml` is owned by the core side. Need a change or a new
  field? Write the proposal into `ui/API-REQUESTS.md` on the UI branch; the
  core side updates the contract and the daemon.
- Until the daemon is ready, develop against a mock that returns the
  examples from the OpenAPI contract (e.g. a small dev-server middleware in
  `ui/`). Against the real daemon: `nestegg ui --dev-proxy http://127.0.0.1:5173`
  forwards everything except `/api/` to a Vite-style dev server.
- Integration into `main` happens via merge of the `ui` branch after review.
