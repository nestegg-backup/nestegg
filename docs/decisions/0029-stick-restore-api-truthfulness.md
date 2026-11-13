# 0029 – Explicit restore sources and live stick usage

- Status: implemented
- Date: 2026-11-11
- Extends: 0026, 0028

Snapshot lists, catalog responses and restore jobs echo their actual `target`
and resolved `stick_uuid` (null for PBS). API capability version becomes 2;
existing requests and response fields remain supported. The UI requires a stick
echo and rejects a different source or UUID. A successful response from an older
daemon that ignores `target=stick` must never appear as stick data. Legacy PBS
responses without the new fields remain usable.

Status lists currently attached registered UUIDs. With several attached sticks,
the user chooses a UUID before listing snapshots. The same UUID accompanies
browsing and restoration; the daemon also pins an implicitly selected UUID
between synchronous validation and the asynchronous job. A stable unavailable
reason separates absence, ambiguity, contention, a snapshot on another medium,
unreadability and unsupported systems. User-facing text is translated in the UI.

A private per-medium kernel lock carries the operation kind. It is acquired
before opening the device and held through PBS shutdown, filesystem flush and
unmount, including error cleanup. Listing, validation and catalog browsing count
as restore use. `/status` reports live usage for every registered UUID and as an
aggregate; stale text after a process exits is ignored once the kernel lock is
released. Failed cleanup retains an unsafe marker until the next successful
session or reboot; a failed probe is conservatively busy. Neither the UI nor another tab
may infer safe removal solely from the previous backup's success. Old status
responses without live usage no longer get a safe-removal claim.

The desktop worker continues to hold the shared run lock for its entire
session. The device-bound backup unit runs `nestegg run --trigger plug`, which
waits for this same lock before opening a session. It does not own or stop the
restore worker's private PBS; each session stops only its own server. Kernel
lock tests cover contention with restore usage and idle state after release.
The disposable KVM API acceptance covers scoped non-root restoration and can
pause a catalog read to observe live usage and a competing request's busy reason.

Running byte counts were first deferred, because the isolated worker returned
one final envelope. They are now implemented (2026-11-13) as a contained
protocol extension: the worker already counts the staging copy exactly as the
backup-server path does (the size of the staging directory, polled while the
client restores; the total is the archive size from the snapshot's file list,
only when whole archives are restored). When the daemon sets
`NESTEGG_STICK_PROGRESS=1`, the worker writes one line
`{"progress":{"restored_bytes":N,"total_bytes":T|null}}` per second to stdout
before the unchanged final envelope; a helper thread is joined before the
envelope is written, so no line can follow it. The daemon splits these lines
from the envelope and updates the job; a worker without the variable (or an
older one) writes only the envelope, which is still accepted. The UI shows
bytes as soon as they arrive and a bar and percentage only with a total;
without any count (older daemon) it keeps elapsed time and final bytes.
