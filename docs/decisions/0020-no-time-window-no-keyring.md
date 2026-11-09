# 0020 – No backup time window, no desktop keyring for credentials

- Status: accepted
- Date: 2026-11-09

## Context

The roadmap listed two things for v0.1 that were never built: an "allowed
time window" for backups in the configuration, and "the system keyring" as
an alternative place for credentials. The roadmap marked v0.1 as
implemented, so it promised more than the code does.

## Decision

Both items are dropped from the roadmap instead of being built.

1. **No time window.** nestegg checks hourly whether a backup is due and
   catches up after the computer was off. A desktop or laptop is usually
   switched off in exactly the hours a "night window" would allow, so a
   window would mostly cause missed backups; the overdue threshold would
   then have to be explained together with it. The run already yields to
   interactive work (`Nice=10`, idle I/O class, batch CPU policy) and
   `backup.rate_limit` limits the upload.
2. **No desktop keyring.** Backups run as a root system service without
   anyone logged in. A user's keyring (GNOME Keyring, KWallet) is only
   unlocked while that user has a session, so a timer run could not read
   the secret. Credentials stay in `systemd-creds` (encrypted with the
   machine's key, decision 0008), with a root-only file as the fallback.

## Consequences

- `docs/roadmap.md` no longer lists either item; the Wake-on-LAN line there
  now describes what the code does (timeout, repeated packet with growing
  pauses) instead of an unspecified "backoff".
- If limits on *when* a backup may run are wanted later, the useful ones
  for a desktop are "not on battery" and "not on a metered connection";
  they would get their own decision.
