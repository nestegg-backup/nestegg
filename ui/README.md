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
  key. The paper key is led through in order: write the file, print it,
  delete it (`rm` of the same path), confirm with the check code; nestegg then
  asks for the statement `yes` (the guide never adds `--printed`, which is for
  scripts), and refuses while the file still lies on this computer. After a
  copy on a USB stick the guide says to unplug the stick. A copy inside a
  folder that is backed up does not count, even on an external disk. It composes the commands from non-secret entries (server, datastore,
  namespace, token ID, path of the USB stick, check code of the paper key)
  and moves on by itself while `/status` is polled. **A command that still
  has a placeholder (`<NAME>`) cannot be copied** (the copy button is
  `aria-disabled` with a short reason next to it) and is written so that a
  shell fails on it.
- **Server view:** "Check connection" (`POST /pbs/check`: the list of checks
  and the time; datastore usage from `GET /pbs` when the daemon knows it). An
  item is passed, failed or **not checked** (`status`; `ok` stays for old
  clients and is `false` for a check that was not made). The first check never
  wakes the server: when it sleeps and Wake-on-LAN is on, "PBS reachable" and
  everything that needs the server (login, datastore access) are "not
  checked", the page says so and offers "wake the server and check again"
  (`?wake=true`, may take minutes: waiting state; a server that still does not
  answer is a failed check). Answers without `status` (older daemons) are read
  by `ok`.
- **Restore:** browse a snapshot, select files/folders in one or several
  areas (several `archives` go into one subfolder each), restore into a new
  directory with progress in bytes (`/snapshots/{time}/browse`,
  `/restores`).
- **Restore from the stick:** with a server and a stick the first step asks
  where the files come from ("Restore from": backup server or backup stick,
  the server is preselected); a stick-only setup goes straight to the stick.
  The stick's requests carry `target=stick` (query for `/snapshots` and
  `/browse`, body for `/restores`); the server's stay as they were. A stick
  that `/status` reports as not plugged in is not opened: the page says so
  and asks `/status` every 5 s until it is back. `503 stick_unavailable`
  (pulled out mid-browse, busy, unreadable) is shown as the stick's problem
  with the daemon's message, never as an unreachable server; the selection
  stays. A running restore from the stick shows no bar or percentage (the
  daemon reports the bytes only at the end) and asks to keep the stick
  plugged in; meanwhile the stick row never says "safe to unplug".
- **Backup stick (decisions 0023, 0026):** with a stick target the page of
  this PC lists the destinations (server and stick, one row each): the
  stick's state from `status.targets.stick` (backing up, not plugged in,
  "safe to unplug" only after a successful stick run, full, file system
  damaged, check failed, other key, not possible on this computer such as
  missing IPv6, overdue after its own threshold), free space as of the last
  run, the last full check, and "Back up to stick now" (`POST
  /targets/stick/runs`) only while a registered stick is plugged in and no
  backup runs. The history and the 14 days switch between the targets
  (`/targets/{target}/runs`); hourly "not plugged in" records of a day are
  folded. The password reminder shows `stick key-check`/`key-copy`. Stick-only
  setups have no server row, no other computers and no token-secret step;
  the key guide offers the
  password-protected copy on the stick. Settings: "overdue after" per target
  and removing one of two targets after a confirmation (decision 0022
  reports both). Missing API pieces: `API-REQUESTS.md`, section 7.
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
| `unknown_run` | the newest run has a state this version does not know (health `failing`, as the daemon reports it), an older success has an unknown trigger, an older failure an unknown error kind |
| `unknown_never` | no backup yet; the one attempt has an unknown state |
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
| `cancel_late` | the backup has just ended when the cancel request arrives: `409 cannot_cancel` (the daemon answers `404 not_found` for an unknown run id and `409` for a run that exists but is over or still starting) |
| `check_slow` | `POST /pbs/check` takes 6 s (it does so anyway with `wake=true`, like a PBS that has to wake up) |
| `check_bad` | the connection check finds a problem (login refused) |
| `check_down` | the backup server is switched off: the connection check answers `200` as the daemon does, never `503`. Without Wake-on-LAN "PBS reachable" is `failed`; with it `not_checked` (nobody asked to wake it) or `failed` after a wake attempt (`wake=true`); login and datastore are `not_checked`. The scenarios `overdue` and `pbs_unreachable` behave the same |
| `no_encryption` | encryption is switched off (no key steps in the guide) |
| `unknown_stored` | `credentials_stored` and `key_present` are `null` (the daemon cannot tell) |
| `no_usage` | `GET /pbs` does not know the datastore usage |
| `long_overdue` | "overdue after" is 744 hours in the file, above the API's cap: `limits` reports 744 as the maximum (and as `file_minimum`) and the settings page can only lower it |
| `stick_pulled` | the stick is pulled out: `plugged_in` is false, stick requests answer `503 stick_unavailable`, and a running restore from the stick fails |
| `stick_busy` | the stick is plugged in but cannot be opened (`503 stick_unavailable`) |

`down` can also be switched while the page is open:
`curl -X POST 'http://127.0.0.1:5173/__mock/fault?down=1'` (and `down=0`); it
applies to the current scenario. The same works for `stick_pulled`, e.g. to
pull the stick out in the middle of choosing files.

Add `&daemon=today` to get what an older daemon offers: no `/machines`,
`/session`, `/pbs` and `/pbs/check` (404), browse and restore answer 501, and
cancelling answers `404` in every case it cannot cancel.

"Back up now" in the mock runs through all phases in about 20 s; in
`pbs_unreachable` and `overdue` it ends with "server not reachable". While
it uploads, "Cancel backup" works (the run ends as `cancelled` after about a
second); in the first seconds it answers `409 cannot_cancel` ("still
starting"), for a run that is over `409` ("already finished"), and `404` for an
id that does not exist, like the daemon (see `API-REQUESTS.md`).
`POST /pbs/check` answers like `check::run` of the daemon (never fails, real
item names, `status` per item) and does not change `/status`. Without a
configuration (`unconfigured`) `/machines` answers `500` like the daemon (the
list of other computers is empty and the sidebar shows no note), and
`503 pbs_unreachable` while the server does not answer (the scenarios
`pbs_unreachable` and `overdue`, the fault `check_down`): the sidebar then says
that other computers cannot be read.
Restoring into `/home/alice/Dokumente` (or another existing folder of the
mock) answers `409 target_exists`. A restore takes about 4 s and reports
`restored_bytes` and `total_bytes` while it runs. From the stick (scenarios
`stick_only` and `both`; `stick_absent` and `both_absent` have it unplugged)
listing takes about 2 s, a restore about 8 s with `restored_bytes` 0 until it
is done and `total_bytes` always null. With `daemon=today` the parameter
`target=stick` is ignored, like a daemon from before stick restores.
`PATCH /config` changes the settings of the current scenario like the daemon
(same validation and the same refusal texts, `src/lib/settings.ts`); later
`/config` and `/status` calls show the new values. Reloading with `?scenario=`
resets them. A value of the wrong type is refused with serde's own text, which
names no field (`invalid settings: invalid type: floating point `1.5`, expected
u32`; `invalid value: integer `-1`, expected u32`); `problemText` turns the
`expected u32` ones into "whole number of hours". Decision 0022:

- "Overdue after" is capped at 168 hours or the rhythm plus 72 hours, whichever
  is larger, for the rhythm that is in effect after the same request (the
  weekly rhythm allows up to 240 hours). A larger value in the file is kept and
  can only be lowered. `limits.overdue_after_hours` has `min`, `max` (the cap
  for the rhythm in the file now), `greater_than_interval`, `base` (168),
  `per_interval_extra` (72) and `file_minimum` (the current value).
- Whether a newly added exclude pattern would leave out a whole folder is
  decided by the daemon alone, with the backup client's own matcher against the
  real accounts. The mock cannot do that: `mock/folderRule.ts` is a **stand-in**
  that refuses a small fixed set of patterns the daemon refuses on a default
  installation, with the daemon's message, and accepts everything else. For the
  home directory `alice` that is `/alice`, `alice`, `**/alice` and `/[a-z]*`.
  Patterns are relative to the root of each archive, so `/home` or `/etc/**`
  match nothing in the archives of the mock's setup and are accepted, as by
  the daemon. A pattern the backup client cannot parse (an unclosed `[`, a
  trailing backslash) is refused with the daemon's message. A pattern already
  in the list is not judged again. The page itself only checks what a browser can know (empty,
  too long in bytes, control and invisible characters, spaces at the ends, a
  leading `-`, a pattern that matches everything, duplicates).

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
| `src/lib/settings.ts`, `src/lib/settingsForm.ts` | What the browser can check of a settings change (ranges, the "overdue after" cap, pattern spelling that does not depend on the machine) and the texts and helpers of the settings page |
| `src/lib/session.ts` | Session token: taken from `#token=`, kept in `sessionStorage`, removed from the address bar |
| `src/lib/api.ts` | API client (relative URLs, `Authorization: Bearer` header) |
| `src/lib/check.ts` | Reading the connection check: passed / failed / not checked per item |
| `src/lib/reveal.ts` | Svelte action that scrolls a message that appears below the fold into view, above the status bar (`scroll-padding-bottom` in `app.css` keeps its height free) |
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
