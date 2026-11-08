# Variant C – list and detail

## Idea

A quiet list-and-detail layout, like a well-made native app. The web UI runs
on one PC and controls only that PC (decision 0003, `api/openapi.yaml`):

- Left: "Dieser PC" (Arbeitsrechner) on top, then the other PCs of the
  household, problems first, each with one short status line. A note says they
  are read-only and managed on the PC itself. Backup server and settings sit
  at the bottom of the list.
- Right: the selected PC. One status sentence, one explanatory paragraph, at
  most one primary action ("Jetzt sichern" only on this PC), a 14-night strip
  and the run history. Problem runs (failed, server unreachable) expand to
  the log tail and a plain-language explanation.
- Other PCs: the same layout, read-only, with what the server reports (last
  backup, problem, key copy yes/no, "read from the server today at 07:20").
- Restore and settings are focused views without the list. Restore has three
  steps (date, files with tree and search, new target folder plus summary)
  and never overwrites. Settings are a read-only view of the effective
  configuration with "Verbindung prüfen"; changes are made with
  `sudo nestegg init` or `/etc/nestegg/config.toml`.
- Phone (≤ 760 px): the list is the first screen, the detail a second one with
  a "Alle PCs" back link.

Colour carries meaning only: normal nights and runs are neutral grey; colour
appears for failed (red), server not reachable (violet, deliberately not red:
not the PC's fault), overdue / missing key copy (amber).

## Palette

| Role | Hex |
|---|---|
| Page / detail surface | `#1b1c1f` |
| List pane, quiet blocks | `#232428` |
| Selection, fields, command boxes | `#2e3035` |
| Text | `#ebebed` |
| Secondary text | `#a9abb1` |
| Tertiary text | `#8e9198` |
| Neutral marks | `#6d7078` |
| Accent (primary action, selection, focus) | `#7fa8f5` |
| Status ok / warn / failed / server unreachable | `#86c49b` / `#e2b56c` / `#ef8f84` / `#c7a2e6` |

All text colours reach at least 4.5:1 on all three surfaces.

## Type

Instrument Sans (Google Fonts), weights 400/500/600, tabular figures
everywhere. Four sizes: 13, 15, 19, 28 px. Monospace (system) only for paths,
commands, the folder name and the run log.

## Left out

- Household overview grid (variant A already does it) and any dashboard tiles.
- Health states `never` and `unconfigured` are supported by the API but not in
  the example data, so they have no screen here. `running` is shown during
  "Jetzt sichern".
- A run of "Jetzt sichern" that ends with "Server nicht erreichbar" is not
  simulated; that state is shown in the history (night to 1 Nov) with its log.
- Light mode, notifications, tray, editing configuration.

## Known weaknesses / open points

- Read-only screens (other PCs, server) have no primary action at all.
- Showing *failed* runs of other PCs assumes nestegg uploads a status report
  for failed runs too; the architecture currently uploads one only with a
  snapshot.
- The example data has a PC without a confirmed key copy, although
  `docs/configuration.md` says backups refuse to run until the key export is
  confirmed. Kept because the brief asks for it.
- "Weiteren PC hinzufügen" uses `<EINLADUNGSCODE>`; no invitation mechanism
  exists in the architecture yet.
- The 14-night strip uses bar height for "nachgeholt" (short bar); it needs
  the legend to be understood.
- The selected list row uses a thin accent bar at its left edge; it marks
  selection, not an alert, but is the one place that leans on a border.
