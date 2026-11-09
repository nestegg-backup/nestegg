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

- A failed backup reaches the desktop and the hook within seconds; a quiet
  stop of backups (timer off, server away for days, nothing ever succeeded)
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
  English.

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
