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

- Automatic backups off **and** "overdue after" at the old maximum of 744
  hours gave a month without any message: the dashboard would say "overdue"
  eventually, but nobody looks at it, and the health check
  ([decision 0017](0017-notifications.md)) only announced the switched-off
  timer once the overdue threshold had passed.
- An exclude such as `/home` or `/etc` was accepted, and the run still counted
  as "complete", so whole home directories or `/etc` could silently drop out
  of the backup.
- Only the journal recorded who changed what, and nobody reads the journal.

The owner of a family computer should notice when its protection is lowered,
and the most destructive settings should be out of a group member's reach
entirely.

## Decision

Three parts, on top of the existing editable-settings machinery.

### 1. Report every protection-reducing change made through the API

After a successful `PATCH /config` by a group member, the daemon classifies
the change (`settings::reductions`). A change **lowers the protection** when it
switches automatic backups off, adds an exclude pattern, raises "overdue
after", raises the interval, or switches Wake-on-LAN off. Changes that raise
the protection (more frequent backups, a lower threshold, a removed exclude, a
re-include `!` pattern, Wake-on-LAN on) are not reported.

A new notification event, **`settings`**, carries this to the logged-in users
and the hook: who changed what, with the old and new value, in plain German
and English like the other texts. Delivery reuses the health check's machinery
(`notify.rs`): the daemon records the change in `notify.json` under the
notification lock (`notify::record_settings_change`) and starts
`nestegg-notify.service` at once (`systemctl start --no-block`), so the change
is announced within seconds instead of at the next hourly timer. The record
keeps per-user bookkeeping, so a user who logs in later still gets it once, and
retries the hook when it failed; an announcement is dropped after three days.

A change made by editing the file as **root** is not reported — root is
trusted and does not go through the API. A change made through the API by root
is not reported either (same reason).

### 2. "Automatic backups are off" is a condition of its own

A switched-off backup timer, while nestegg is configured, is now its own
health-check condition (`automatic_off`), announced at once and repeated every
seven days, **independent of the overdue threshold**. It replaces the old
"overdue while the timer is off" variant; the text still says how old the last
successful backup is. A deliberately paused computer is thus reported calmly
(weekly), but it is reported from the start, not only after a month.

### 3. The API refuses the extremes; root keeps them in the file

- **"Overdue after"** is capped at **168 hours (7 days)** through the API. A
  larger value that root set in the file is kept and shown, and can then only
  be *lowered* through the API, never raised past what the file already has
  (`limits.overdue_after_hours.max` reports the effective maximum). So a group
  member can no longer combine "automatic backups off" with a month of
  silence.
- **Excludes** that would drop a whole configured source or a whole top-level
  directory of an archive are refused **when newly added**: a pattern whose
  fixed part is a source path or a parent of one, alone or followed only by
  wildcards (`/home`, `/home/`, `/home/*`, `/home/**`, `/usr/local`, …); any
  anchored pattern with a single path component (`/alice`, `/etc`, `/var` — a
  whole top-level entry of some archive); and a bare name equal to the last
  component of a source path (`home`, `etc`). Ordinary patterns stay allowed
  (`**/.cache`, `**/node_modules`, `/alice/Downloads`, `/home/*/Videos`). A
  dangerous pattern that root already put in the file is grandfathered, so a
  group member can still edit the rest of the list.

The refusal messages say in plain words why, and that an administrator can set
the value as root in `/etc/nestegg/config.toml`.

## Consequences

- Turning off the protection of this computer's backups is visible: the owner
  (and anyone they told the hook to notify) hears about it, named, within
  seconds.
- A group member can still make everyday changes, but the two moves that made
  backups silently useless — a month-long overdue window and excluding a whole
  source — are now out of reach through the web UI. root can still do anything
  in the file, which is the intended trust boundary.
- The rule for excludes is deliberately a little stricter than the minimum: any
  anchored single-component pattern is refused, not only the configured sources
  and (in system mode) the system directories. Such a pattern can only ever
  remove a whole top-level entry of an archive, so nothing useful is lost, and
  the rule needs no special case for system mode.
- The `settings` announcement and the `automatic_off` condition are two more
  message kinds the hook may see (`NESTEGG_EVENT=settings`,
  `automatic_off`); existing hooks ignore unknown events.
- The detail of the `settings` message (who, which patterns) can contain
  paths, so it follows the two detail levels of
  [decision 0017](0017-notifications.md): members of group `nestegg` and root
  see it in full, everyone else only learns that the settings were changed.
- `tests/e2e/systemd.sh` switches automatic backups off through the API and
  checks that the hook gets the `settings` event with the user's name without
  waiting for the hourly timer, that the `automatic_off` condition follows, and
  that an exclude `/home` is refused with the plain message.

## Alternatives rejected

- **Only report, do not limit.** Reporting alone leaves the extreme
  combinations possible: a determined or careless group member could still set
  a month-long overdue window or exclude a whole home directory, and the owner
  would get one message and then silence. The limits keep the worst cases out
  of reach; the report covers everything else.
- **Settings for administrators only (no group editing).** Taking the editable
  settings away from group members would undo the point of
  [decision 0013](0013-editable-settings-in-the-web-ui.md) — everyday changes
  without `sudo`. Most changes are harmless and useful; only the extremes need
  to be fenced off, and the rest is made visible rather than forbidden.
