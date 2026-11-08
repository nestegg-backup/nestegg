# nestegg web UI

The browser interface of nestegg. It is served locally by `nestegg ui`
(loopback only, random port, one-time token) and talks exclusively to the
local API described in [`../api/openapi.yaml`](../api/openapi.yaml).

## Scope

- **v0.1/v0.2 – single machine:** dashboard (health, last success, next run,
  current run with live state), "back up now", run history with details and
  log tail, snapshot list, configuration view (read-only), encryption key
  status with a clear warning while the key has not been exported.
- **v0.2 – restore:** browse a snapshot, select files/folders, restore into a
  new directory (endpoints `/snapshots/{time}/browse`, `/restores`).
- **v0.3 – overview of several machines:** same UI, additional view fed by a
  read-only PBS connection (API to be specified).

Read [`../docs/architecture.md`](../docs/architecture.md) and decision
records `docs/decisions/0003-*.md` first.

## Hard requirements

1. **No network requests except to the same origin.** No CDNs, web fonts,
   analytics or external images – the UI must work fully offline and never
   leak that a machine is running nestegg.
2. **Build output in `ui/dist/`** (static files: `index.html` + assets). The
   Rust binary embeds this directory at compile time; there is no Node.js at
   runtime. Keep the bundle small.
3. **Auth:** the first page load arrives as `/?token=<token>`; the server sets
   an HttpOnly cookie and the UI must not store the token in localStorage.
   Relative URLs only (`api/v1/status`, not absolute hosts).
4. **License:** all dependencies must be AGPL-3.0-compatible (MIT, BSD,
   Apache-2.0, ISC, MPL-2.0, GPL/AGPL are fine). Run a license check before
   adding dependencies.
5. **Privacy rules of the repository apply** (see `../AGENTS.md`): example
   data and screenshots use placeholders such as `desktop`,
   `pbs.example.com`, `/home/alice`.
6. **Accessible and responsive:** keyboard usable, readable at phone width
   (the overview will be opened from phones later), light and dark mode.
7. **Language:** English UI strings, kept in one place so translations
   (German first) can be added.

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
  `ui/`). `nestegg ui --dev-proxy` support for a Vite-style dev server will
  be added on the core side.
- Integration into `main` happens via merge of the `ui` branch after review.
