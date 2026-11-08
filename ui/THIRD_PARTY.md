# Third-party components of the web UI

## Shipped in `dist/` (embedded into the nestegg binary)

| Component | Version | Licence | Notes |
|---|---|---|---|
| [Svelte](https://svelte.dev) runtime | 5.57.3 | MIT | Compiled into `assets/index-*.js`. |
| [Instrument Sans](https://github.com/Instrument/instrument-sans) via `@fontsource/instrument-sans` | 5.3.0 | OFL-1.1 | Latin subset, weights 400/500/600, `woff2` only. Served from the same origin; no font service is contacted. |

The build copies both licence texts to `dist/licenses/`.

## Build and test tools (not shipped)

Vite, `@sveltejs/vite-plugin-svelte`, Vitest, svelte-check, TypeScript and
their dependencies. `npm run licenses` lists every installed package and
fails on a licence that is not AGPL-3.0-compatible. Result at the time of
writing: 64 packages – 54 MIT, 5 Apache-2.0, 2 MPL-2.0 (lightningcss, used by
Vite at build time), 1 OFL-1.1, 1 ISC, 1 BSD-3-Clause.
