# 0030: Offline rescue reconciles network pins and backup state

## Context

A system snapshot captures the state directory before its own backup has
finished. Restoring it can therefore display "never" and a captured running
backup. Hardware-specific network selectors can also leave the restored
system offline when the NIC changes.

## Decision

Reconcile the offline tree in rescue, after file restoration and before
reporting success. Do not defer this to a daemon start: CLI-only installations
must be truthful on their first status query too. Record nullable
`restored_from` provenance (`snapshot`, `snapshot_time`, `restored_at`,
`target`), and a synthetic successful run for the source target at the snapshot
time. Its original trigger is unknown and `verified` stays false; a successful
restore is not a newly completed upload/read-back. Preserve provenance across
later backups, other target histories and key/export metadata. Remove the
captured current-run record and offline lock files. Kernel locks themselves
cannot survive restoration. A malformed snapshot time fails rather than
inventing a success time. `api/openapi.yaml` defines the additive API field.

Network fix-up only rewrites literal selectors when all detected wired pins name
one absent MAC, other selectors are explicitly Wi-Fi, and exactly one physical
wired NIC exists. Known Wi-Fi pins are reported and left unchanged. Sysfs `device`, link type and absence of `wireless`/`phy80211` identify
physical wired candidates. This intentionally excludes uncertain formats,
udev and ifupdown from automatic changes. Back up every changed network file
next to it under a hidden non-configuration suffix, preserving permissions;
never globally replace MAC strings or modify Wi-Fi secrets. Report both
changed and unchanged findings with a manual remedy in the final summary and
on disk. Do not change cloud-init policy or its datasource; explain that it
can regenerate the file and how the operator can make a durable correction.

The shared-lock regression test must not infer holder visibility merely from
a readable `/proc/locks`. The existing kernel probe already handles unlisted
locks (including filesystems whose lock/stat device numbers differ). Test
real lock exclusion with deterministic injected lock-list data for the PID
message, and explicitly test empty/unreadable diagnostic input as well. Pass
the injected reader through to diagnostics; no process-wide environment
mutation or retries that conceal an assertion failure. A disposable btrfs
filesystem reproduced the differing stat/lock device numbers. The historical
parallel CI omission itself was not reproduced here, so its precise kernel
or filesystem trigger remains unknown; the invalid test assumption is proven.

## Consequences

Health may be overdue immediately after an old snapshot is restored. The
restore origin remains distinct from the last backup. Conservative network
repair may require manual name/datasource corrections and does not promise
arbitrary network configuration migration. No new network access, protocol
implementation or UI change is needed.
