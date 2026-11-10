# 0017 – Telling people when backups fail or stop

- Status: accepted
- Date: 2026-11-09

## Context

nestegg backs up a family desktop from a root systemd timer. Nobody learns
that backups fail, or have silently stopped, unless they open the web UI:

- `nestegg-backup.service` treats exit code 75 (server unreachable) as
  success and has no `OnFailure=`.
- "Overdue" was only a computed display state (`state::health`); nothing
  looked at it unless someone asked.
- A backup that succeeds but leaves something out (`Run::incomplete`), or a
  server that reports damaged snapshots (`State::server_verify_failed`), had
  no way to reach a person either.

The people who need to know are usually not the ones who look at a terminal
or a dashboard. Some maintainers want a push message to their phone or a
mail instead of (or besides) a notification on the desktop.

## Decision

A separate hourly check, independent of the backup run: **`nestegg notify`**
(root), started by `nestegg-notify.timer` (hourly, `Persistent=true`, five
minutes after boot) and, to report a result right away, by
`nestegg-backup.service` itself through `OnSuccess=` and `OnFailure=`.
`SuccessExitStatus=75` stays: an unreachable server is reported through
`overdue`, not on every missed attempt.

**What is a problem** (from the same data as `nestegg status`):

| Event | When |
|---|---|
| `failing` | The last run failed. The message names the error (`credentials_unreadable`, key not exported, a missing source, ... are failures with their own text). Also when nothing ever succeeded. |
| `overdue` | No success within `overdue_after_hours`. This also covers a server that stays unreachable, and a setup that was configured but never succeeded once its configuration file is older than `overdue_after_hours`. |
| `incomplete` | The last backup succeeded but left something out (`Health::Incomplete`; the reasons are in the message). |
| `damaged` | The server's own verification marks snapshots of this computer as failed. |

Nothing is reported while nestegg is not configured (no configuration file)
or while a backup is running. A configuration file that exists but cannot be
used is a `failing` problem with the parser's message, and its `[notify]`
section is still read if it is valid. `failing` wins over `overdue` and `incomplete` (as in
`state::health`); `damaged` is independent.

**Policy**, kept in `/var/lib/nestegg/notify.json` (root-owned):

- A problem is announced when it appears, then repeated every 24 hours while
  it lasts. `incomplete` repeats every 7 days; so does `overdue` while the
  backup timer is switched off (automatic backups off on purpose; the text
  says so). A repeat that is due within 30 minutes counts, so the daily
  rhythm does not slip by an hour per day.
- A changed problem (another kind, another error text, the timer switched
  on or off) is announced again at once.
- When all problems are gone after something was announced, one "backups
  work again" message follows. It goes to the people and the hook that heard
  about the problem; if a user is not logged in then, it waits for up to
  three days.
- A last run that ended without a result (server unreachable, cancelled)
  neither confirms nor ends a failure: an announced failure stays open and
  no "works again" message is sent on that basis.
- Bookkeeping is per user: someone who logs in a day after the message still
  gets it, nobody gets it twice.

**Channel 1, desktop** (`[notify] desktop = true`, the default): for every
regular user (uid 1000 to 59999) with a session bus at
`/run/user/<uid>/bus`, `notify-send` runs as that user, with their uid, gid
and supplementary groups (the helper of `identity.rs`), `DBUS_SESSION_BUS_ADDRESS`
and `XDG_RUNTIME_DIR` set, app name `nestegg`, urgency critical for
`failing`, `overdue` and `damaged`. A missing `notify-send` or a session
that does not answer is logged, not an error, and is tried again at the next
check. The texts are English or German, chosen from the system locale
(`/etc/default/locale`, `/etc/locale.conf`, then `LANG`), in plain everyday
language, and end by pointing to `nestegg ui`. The package recommends
`libnotify-bin`.

**Channel 2, command** (`[notify] command = ["/path/to/hook", "arg"]`, off
unless set): an array, run without a shell, as root, with a timeout of 60 s.
The event is in `NESTEGG_EVENT` (`failing`, `overdue`, `incomplete`,
`damaged`, `recovered`, `test`), `NESTEGG_TITLE`, `NESTEGG_MESSAGE`,
`NESTEGG_HOSTNAME` and `NESTEGG_LAST_SUCCESS` (RFC 3339 or empty); the
message is also on stdin. A failing hook is logged and tried again at the
next check; it never stops the desktop channel. `docs/configuration.md` has
an ntfy and a mail example.

**Not editable through the web UI.** The hook runs as root, so `[notify]` is
terminal-only like the server and the sources: the settings API accepts a
fixed set of keys and names `notify`, `desktop` and `command` as terminal-only
(`settings.rs`), and its writer keeps the section when it rewrites the file
(tested).

**Units.** `nestegg-notify.service` is a oneshot with the usual restrictions
(`ProtectKernelTunables`, `ProtectControlGroups`, no module loading) but,
unlike the backup unit, without `NoNewPrivileges`: a hook that sends mail
through Postfix's `sendmail` needs its set-group-ID bit. Nothing that hides
paths or identities is allowed either (decision 0007), and no network
restrictions, because a push-message hook needs the network.
`crates/nestegg/tests/units.rs` enforces both.

**Package.** The package enables `nestegg-notify.timer` once, on a first
installation and on the upgrade from a version that did not have the unit
(`postinst`; the marker `/var/lib/nestegg/notify-timer-enabled`). Later
upgrades leave a timer the user switched off alone. `prerm` and `postinst`
treat it like the other units for remove and reinstall (decision 0012). The
check does nothing while nestegg is not configured, so enabling it without
setup is safe.

**Commands.** `nestegg notify --test` sends a test message through all
configured channels and prints what it did; `nestegg notify --dry-run`
prints what would be sent and changes nothing.

## Consequences

- A failed backup usually reaches the desktop and the hook within seconds
  (if a check is already running at that moment, the next hourly check
  reports it); a quiet stop of backups (timer off, server away for days, nothing ever succeeded)
  within an hour of passing the threshold.
- A server that is switched off most of the week is not noisy: only
  `overdue` counts, once per day (or per week with automatic backups off).
- The record `notify.json` is small and can be deleted; the worst case is
  that current problems are announced once more.
- Manual runs started from the web UI are transient units without
  `OnSuccess=`/`OnFailure=`; the next hourly check reports their result.
- Without a desktop session and without a hook, nothing is delivered and
  nothing is lost: the problem stays visible in `nestegg status` and the
  web UI.
- Texts exist in English and German only; another language falls back to
  English. Error texts and the reasons for an incomplete backup are quoted
  as recorded, in English.
- The desktop body is escaped (`&`, `<`, `>`): notification services render
  it as markup, and an error text can contain text from the backup server
  or a file name. The hook gets the plain text.
- A state file that cannot be read (damaged, or written by a newer version)
  is reported as `failing` like an unusable configuration file; backups
  fail on the same file, and nothing else would report it.
- "Set up but never backed up" counts from the first check that saw it (or
  the date of the configuration file, whichever is earlier), so changing a
  setting does not restart the wait.
- The same error with other numbers in its text (a size, a duration) is the
  same problem, not a new one.
- With two problems at once, the end of one of them is not announced while
  the other lasts; "backups work again" is sent when nothing is left.
- Regular users are those from `UID_MIN` (`/etc/login.defs`) up to the end
  of the systemd-homed range (60513).
- After `apt remove` and a new installation the timer is enabled again if
  it was enabled before, also without a configuration (unlike the backup
  timer, it needs none).

## Alternatives rejected

- **Only `OnFailure=` on the backup unit.** Reports nothing for a server that
  is unreachable (exit 75 is deliberately success), a switched-off timer, a
  computer that was off for a week, an incomplete backup or damaged
  snapshots.
- **Mail or push built into nestegg.** Needs a mail stack, credentials and a
  network protocol in a program that runs as root; a hook command covers
  every service without that.
- **A shell string for the hook.** Quoting problems and an injection
  surface; an array is explicit.
- **Talking to the notification service over D-Bus from the daemon.** Needs a
  D-Bus library and the same identity switch; `notify-send` is the existing,
  tested interface and absent hosts degrade quietly.
- **`loginctl` or `sd-login` to find sessions.** A session bus socket below
  `/run/user/<uid>` is what `notify-send` needs and avoids another
  dependency; users without a running session bus cannot receive a message
  anyway.
- **Editing `[notify]` in the web UI.** Anyone in group `nestegg` could then
  run commands as root.
- **Repeating a problem every hour.** Alert fatigue; people stop reading.

## Update 2026-11-10 (decision 0022): hardening and two new events

Review of the health check and the editable settings
([decision 0022](0022-settings-changes-that-reduce-protection.md)) changed five
things on the notification side. The policy and the two channels are otherwise
as above.

- **Two new events.** `automatic_off` ("Automatic backups are off") is now its
  own condition, announced at once and repeated every seven days, independent
  of the overdue threshold; it replaces the old "overdue while the timer is
  off" variant, and the text still says how old the last backup is. `settings`
  ("Backup settings changed") reports a group member lowering the protection
  through the web UI; the daemon records it in `notify.json` and starts the
  check at once. Both are also hook events (`NESTEGG_EVENT`).

- **Two detail levels.** `find_sessions` reaches every regular user, not only
  members of group `nestegg`, and a failure or settings text can contain paths
  and file names. Members of group `nestegg` and root now get the full desktop
  text; every other logged-in user gets a short version that only names the
  kind of problem ("the backup of this computer failed / is overdue / is
  incomplete / automatic backups are off / the settings were changed") and
  asks them to tell whoever looks after the computer. The hook always gets the
  full text.

- **The record is saved after each delivery.** `notify.json` was written only
  at the end of a check; a check killed halfway (unit timeout, reboot)
  redelivered everything it had already sent. It is now saved after every
  desktop and hook delivery.

- **A failing hook makes the unit fail.** A hook that failed was only logged,
  so `systemctl --failed` and the journal priority showed nothing. After
  delivering everything else, `nestegg notify` now ends with a non-zero exit
  code when a hook delivery failed (the retry at the next check stays). This
  cannot loop or block the backup unit: `nestegg-notify.service` has no
  `OnFailure=`, and the backup unit only *starts* it through `OnSuccess=` /
  `OnFailure=`, so the check's own failure does not propagate back.

- **The configuration file is group-readable, not world-readable.** Because a
  `[notify] command` may carry a token, `/etc/nestegg/config.toml` is now
  created `root:nestegg` mode 0640 by `nestegg init` (group members still read
  it for `nestegg status`); the settings writer keeps that owner, group and
  mode, and the package's `postinst` corrects an existing 0644 file on upgrade.
  A secret a hook needs belongs in a root-only file that the command reads; the
  ntfy example in `docs/configuration.md` shows this.

## Update 2026-11-10 (decision 0022, second review): settings pacing and locks

A second review of the settings-change reporting refined how the notification
machinery delivers and protects itself (full detail in
[decision 0022](0022-settings-changes-that-reduce-protection.md)):

- **The `settings` event is paced.** A check started for a settings change
  waits out a short quiet period before taking the notification lock (so a burst
  coalesces into one run), re-drains the inbox once more before releasing the
  lock (so a change recorded during a slow hook is delivered by the same run,
  not the next hourly timer), and after the first message holds a recipient to
  at most one settings message every few minutes (later ones combined, nothing
  dropped). `nestegg-notify.service` keeps `StartLimitIntervalSec=0` so a burst
  cannot trip the default start-rate limit and drop reports.

- **The report is durable.** The change is recorded as a write-ahead intent
  before the configuration is changed and completed afterwards, so a crash in
  between still yields a (generic) message; a damaged inbox is set aside and
  reported, not silently discarded.

- **The notification lock files are root-only.** `.notify.lock` and
  `.notify-settings.lock` (like `.current.lock` and the run lock) are now `0600`
  and tightened on open, so a group member cannot hold a shared `flock` that
  would block the daemon. The daemon's wait for the inbox lock is time-bounded.

## Update 2026-11-10 (third review): robustness of the health check

A third review of `nestegg notify` fixed five edge cases. None changes the
policy above; they make it hold up against undelivered reports, a wrong clock, a
record from a newer version, a stuck lock, and a many-user desktop.

- **A settings announcement is never lost.** It used to be dropped three days
  after its time whether or not it had reached anyone (nobody logged in over a
  long weekend, the hook kept failing), and a fresh change merged into an older
  entry inherited the older time and could expire within minutes. Now an
  announcement leaves the record only after it has been delivered on **every
  channel that is switched on** — the hook (if configured) has succeeded, and
  the desktop message (if the desktop channel is on) has reached at least one
  user — and only then is it kept for the keep window (three days), counted from
  the **newest** change in a merged entry. Time alone never removes an
  undelivered announcement; a channel that is switched off holds nothing back;
  the record stays bounded by combining per user as before. A hook that keeps
  failing is retried for ever but with a growing, bounded pause (doubling from
  five minutes up to six hours), so it is not hammered at every run.

- **Times in the future are clamped.** If the clock was ahead when an entry was
  written and was then corrected, a stored time could lie in the future and
  (depending on where it was compared) silence the repeat of a problem, hold a
  rate limit open until the clock caught up, keep an "intent in flight" window
  from ever ending, or push the debounce past the unit's `TimeoutStartSec`. Every
  stored "when it happened" time is now pulled back to now where the state and
  the inbox are read (and written back); the rate-limit and retry stamps, which
  are meant to lie ahead, are instead bounded where they are used so a future
  value can never suppress a message or hold one back for longer than the
  backoff cap; and the settings debounce is bounded by real elapsed time
  regardless of what the stored times say.

- **An unknown last-run state is reported, not ignored.** A run record written by
  a newer nestegg (`RunState::Unknown`) was neither a success nor a failure to
  the places that only looked for `failing`, so it could pass silently. It is now
  its own event, **`outdated`** ("nestegg needs an update"), announced once like
  a failure (it suppresses the quieter overdue/incomplete just as a real failure
  does) with a text that says this version does not understand the last record
  and nestegg should be updated. It is detected from the run state directly, so
  it is correct whether or not `state::health` already classifies an unknown
  state as `failing`. `outdated` is another hook event (`NESTEGG_EVENT`); it
  carries no private path, so every logged-in user sees the same text.

- **The notification lock wait is bounded.** Taking `.notify.lock` used to block
  for ever if another process held it (or the run-state lock the check takes
  through `nestegg status`), so the unit would hang until systemd killed it at
  `TimeoutStartSec`. The wait is now time-bounded (90 s, comfortably longer than
  one check holds the lock but well below the unit's limit even after the
  settings debounce); on timeout the check ends with a clear message and a
  non-zero exit, and the timer tries again. (The `.current.lock` wait inside
  `nestegg status` / `state.rs` is a separate code path and still blocks; bounding
  it is out of this change's scope.)

- **Desktop message text is visible in the process list (accepted limit).** The
  title and body are passed to `notify-send` as command-line arguments, so on a
  multi-user computer another local user can read them — including the
  full-detail version a group member sees — for the moment the message is shown;
  `hidepid` systems are unaffected. Avoiding this would mean calling
  `org.freedesktop.Notifications.Notify` over D-Bus, but no D-Bus library is in
  the dependency tree and the decision above keeps `notify-send` rather than add
  one. The limit is documented in `docs/configuration.md` and accepted; it is not
  changed here.

## Update 2026-11-10 (fourth round): who releases a settings message, future success stamps, the unit state

- **A settings message is delivered when a member has it, not when anyone has
  it.** The desktop part of "delivered" counted as soon as any logged-in user
  had received the announcement, also a user outside group `nestegg`, who only
  gets the general text. After the keep window a member who logged in later
  never saw it. An announcement now carries `full_done`: set when the message
  with the full text reached at least one member (or root). Users without the
  detail still get the general text once each (`users_done`), but they do not
  release it. When entries are merged above the bound, `full_done` is the
  intersection like `users_done` (a member who saw only the older change has
  not seen the combined one). With the desktop channel off nothing is held for
  it. So that the record stays bounded on a computer where no member ever logs
  in, the wait for a member ends after 30 days (counted from the newest change):
  the desktop part then counts as done, the announcement is dropped by the
  normal keep window, with one journal line. A hook message that was never
  delivered is still never dropped by time. Records written before the field
  existed read as "not yet seen by a member"; they wait at most the 30 days.
- **A success stamp in the future is not a fresh backup.** `evaluate` computed
  the age with `.max(0)`, so with a last success stamped in the future (the
  clock was ahead, then corrected) a check concluded "fresh backup, nothing to
  report" and, after an earlier overdue message, announced "works again". A
  stamp more than 5 minutes ahead of the clock now says nothing about the age
  (`age_hours` is empty): while the timer is on, the backup counts as overdue
  (event `overdue`, key `clock`, text: the time of the last backup lies in the
  future, the clock of this computer was wrong, nestegg backs up again at the
  next check); with the timer off the `automatic_off` text says the same
  instead of "not backed up yet". It is decided from the stamp itself, so it is
  right whether or not `status.health` already reports it, and it hides the
  quieter `incomplete`; a failed last run stays the louder message. The hook's
  `NESTEGG_LAST_SUCCESS` is empty then. Up to 5 minutes ahead is clock noise
  and counts as now.
- **The unit's failed state no longer flips.** A settings message the hook could
  not take is retried with a growing pause. Only the check that really tried
  the hook ended non-zero; a check held back by the pause ended 0 and cleared
  the failed state of `nestegg-notify.service`, so `systemctl --failed`
  alternated. While the failure is pending (`settings_command_failures` > 0 and
  the hook still switched on) every check ends non-zero with one line
  ("the command still fails ..."), also the ones inside the pause. The other
  hook paths (problem and recovery messages) try at every check and already
  ended non-zero each time, so they agree. Switching the hook off ends the
  pending failure (counter and pause are reset); a later switch-on starts fresh.
