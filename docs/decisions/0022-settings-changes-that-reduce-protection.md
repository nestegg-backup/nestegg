# 0022 – Settings changes that reduce the protection

- Status: accepted
- Date: 2026-11-10

## Context

Members of group `nestegg` may change a fixed set of settings through the web
UI (`PATCH /api/v1/config`, [decision 0013](0013-editable-settings-in-the-web-ui.md)):
the backup interval, "overdue after", the exclude list, Wake-on-LAN and the
backup timer. That is convenient, but it also lets a group member — or a
compromised browser session, or a less trusted member of the household — turn
off the protection of this computer's backups quietly:

- Automatic backups off **and** a very long "overdue after" gave weeks without
  any message: the dashboard would say "overdue" eventually, but nobody looks
  at it.
- An exclude such as `/home/alice` or (in system mode) `/etc` was accepted, and
  the run still counted as "complete", so whole home directories or system
  directories could silently drop out of the backup.
- Only the journal recorded who changed what, and nobody reads the journal.

The owner of a family computer should notice when its protection is lowered,
and the most destructive settings should be out of a group member's reach
entirely.

## Decision

Three parts, on top of the existing editable-settings machinery, plus the
delivery, flood-control, locking and validation rules that make them hold up.

### 1. Report every protection-reducing change made through the API

After a successful `PATCH /config` by a group member, the daemon classifies the
change (`settings::reductions`). A change **lowers the protection** when it
switches automatic backups off, adds an exclude pattern, removes a `!`
re-include, reorders the exclude list so a re-include no longer wins, raises
"overdue after", raises the interval, or switches Wake-on-LAN off. Changes that
raise the protection (more frequent backups, a lower threshold, a removed
exclude, an added re-include, Wake-on-LAN on) are not reported.

A notification event, **`settings`**, carries this to the logged-in users and
the hook: who changed what, with plain German and English text like the other
events. Delivery reuses the health check's machinery (`notify.rs`). A change
made by editing the file as **root** is not reported — root is trusted and does
not go through the API; a change made through the API by root is not reported
either.

### 2. "Automatic backups are off" is a condition of its own

A switched-off backup timer, while nestegg is configured, is its own
health-check condition (`automatic_off`), announced at once and repeated every
seven days, **independent of the overdue threshold**. It replaces the old
"overdue while the timer is off" variant; the text still says how old the last
successful backup is. It is evaluated next to the other conditions, so it is
announced even when the last run also failed. A deliberately paused computer is
thus reported calmly (weekly), but from the start, not only after weeks.

### 3. The API refuses the extremes; root keeps them in the file

- **"Overdue after"** is capped through the API at `max(168, interval + 72)`
  hours — seven days, or the interval plus three days when that is larger, so a
  weekly rhythm (interval 168 h) can still be overdue after up to ten days. The
  cap is evaluated against the interval in effect *after the same request*,
  since both keys can change together. A larger value that root set in the file
  is kept and shown, and can then only be *lowered* through the API, never
  raised past what the file already has. `limits.overdue_after_hours` reports
  `base` (168), `per_interval_extra` (72) and `file_minimum` so the web
  interface can recompute the cap for an interval it is about to set.

- **Excludes** are judged as the backup client really applies them. Each
  `--exclude` is matched *inside each archive*, relative to that archive's root
  and with later patterns winning (a `!` re-include can bring a path back). The
  archives that are actually produced decide what is tested:

  - in normal mode each configured source is its own archive (`home.pxar` from
    `/home`, with entries `/alice`, `/bob`, …): a change is refused if its whole
    new list would empty a source or a regular user's home directory inside it
    (`/alice`, `alice/**`, `/[a-z]*`, …);
  - in system mode the operating system is one `system.pxar` rooted at `/`: the
    top-level system directories (`/etc /home /root /usr /var /opt /boot /srv`)
    and the home directories by their full path (`/home/alice`) are tested
    there; a source that keeps its own archive (a data disk that must be its own
    mount) is tested separately.

  "Would empty the folder" means the list excludes the folder entry itself, or
  excludes every child — tested against the folder's **real top-level entry
  names** (read as root, names only, a bounded number) and two synthetic probe
  names that catch a pattern matching every possible child. A change is refused
  only when the **new** list would empty a folder that the **old** list did not,
  so a dangerous pattern root already put in the file is grandfathered and a
  plain reorder is refused only when it changes the outcome.

  A pattern the matcher cannot parse is refused up front with a plain message:
  an unclosed `[`…`]` character class, a trailing `\`, a malformed range
  (`[a-]`). nestegg links an older release (0.1.2) of the client's own
  `pathpatterns` crate (1.0.0); both share an off-by-one bug that *panics* the
  parser on an unclosed class, so the parse is wrapped and any panic is caught
  and turned into a refusal rather than an HTTP 500.

The refusal messages say in plain words why, and that an administrator can set
the value as root in `/etc/nestegg/config.toml`.

### 4. Durable delivery, no flood, honest about failures

- **Write-ahead report.** Before the daemon changes the configuration it records
  a short *intent* in a small inbox (`notify-settings.json`, its own lock). If
  even the intent cannot be recorded the change is not applied. After the change
  the intent is completed with the real reductions (or dropped when nothing was
  lowered). A crash in between still leaves the intent, which the next check
  delivers as a generic "the settings were changed by …; the details are in the
  system log" message. A damaged inbox is set aside under a `.corrupt-…` name,
  logged, and reported as such — never silently discarded. The HTTP answer is
  not "200 OK" unless the report was durably recorded.

- **Delivered within seconds, and again at the end of the run.** After recording
  the daemon starts `nestegg-notify.service` (`systemctl start --no-block`). The
  check drains the inbox under its main lock and announces it. Before it
  releases the lock it drains the inbox *again* (bounded), so a change recorded
  while the check was running — its hook can be slow — is delivered by that same
  run instead of waiting for the hourly timer.

- **No flood.** A check started for a settings change waits out a short quiet
  period before it takes the main lock (until the newest pending change is about
  20 s old, at most ~2 minutes from the oldest), so a burst coalesces into one
  run; systemd collapses the repeated `start --no-block` triggers onto that one
  sleeping instance (`StartLimitIntervalSec=0`, kept so a burst cannot trip the
  default start-rate limit and drop reports). Everything pending is combined
  into one message per recipient. After the first message a recipient is told at
  most once every few minutes — later changes are combined and delivered once
  that time is over, and nothing is dropped. The record is combined per user
  rather than truncated when it would grow large, so one member's flood can
  never push another member's unreported change out.

- **Delivery is independent of the problem conditions**, so a `settings` change
  is announced even while a backup is in progress. The record is saved after
  each delivery, so a check killed halfway does not resend what it already sent;
  every message is held under a few kilobytes and the hook's stdin write cannot
  block the check, so a huge exclude list can neither make delivery fail nor
  hang it.

### 5. Locking and file permissions

- Every lock file that only root processes need (`.notify.lock`,
  `.notify-settings.lock`, `.current.lock`, and the run lock `run.lock`) is
  created `0600` and tightened to `0600` when it is opened, so a group member
  cannot open it read-only and hold a shared `flock` — which would otherwise
  block the daemon's recording path or keep backups from starting. `nestegg
  status` still works for a group member while a backup runs, because it reads
  `/proc/locks` rather than the lock file. The daemon's wait for the inbox lock
  in the request path is time-bounded, so a stuck lock cannot tie up a worker
  thread forever.
- `postinst` only **tightens** a `config.toml` that is too open (readable,
  writable or executable by others, or writable by the group) or wrongly owned;
  it sets the owner to root, and the mode only after the owner change worked. A
  stricter `root:root 0600` an administrator chose is kept; a symlink is never
  followed; and when the `nestegg` group is missing the file falls back to
  root-only `0600` rather than staying world-readable.
- `nestegg init` writes `config.toml` the way the settings writer does: a new
  file created with the final mode `0640` from the start (no window at a looser
  mode, also on `--force`), synced and renamed over any old file, refusing a
  symlink rather than following and truncating it.

## Consequences

- Turning off the protection of this computer's backups is visible: the owner
  (and anyone they told the hook to notify) hears about it, named, within
  seconds, and durably — a crash or a slow hook cannot lose the report.
- The `settings` announcement and the `automatic_off` condition are two more
  message kinds the hook may see (`NESTEGG_EVENT=settings`, `automatic_off`);
  existing hooks ignore unknown events. The detail can contain paths, so it
  follows the two detail levels of [decision 0017](0017-notifications.md):
  members of group `nestegg` and root see it in full, everyone else only learns
  that the settings were changed.
- `tests/e2e/systemd.sh` switches automatic backups off through the API and
  checks that the hook gets the `settings` event with the user's name without
  waiting for the hourly timer, that the `automatic_off` condition follows, that
  an exclude of a whole source is refused with the plain message, and that a
  group member cannot open the daemon's lock files.

### What this does and does not guarantee (the limits, plainly)

- **The exclude guard is a guard against accidents and casual misuse, not a
  security boundary.** A group member can still narrow the backup in ways that
  are not refused — excluding a user's folders one by one, for example — but
  every such change is reported, named, to the owner and the hook. The refusal
  only fences off the one-move "drop a whole home, source or system directory"
  cases.
- **Pattern matching is best-effort against an older library.** nestegg uses
  `pathpatterns` 0.1.2; the backup client links 1.0.0 — the same code, an older
  release. The two behave the same on the cases that matter here (confirmed with
  `pxar create --exclude` in the e2e client image), but nestegg does not promise
  byte-for-byte parity; it promises the same syntax and that a pattern it refuses
  is one the client would also choke on.
- **root is unbounded in the file.** Every limit above binds only changes made
  through the API by a group member. root can set anything in
  `/etc/nestegg/config.toml`, which is the intended trust boundary.

## Alternatives rejected

- **Only report, do not limit.** Reporting alone leaves the extreme
  combinations possible: a determined or careless group member could still set a
  weeks-long overdue window or drop a whole home directory, and the owner would
  get one message and then silence. The limits keep the worst cases out of
  reach; the report covers everything else.
- **Settings for administrators only (no group editing).** Taking the editable
  settings away from group members would undo the point of
  [decision 0013](0013-editable-settings-in-the-web-ui.md) — everyday changes
  without `sudo`. Most changes are harmless and useful; only the extremes need
  to be fenced off, and the rest is made visible rather than forbidden.

## History

- **First implementation.** Reporting, the `automatic_off` condition, the
  overdue cap and an exclude rule that matched patterns *syntactically*.
- **First review.** The syntactic rule was both too weak and too strong
  (accepting `alice`, `/[a-z]*` while refusing harmless `/.cache`); it was
  replaced by matching with the client's own `pathpatterns` matcher, the
  pattern-length limit moved to bytes, the overdue cap gained the
  `interval + 72` allowance for a weekly rhythm, and the change was recorded in a
  durable inbox before the HTTP answer.
- **Second review (this revision).** The matcher is now applied **archive-
  relative over the whole new list** (so anchored source paths like `/home` are
  no longer refused in normal mode where they match nothing, and a home
  directory is tested in the archive that really contains it, including its real
  top-level entries); malformed patterns that *panic* the matcher are caught and
  refused instead of returning HTTP 500; the inbox is re-drained at the end of a
  run and paced by a quiet period plus a per-recipient rate limit; the record is
  written ahead as an intent and a damaged inbox is reported; all root-only lock
  files are `0600`; `postinst` also tightens a group-writable or wrongly owned
  file and falls back to `0600` without the group; and `nestegg init` writes the
  file atomically with its final mode and refuses a symlink.
