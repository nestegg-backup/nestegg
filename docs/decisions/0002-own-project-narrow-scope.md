# 0002 – Own project with a narrow, complementary scope

- Status: accepted
- Date: 2026-11-08

## Context

The landscape review in `docs/research/existing-projects.md` found several
young projects that back up Linux desktops to a Proxmox Backup Server (PBS).
BackupPilot comes closest for a single machine; PBS Plus, PBCM and PBC Manager
offer central overviews but no desktop client. No project combines local
control on each desktop with a household-wide overview, and none handles a
PBS that is asleep or only online in a time window.

Contributing to BackupPilot was considered. It was rejected because the
multi-machine overview needs a central component that affects the basic
architecture, and BackupPilot is designed around a single GNOME desktop.

## Decision

1. Build an own project instead of extending an existing one.
2. Keep the scope narrow and complementary. The project does **not** implement
   backup logic. `proxmox-backup-client` (or the official Proxmox Rust crates)
   does chunking, deduplication, encryption, transfer and restore.
3. The project adds what is missing elsewhere:
   - scheduling and catch-up runs on desktops that are not always on,
   - waking a sleeping PBS (Wake-on-LAN, optional) and treating "PBS was
     unreachable" as its own state,
   - control from the terminal and from a graphical interface,
   - an overview of several machines,
   - simple restore, packaging, and tests on Ubuntu and Linux Mint.
4. Everything is generic and configurable. A maintainer's own setup is one
   configuration among many, never a hard-coded assumption.
5. Backups stay standard PBS snapshots. They must remain restorable with plain
   `proxmox-backup-client`, without this project (no lock-in).
6. Initial target: Debian-based distributions (Ubuntu, Linux Mint), because
   the official client is packaged for them. Other distributions later.
7. Scheduling is a replaceable layer. Proxmox has proposed "scheduled backup
   jobs" for the official client (patch series 2026-10-14). If it is released,
   the project should delegate scheduling to it.

## Consequences

- The project starts with one maintainer. Long-term maintenance, not the
  initial build, is the main cost. Automated tests on clean Ubuntu and Mint VMs
  and clear documentation are required from the first release.
- Good ideas from BackupPilot, pbsgui and others may be adopted. Copying code
  requires checking license compatibility (several are AGPL-3.0).
- The first release covers the terminal workflow only; the graphical interface
  and the multi-machine overview follow (see `docs/roadmap.md`).
