# 0007 – Sandboxing of the systemd units

- Status: accepted
- Date: 2026-11-09

## Context

The first packaged units used the usual systemd hardening options. Two of
them broke the product silently:

- `nestegg-backup.service` had `ProtectKernelModules=yes`. systemd
  implements it partly by mounting an empty, inaccessible tmpfs over
  `/usr/lib/modules` (and `/lib/modules`). `proxmox-backup-client` does not
  cross mount points, so every timer-made `system.pxar` lacked the kernel
  modules, and a rescue restore of such a snapshot could not build an
  initramfs or load drivers. The run still reported success. Manual runs
  (plain `systemd-run` without these properties) were not affected, so the
  defect depended on how the run was started. `PrivateTmp=yes` likewise
  replaced `/tmp` and `/var/tmp` with private bind mounts, which the disk
  layout then recorded as mounts of the system.
- `nestegg.service` (the daemon) had `ProtectSystem=full`,
  `ProtectHome=read-only`, `PrivateTmp=yes` and `RestrictSUIDSGID=yes`.
  API restores run inside the daemon and its child processes (root extracts
  into `/var/lib/nestegg/restore-staging` or directly into a root-owned
  directory; a `tar` process running as the requesting user copies into a
  new directory below the user's home or `/tmp`). They share the unit's
  mount namespace, so restores into `/home`, `/root`, `/etc` or `/usr/local`
  failed with "Read-only file system", restores into `/tmp` went into a
  private directory nobody else could see, and setuid/setgid entries failed
  with "Operation not permitted", also for root.

## Decision

1. Neither unit uses options that hide, replace, remount or lock paths, or
   that change the identity files are read and written with:
   `PrivateTmp`, `ProtectSystem`, `ProtectHome`, `ProtectKernelModules`,
   `PrivateDevices`, `ProtectProc`, `ProcSubset`, `PrivateUsers`,
   `DynamicUser`, `User`/`Group` other than root, `InaccessiblePaths`,
   `ReadOnlyPaths`, `TemporaryFileSystem`, `BindPaths`,
   `BindReadOnlyPaths`, `RootDirectory`, `RootImage` and the image options.
   `RestrictSUIDSGID` is ruled out as well, because restores must recreate
   setuid and setgid entries.
2. Allowed is hardening that only restricts privileges or touches the
   `/proc` and `/sys` pseudo file systems, which are never backed up or
   restored: `NoNewPrivileges`, `ProtectKernelTunables`,
   `ProtectControlGroups`, `LockPersonality`, a `CapabilityBoundingSet=~...`
   deny list that keeps the capabilities backups and restores need, and a
   `SystemCallFilter=~...` deny list of harmless groups.
3. The intent of `ProtectKernelModules` (no module loading) is kept with
   `CapabilityBoundingSet=~CAP_SYS_MODULE` and `SystemCallFilter=~@module`,
   which hide no files. Both exist in systemd 249 (Ubuntu 22.04, Linux
   Mint 21), the oldest supported version.
4. Restores keep running inside the daemon for now. Moving them into
   transient units with their own sandbox remains possible later; it would
   not change the rules above, since a restore must reach the same paths.

## Consequences

- `crates/nestegg/tests/units.rs` parses both unit files and fails on any
  forbidden option, on a capability or system call deny list that would
  break backups or restores, and on a missing module-loading block. It also
  checks the `--property=` list the daemon uses for manual runs.
- `tests/e2e/systemd.sh` checks that a timer-made snapshot contains a file
  below `/usr/lib/modules`, runs API restores as an unprivileged user into
  the home directory and `/tmp`, and as root into `/root` (setgid bit kept),
  all through the daemon started by systemd. These checks only mean
  something if systemd can create mount namespaces in the test container: in
  a default Docker container (no `CAP_SYS_ADMIN`, default seccomp and
  AppArmor profiles) systemd silently ignores mount sandboxing options, and
  the checks would pass even with the old units. The script therefore starts
  the container with `CAP_SYS_ADMIN` and unconfined seccomp/AppArmor, and
  first checks that `ProtectHome=read-only` really makes `/root` read-only.
- The daemon has less isolation than a typical network service. This is
  acceptable: it listens only on a Unix socket restricted to the `nestegg`
  group, and its job (restores as root into arbitrary root-owned paths)
  needs write access to the whole file system anyway.
