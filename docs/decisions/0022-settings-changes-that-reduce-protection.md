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
- The exclude rule is a guard against accidents and casual misuse, **not a
  security boundary**. A group member can still narrow the backup in ways that
  are not refused — excluding a user's folders one by one, for example — but
  every such change is reported, named, to the owner and the hook. The refusal
  only fences off the one-move "drop a whole home or source" cases; see the
  2026-11-10 revision below for how the rule is actually evaluated (the first
  version's claim that it "needs no special case for system mode" was wrong).
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

## Revision 2026-11-10 (review follow-up)

An independent review found gaps in the first implementation. The corrections:

### The exclude rule is evaluated against what the backup client really does

The first version matched patterns syntactically and was both too weak and too
strong: with the default configuration it accepted `alice`, `/[a-z]*`,
`alice/**` and other spellings that remove a whole user's home from
`home.pxar`, while refusing harmless anchored names such as `/.cache` with a
misleading message. The claim in *Consequences* that an anchored
single-component pattern "can only ever remove a whole top-level entry" was
simply not true (archive paths are relative to each source, so `/alice` in
`home.pxar` is a whole home directory).

The rule now builds the pattern exactly as `proxmox-backup-client` does — one
`MatchEntry` from the `pathpatterns` crate (the client's own matcher,
`AGPL-3.0-or-later`, so licence-compatible) — and tests whether, applied in the
relevant archive, it would leave out a whole protected folder:

- a **configured source** (`/home`, `/etc`, ...), tested by its path;
- a **regular user's home directory** that lies inside a source
  (`/home/alice`), taken from the account database (NSS), tested in that
  source's archive (`/alice` in `home.pxar`); and
- in **system mode**, a whole **top-level system directory**
  (`/etc /home /root /usr /var /opt /boot /srv`) in `system.pxar`.

"Leaves out the whole folder" means the pattern matches the folder itself, or
matches every child (so `alice/**` and `alice/*` count). The cases above, the
near misses (`**/.cache`, `/alice/Downloads`, `/home/*/Videos`, ...) and the
real behaviour of each were confirmed with `pxar create --exclude` in the e2e
client image and turned into a table-driven test. As noted above, this is a
guard against accidents, not a security boundary.

The pattern-length limit is now counted in **bytes** (256), so a few hundred
multi-byte characters cannot slip past it, and invisible/format characters in a
pattern are rejected like control characters.

### The weekly rhythm keeps a valid "overdue after"

Capping "overdue after" at 168 h while requiring it to exceed the interval left
a weekly rhythm (interval 168 h) with no valid threshold. The API cap is now
`max(168, interval + 72)` hours — evaluated against the interval in effect
after the same request, since both keys can change together — so a weekly
rhythm can be overdue after up to ten days. `limits.overdue_after_hours` reports
`base` (168), `per_interval_extra` (72) and `file_minimum` so the web interface
can recompute the cap for an interval it is about to set. Raising the threshold
above 168 h this way is still reported as a reduction.

### Delivery, flood control and honesty

- **Durable record before the answer.** A change is written to a small inbox
  with its own short lock *before* the HTTP response, so a daemon restart (a
  package upgrade) while a slow check holds the main lock cannot lose it. The
  next check drains the inbox under the main lock and announces it.
- **No flood.** All pending changes for one recipient are combined into a
  single message (identical reductions listed once), the stored list is capped,
  and `nestegg-notify.service` keeps `StartLimitIntervalSec=0` because systemd
  coalesces the repeated `start --no-block` triggers and one run now does all
  the pending work.
- **Every change that took effect is reported**, even when the request ends in
  an error (the timer could not be started after the file was saved, or the
  rollback failed).
- **More reductions are reported:** removing a `!` re-include, and reordering
  the exclude list when a re-include is present (later patterns win).
- **"Automatic backups are off" is evaluated next to the other conditions**, so
  it is announced even when the last run also failed, not only in the
  else-branch of a failure.
- The condition is named `automatic_off` everywhere now (the record and the
  hook); records written as `automaticoff` by the first version still load.
- Every message is held under a few kilobytes before it reaches `notify-send`,
  the hook environment or the hook's stdin, and the stdin write cannot block
  the check, so a huge exclude list can no longer make delivery fail or hang.

### Packaging and `init`

- `postinst` only **tightens** an over-permissive `config.toml` (readable by
  users outside the group); a stricter `root:root 0600` chosen by an
  administrator is kept, the mode is set only after the owner change worked, and
  a symlink is never followed.
- `nestegg init` creates the file `0640` from the start (no window at the umask
  default). Without the `nestegg` group it stays `root:root 0640` (root-only,
  safe) with a note telling the administrator how to let group members read the
  status once the group exists.
