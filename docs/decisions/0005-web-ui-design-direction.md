# 0005 – Web UI design direction

- Status: accepted
- Date: 2026-11-08

## Context

Decision 0003 makes the graphical interface a local web UI (`nestegg ui`)
that controls only the PC it runs on; other PCs of the household are shown
read-only from what they reported to the PBS. Three clickable prototypes were
built to choose the visual direction (`prototype/web-ui/`). Brief for all of
them: simple, modern, plain dark design, colour only where it carries meaning,
no template look.

| Variant | Idea |
|---|---|
| A | Household answer sentence on top, 14-night grid for all PCs. |
| B | One calm column that reads like a short status report. |
| C | List of PCs on the left, selected PC on the right, like a native app. |

## Decision

Variant C (`prototype/web-ui/variant-c/index.html`) is the design direction,
taken as it is. Only its example data was corrected to match the shared
example scenario.

Key elements of C that the real UI keeps:

- Left list: "Dieser PC" on top, other PCs below (problems first), backup
  server and settings at the bottom. On a phone, list and detail are two
  screens.
- Detail: one status sentence in plain German, one primary action, a strip of
  the last 14 nights, run history with expandable log and a plain-language
  explanation.
- Only this PC offers "Jetzt sichern" and "Dateien wiederherstellen"; restore
  always goes into a new folder.
- Dark neutral greys (`#1b1c1f`, `#232428`, `#2e3035`), one blue accent
  (`#7fa8f5`) for the primary action, selection and focus, status colours only
  on problems, font Instrument Sans.

Additions for the real UI (same date):

- Language: German and English from the start, chosen by the browser
  language (German for `de*`, English otherwise); strings live in one file
  per language.
- Theme: dark (as in C) is the default and the reference; a light variant
  derived from the same tokens is shown when the system prefers light.
- The UI is built in parallel to the backend on the `ui` branch in its own
  worktree, against a mock of `api/openapi.yaml`, as agreed in `ui/README.md`.

## Consequences

- `ui/` is built from variant C; A and B stay in `prototype/web-ui/` as
  reference until the real UI exists.
- Open questions found while prototyping are listed in
  `prototype/web-ui/README.md` (failed runs of other PCs are invisible until
  their next success; "no key copy" blocks backups instead of being a
  warning).
