# 0011 – The web UI offers restore based on the server, not on local history

- Status: accepted
- Date: 2026-11-09
- Relates to: decision 0003 ("reinstall the distribution, then restore")

## Context

The daemon derives `health` and `last_success` of `/status` from its local
state in `/var/lib/nestegg`. After a reinstall, or when that directory is
lost, the state is empty: health is `never` and `last_success` is missing,
although the snapshots of the same backup id are still on the PBS. The web
UI used to show "Restore files" and ask `/snapshots` only when
`last_success` existed, so restore was unreachable exactly when it matters
most, and the page claimed there was no backup at all.

## Decision

1. **Restore is offered whenever the computer is set up** (health is not
   `unconfigured`). The page of this computer loads `/snapshots` once per page
   load in that case, independent of `last_success`.
2. **The snapshot list decides, the local history does not.** The entry is
   shown while the list loads and when loading fails (step 1 of the restore
   flow shows the error with a retry, and "On the server" shows it too). Only
   a loaded, empty list hides it; if the local history still names a backup,
   a short hint says that the server holds none.
3. **Texts stay honest without local history.** With health `never` and
   snapshots on the server, the page says "No backup from this installation
   yet" and names the number and the newest of the earlier backups. Where
   `/status` leaves out `last_success` (allowed by the contract), the newest
   snapshot time is used as the last backup. The 14-day strip also counts days
   with a snapshot on the server.

## Consequences

- One `/snapshots` request per page load also for computers without a local
  backup. It needs the PBS; a sleeping server shows the usual "does not
  answer" note, it is not woken for this.
- The mock has a scenario `reinstalled` (empty local state, snapshots on the
  server) to reproduce this.
