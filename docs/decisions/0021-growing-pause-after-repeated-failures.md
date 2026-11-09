# 0021 – Growing pause between automatic retries after repeated failures

- Status: accepted
- Date: 2026-11-09

## Context

The timer checks every hour whether a backup is due. A backup is due until
one succeeds, so a problem that does not go away by itself (a revoked API
token, a full datastore, a snapshot that fails the read-back check of
decision 0015) was retried every hour, forever. Each retry that gets as far
as the server wakes it with Wake-on-LAN: a server that is meant to sleep was
woken 24 times a day for nothing.

Since decision 0017 the user is told about a failing backup, so retrying
often no longer serves to "get noticed".

## Decision

1. The state counts failed runs in a row (`consecutive_failures`). A success
   resets the count. Runs that ended as `pbs_unreachable` or `cancelled` are
   neither failures nor successes: they do not count and do not reset.
2. The first failure is retried at the next hourly check as before (many
   failures are transient). From the second failure in a row the timer waits
   2, 4, 8, 16 hours after the last failure before it tries again, never
   longer than `schedule.interval_hours`. A check that falls into the pause
   ends like "not due": exit code 0, no run record, the server is not
   contacted.
3. The pause ends at once when something changes that can cure the failure:
   the configuration file is written after the failed run started (by hand
   or through the web UI),
   credentials are stored (`credentials set-password`), a key is created,
   imported or confirmed. A manually started run (`nestegg backup`, "Back
   up now") is never held back.
4. `schedule.next_run` of the status shows the real next attempt.

An unreachable server keeps being checked every hour: that is the normal
case for a laptop away from home or a server that is switched on in the
evening, and the check itself costs one TCP connection attempt (plus the
Wake-on-LAN packets, repeated with growing pauses within one check, see
decision 0009).

## Consequences

- A permanently failing backup contacts the server once per backup interval
  instead of every hour.
- After a fix that nestegg cannot see (the token was repaired on the
  server), the next automatic attempt can be up to one interval away; the
  message of the paused check and the documentation name `nestegg backup`.
- State files of older versions have no counter; it starts at 0.

## Alternatives rejected

- **Pausing only for failures after the server was contacted.** Local
  failures (key not exported, source missing) are cheap to retry, but the
  rule "which error kinds pause" is hard to explain, and the commands that
  cure them end the pause anyway.
- **A configurable schedule.** Nobody should have to tune this.
