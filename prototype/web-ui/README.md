# Web interface prototypes

Clickable, static HTML prototypes used to choose the visual direction of the
web interface (see `docs/decisions/0003-product-and-technical-baseline.md`; the UI runs locally on each PC and controls only that PC, other PCs are shown read-only). They are not
production code. Every page contains invented example data only; nothing talks
to a real Proxmox Backup Server.

Open a file directly in a browser, no server needed.

Design brief for all variants: simple, modern, dark; colour only carries
meaning; no template look (KPI tiles, badge clutter, gradients, invented
branding).

| Variant | File | Idea |
|---|---|---|
| A | `index.html` | Household overview that answers "is everything backed up?" in one sentence; a 14-night grid per machine makes shared causes (server did not wake up) visible at a glance. |
| B | `variant-b/index.html` | Single calm column that reads like a short status report; details only on request. |
| C | `variant-c/index.html` | List of machines on the left, selected machine on the right, like a native app. |

Example data: today is Sunday 8 Nov 2026; four machines; the server did not
wake up in the night to 1 Nov; the laptop has been off for nine nights; the
living-room PC failed once and has no copy of its encryption key.

## Pre-assessment (2026-11-08, before the maintainer's rating)

| Variant | Clarity | Look | Distinct | Phone | Notes |
|---|---|---|---|---|---|
| A | 8 | 7 | 8 | 7 | Household answer sentence; the night grid shows shared server failures at a glance. Busier page. |
| B | 5 | 7 | 4 | 8 | Calmest; states some things the overview cannot know (wrong "failed", "was off"). |
| C | 8 | 9 | 7 | 6 | Most polished, app-like; no household summary sentence; changed the example data. |

Chosen by the maintainer: **variant C, unchanged** (see
`docs/decisions/0005-web-ui-design-direction.md`). Only its example data was
corrected.

## Open questions found while prototyping

- The status report is uploaded together with a successful snapshot only, so
  the overview cannot see failures of other PCs until their next success.
  Should failed runs upload a report too?
- Backups refuse to run until the key export is confirmed, so "backups exist
  but the key was never exported" cannot occur in normal operation. The
  realistic state is "backups blocked until key export is confirmed".
