# 0009 – A Wake-on-LAN packet that cannot be sent is not a failure

- Status: accepted
- Date: 2026-11-09

## Context

With `[wake]` configured, a run that finds the PBS down sends a magic packet
and polls the PBS until `wake.timeout_seconds`. Sending the packet can fail
before anything leaves the machine: a laptop without network (no Wi-Fi,
flight mode) or one that has just resumed from sleep and is still
reconnecting gets `Network is unreachable` from `sendto()`.
`After=network-online.target` does not help after a resume.

This error used to abort the run as `failed` (exit code 1, error kind
`internal`). The same machine without `[wake]` correctly ended as
`pbs_unreachable` (exit code 75). The health turned `failing` every hour, the
systemd unit went into the failed state, and `pbs_reachable` was not
updated.

## Decision

1. An error while sending the magic packet is reported once as a run
   warning with a plain explanation ("could not send the Wake-on-LAN packet
   that wakes the backup server (Network is unreachable). Is this computer
   connected to the network? …"). It never aborts the run.
2. The run keeps waiting until the configured timeout, as documented for
   Wake-on-LAN. While no packet has gone out, it retries the packet at every
   poll (every 5 s), so the packet leaves as soon as the network is back;
   after a successful packet it repeats it with growing pauses (10, 20,
   40 s, then every minute; before 2026-11-09 a fixed minute), so a lost
   first packet costs seconds, not a third of the default timeout.
3. If the PBS is still not reachable at the timeout, the run ends as
   `pbs_unreachable` (exit code 75) and records `pbs_reachable = false`. The
   error message says that sending the wake-up packet failed.

## Alternatives considered

- **Return `pbs_unreachable` immediately on a send error.** Faster for a
  machine that is really offline, but it gives up in the common resume case
  where the network comes back a few seconds later, and it would differ from
  the documented "send the packet and poll until the timeout". An offline
  machine waiting the full timeout costs nothing but time in a background
  run.
- **Retry only for selected errors (`ENETUNREACH`, `EHOSTUNREACH`).** Other
  send errors (for example a socket that cannot be opened) also leave the
  PBS asleep; treating them differently would turn the same situation into a
  hard failure again. All send errors are handled the same way, and the
  warning shows the actual cause.
