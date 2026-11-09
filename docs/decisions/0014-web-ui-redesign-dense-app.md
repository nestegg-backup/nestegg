# 0014 – Web UI redesign: dense desktop-app layout

- Status: accepted
- Date: 2026-11-09
- Partly supersedes: [0005](0005-web-ui-design-direction.md) (look and layout)

## Context

The UI built from decision 0005 (variant C) worked, but the product owner
found it boring: large type, a lot of empty space, little information per
screen, and settings that took over the whole page. With editable settings
(decision 0013) the settings page also needed real controls.

Three static prototypes were made with the same placeholder data and the
same brief as before (simple, modern, plain; no template look; colour only
where it means something):

| Variant | Idea |
|---|---|
| 1 | Calm and refined: large type, a 14-day calendar, settings with a save bar. |
| 2 | Native program look: dense, a mini strip per PC in the list, every setting saved at once. |
| 3 | Night band: each night a bar placed by time and duration, settings changed one at a time. |

The owner chose variant 2.

## Decision

The web UI follows variant 2. What changed compared with 0005:

- **Title bar** on top: the current page name on the left (the page's only
  `h1`, with a quiet addition such as "dieser PC, Linux Mint 22.2"), the
  theme choice "System / Hell / Dunkel" on the right. The prototype showed
  "Einstellungen" both in the title bar and as a page heading; the real UI
  names a page only once, in the title bar.
- **Sidebar** with every PC, each with a 14-day **mini strip** next to its
  name and its status line below; "Backup-Server" and "Einstellungen" at the
  bottom. The strip of this PC comes from its run history plus the snapshots
  on the server (the same days as its 14 boxes). Other PCs' strips come from
  `recent_snapshots` of `/machines`: the PBS only knows successful backups,
  so they show "gesichert" or "keine Sicherung" and never a failure. Without
  a current `/machines` answer there is no strip at all.
- **Status bar** at the bottom: whether the backup server was reachable at
  the last check, the nestegg version, and short messages such as
  "Gespeichert um 08:14: Rhythmus täglich" (announced to screen readers).
- **Detail**: status sentence and explanation on the left, actions
  ("Dateien wiederherstellen", "Jetzt sichern") on the right; 14 day boxes
  with weekday labels; the run history as a table (day, start, result,
  duration, uploaded, log); an "Über diesen PC" side column.
- **Settings live in the right-hand pane**, the sidebar stays visible. They
  are grouped cards with switches, a select for the rhythm, a number field
  for "Überfällig melden nach", the exclude list with remove/add, the
  Wake-on-LAN switch, and a read-only block "Server und Verschlüsselung"
  with `sudo nestegg init` to copy. Each control saves on its own
  (`PATCH /config`) and shows "Gespeichert" next to it; a value that is
  refused (checked in the page with the daemon's rules, the daemon stays the
  authority) is not saved and its row is marked with the reason. Only
  fields listed in `editable` get a control. When a longer rhythm would
  leave the overdue threshold at or below it, the page raises the threshold
  in the same request (half a rhythm, at least 12 hours, on top) and says
  so, instead of refusing the choice.
- **Theme choice** System / Hell / Dunkel, kept per browser in
  `localStorage` (the only value the UI stores there) and applied before the
  first paint by a small same-origin script (`ui/public/theme.js`; the CSP
  of `nestegg ui` forbids inline scripts). Both themes are first-class; the
  token pairs are checked for WCAG AA in `ui/tests/contrast.test.ts`.
- **Phone** (up to 760 px): list and detail stay two screens. Every detail
  screen has a clear back control "‹ Alle PCs" in the title bar (accent
  colour, at least 44 px high); the theme is chosen on the list screen.
- The restore flow keeps its three steps and runs in the pane as well, with
  a step indicator and "Abbrechen" next to the step.

What stays from 0005: list of PCs on the left and the selected one on the
right; this PC first, other PCs read-only and sorted problems first; one
status sentence in plain German; only this PC offers backup and restore;
restore always into a new folder; German and English chosen by the browser;
Instrument Sans as the bundled font; blue as the only accent; status colours
only for problems ("alles in Ordnung" keeps a grey dot).

## Consequences

- Deviations from the prototype: the font stays Instrument Sans (the
  prototype used IBM Plex from a font service, which the UI must not
  contact; bundling another font is a separate decision). The fail colour
  of the day boxes and some greys were adjusted to reach WCAG AA.
- More information per screen means smaller type (14 px body); the
  breakpoint at 1100 px moves the side column below the history.
- The prototypes stay outside the repository; screenshots of the chosen
  variant were used for review only.
