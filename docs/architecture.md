# Architecture

nestegg is a thin, careful layer around the official `proxmox-backup-client`.
It never touches backup data itself: chunking, deduplication, encryption,
transfer and restore stay in the official client.

```
            ┌──────────────── one desktop ────────────────────────────────┐
            │                                                             │
 systemd    │  nestegg-backup.timer ──► nestegg-backup.service            │
            │                              (oneshot, root)                │
            │                              `nestegg run --trigger timer`  │
            │                                     │                       │
            │                                     ▼                       │
            │   /etc/nestegg/config.toml   ┌──────────────┐   proxmox-    │
            │   systemd credentials ─────► │ run pipeline │ ─►backup-  ───┼──► PBS
            │                              └──────────────┘   client      │   (HTTPS 8007)
            │                                     │ state + history       │
            │                                     ▼                       │
            │                          /var/lib/nestegg/                  │
            │                                     ▲                       │
            │  nestegg.service (root daemon) ─────┘                       │
            │     JSON API on /run/nestegg/api.sock (0660 root:nestegg)   │
            │        ▲                         ▲                          │
            │        │                         │                          │
            │  `nestegg status` …        `nestegg ui` (user in group      │
            │  (CLI, any group member)    nestegg): HTTP on 127.0.0.1,    │
            │                             random port + one-time token,   │
            │                             serves ui/dist + proxies /api   │
            └─────────────────────────────────────────────────────────────┘
```

## Components

| Component | Runs as | Job |
|---|---|---|
| `nestegg run` | root, started by the timer or the daemon | One backup run: lock, optional Wake-on-LAN, wait for PBS, build system inventory, call `proxmox-backup-client backup`, upload status report, record result. |
| `nestegg-backup.timer` | systemd | Daily schedule, `Persistent=true` (catch-up after the machine was off), randomized delay. |
| `nestegg notify` (`nestegg-notify.service`, `.timer`) | root | Hourly health check, also started after every backup run: tells logged-in users (`notify-send`, run as the user) and an optional hook command when backups fail, are overdue, incomplete or damaged, and when they work again. Remembers what it reported in `notify.json` ([decision 0017](decisions/0017-notifications.md)). |
| `nestegg daemon` (`nestegg.service`) | root | Local JSON API over a Unix socket. Reads state/history, lists snapshots, starts runs (`systemctl start nestegg-backup.service`) and restores, and changes the few settings group members may change (rhythm, overdue threshold, excludes, Wake-on-LAN on/off, timer on/off; [decision 0013](decisions/0013-editable-settings-in-the-web-ui.md)). |
| `nestegg <command>` | user in group `nestegg`, or root | CLI. Read commands go through the socket; `init`, `credentials` and `key` need root. |
| `nestegg ui` | user in group `nestegg` | Starts a local HTTP server on `127.0.0.1:<random>` with a one-time token in the URL, opens the browser, serves the embedded web UI from `ui/dist`, and forwards `/api/v1/*` to the socket. |

Authorization is done by Unix permissions on the socket (group `nestegg`).
The browser side is protected by the random port, loopback binding and a
per-session token. Nothing listens on a network interface.

The backup and daemon units are hardened only with options that restrict
privileges (`NoNewPrivileges`, no module loading, read-only `/proc/sys` and
`/sys`); the notification unit does the same except `NoNewPrivileges`, which
would break set-group-ID mail programs in a hook
([decision 0017](decisions/0017-notifications.md)).
Options that hide or lock paths (`PrivateTmp`, `ProtectSystem`,
`ProtectHome`, `ProtectKernelModules`, ...) are not used: the backup unit
must see the whole file system for `system.pxar`, and API restores run
inside the daemon and write into home directories, `/tmp` and root-owned
directories. See [decision 0007](decisions/0007-systemd-unit-sandboxing.md);
`crates/nestegg/tests/units.rs` enforces it.

## Run pipeline

1. Acquire `/run/nestegg/run.lock` (no overlapping runs).
2. Check sources exist (missing configured sources fail the run instead of
   silently producing an incomplete backup).
3. PBS availability: a TCP connection to the PBS port (no credentials are
   sent). If unreachable and Wake-on-LAN is configured, send the magic
   packet, repeat it with growing pauses and poll until
   `wake.timeout_seconds`. Still unreachable → state
   `pbs_unreachable` (distinct from failure); the timer retries next time.
   A packet that cannot be sent (no network) is a warning, not an error:
   the run keeps retrying it while it polls and ends the same way
   ([decision 0009](decisions/0009-wake-on-lan-send-failure.md)).
4. Generate the system inventory into `/var/lib/nestegg/inventory/`
   (package selections, manually installed packages, Flatpak/Snap lists,
   enabled units, `lsblk`/`findmnt` output, OS release).
5. `proxmox-backup-client backup` with one archive per source
   (`home.pxar:/home`, `etc.pxar:/etc`, …, `inventory.pxar:…`) and the
   metadata blob `nestegg-meta.conf` (sources, archive → source path,
   incomplete reasons; written to `/run/nestegg/tmp/`),
   `--backup-type host --backup-id <id> [--ns <ns>]`, excludes,
   `--change-detection-mode`, rate limit. The encryption key (`--keyfd`) and the
   token secret (`PBS_PASSWORD_FD`) are handed over on file descriptors; no
   secret is ever a command-line argument or written to a log.
6. Read the snapshot back: all archives listed, metadata blob and
   inventory restored and identical; otherwise `verification_failed`. Then
   record the snapshots the server's own verification marked as failed
   ([decision 0015](decisions/0015-complete-and-verified-backups.md)).
7. Write a JSON status report into the snapshot notes, so the read-only
   overview can show details (display only; permissions never depend on
   it, [decision 0016](decisions/0016-restore-scope-from-backup-metadata.md)).
8. Write the run record (`/var/lib/nestegg/runs/<id>.json`) and update
   `state.json`. Runs that skipped something the configuration asks for
   are recorded as `incomplete`.

## Files

| Path | Content |
|---|---|
| `/etc/nestegg/config.toml` | Configuration (no secrets). Written by `nestegg init` and, for the settings of decision 0013, by the daemon (only the touched keys, atomically). |
| `/etc/nestegg/credentials/` | Encrypted credentials (`*.cred`, `systemd-creds`) or root-only plain files. |
| `/var/lib/nestegg/state.json` | Last run, last success, next run. |
| `/var/lib/nestegg/runs/` | Run history, one JSON file per run, pruned to a configurable count. |
| `/var/lib/nestegg/notify.json` | What the health check has reported to whom (policy state, can be deleted). |
| `/var/lib/nestegg/inventory/` | Generated system inventory (backed up). |
| `/run/nestegg/` | Lock file and API socket; `tmp/` (root only) holds the metadata file of a running backup, read-back copies and cancel notes. |

## Interfaces

- API contract between daemon and UI: [`api/openapi.yaml`](../api/openapi.yaml).
  It is the single source of truth; the UI never talks to the PBS directly.
- Configuration reference and setup guide: [`docs/configuration.md`](configuration.md);
  rescue stick: [`docs/rescue.md`](rescue.md).

## Repository layout

| Path | Owner | Content |
|---|---|---|
| `crates/nestegg/` | core | Rust binary: CLI, run pipeline, daemon, UI server. |
| `api/openapi.yaml` | core (changes reviewed by UI) | HTTP API contract. |
| `ui/` | UI | Web UI source; build output `ui/dist/` is embedded into the binary. |
| `packaging/` | core | systemd units, Debian packaging. |
| `docs/` | both | Decisions, architecture, research. |
