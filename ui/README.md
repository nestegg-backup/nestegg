# nestegg web UI

The browser interface of nestegg. It is served locally by `nestegg ui`
(loopback only, random port, one-time token) and talks exclusively to the
local API described in [`../api/openapi.yaml`](../api/openapi.yaml).

## Scope

- **v0.1/v0.2 – single machine:** dashboard (health, last success, next run,
  current run with live state), "back up now", run history with details and
  log tail, snapshot list, settings (a few editable, decision 0013), encryption key
  status with a clear warning while the key has not been exported.
- **v0.2 – restore:** browse a snapshot, select files/folders, restore into a
  new directory (endpoints `/snapshots/{time}/browse`, `/restores`).
- **v0.3 – overview of several machines:** same UI, additional view fed by a
  read-only PBS connection (API to be specified).

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
endpoints of `api/openapi.yaml` plus the proposals in `API-REQUESTS.md`,
with placeholder data only.

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

`down` can also be switched while the page is open:
`curl -X POST 'http://127.0.0.1:5173/__mock/fault?down=1'` (and `down=0`); it
applies to the current scenario.

Add `&daemon=today` to get only what the daemon offers today: no
`/machines` and `/session` (404), browse and restore answer 501.

"Back up now" in the mock runs through all phases in about 20 s; in
`pbs_unreachable` and `overdue` it ends with "server not reachable".
Restoring into `/home/alice/Dokumente` (or another existing folder of the
mock) answers `409 target_exists`.
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
| `src/components/` | Title bar, sidebar, status bar and the views: this PC, other PCs, server, settings, restore flow |
| `public/theme.js` | Applies the stored theme choice before the first paint (same-origin script; the CSP forbids inline scripts) |
| `src/lib/theme.ts` | Theme choice System / Hell / Dunkel, kept in localStorage |
| `src/lib/settings.ts`, `src/lib/settingsForm.ts` | Settings rules (same as the daemon) and the texts and helpers of the settings page |
| `src/lib/session.ts` | Session token: taken from `#token=`, kept in `sessionStorage`, removed from the address bar |
| `src/lib/api.ts` | API client (relative URLs, `Authorization: Bearer` header) |
| `src/lib/derive.ts`, `src/lib/text.ts` | State derivation and status sentences (unit-tested) |
| `src/lib/format.ts` | Date, size and duration formatting |
| `src/i18n/` | German and English strings |
| `src/styles/tokens.css` | Colour tokens, dark and light (by system preference or by choice) |
| `mock/` | Dev-server mock of the API |
| `tests/` | Vitest: derivation, formatting, i18n key parity, contrast (WCAG AA, both themes), theme, settings, mock |

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
