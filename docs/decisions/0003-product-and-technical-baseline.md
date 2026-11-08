# 0003 – Product and technical baseline

- Status: accepted
- Date: 2026-11-08

## Decisions

| Topic | Decision | Reason |
|---|---|---|
| Backup scope (v0.1) | Home directories, `/etc`, `/root`, `/usr/local`, plus a generated system inventory (package lists, enabled services, disk layout). File-level. | Covers "reinstall the distribution, then restore everything" with little effort. Bare-metal restore and disk images come later, if at all. |
| User interface | Local web UI in the browser, served by the `nestegg ui` command on `127.0.0.1` only. A tray indicator may follow. | Looks the same on Cinnamon, GNOME, KDE; the same UI later becomes the multi-machine overview. |
| Language | Rust (single static-ish binary). | Same ecosystem as Proxmox, robust for a root service, easy `.deb` packaging, official Proxmox crates usable later. |
| Supported systems | Ubuntu 24.04 / 26.04 LTS, Linux Mint 22.x and successors, Debian 13. | All Debian-based; the official client is available for them. Tested on clean VMs. |
| Encryption | Client-side encryption on by default. Setup forces a key export (paper key / file) before the first backup. | The PBS operator or a stolen PBS disk must not expose personal data. |
| Execution | System service running as root (systemd timer + oneshot service). Control via a Unix socket restricted to the `nestegg` group. | Needed for `/etc` and all users; runs without anyone logged in. |
| Multi-machine overview | Read-only from the PBS (Audit role): last snapshot per machine, plus a small status report uploaded with each snapshot. No central service. | Nothing extra to host, secure, or update. |
| Credentials | systemd credentials (`systemd-creds`, TPM-bound when available) passed to `proxmox-backup-client` via its documented `proxmox-backup-client.*` credential names; fallback to root-only files. | Secrets encrypted at rest, never in config files, process arguments, or logs. |

## Out of scope for now

- Own implementation of the PBS protocol, chunking or encryption.
- Non-Debian distributions (possible later via the static client).
- Disk images and bare-metal restore.
