# Existing projects: Linux desktops → Proxmox Backup Server

Snapshot of the landscape as of **2026-11-08**. Facts were taken from project
READMEs, source files, releases and package indexes (GitHub API, Forgejo API,
Codeberg). Two independent research passes were compared. **Nothing was
installed or tested**; "has feature X" means documented or visible in code,
not proven in daily use.

## Short answer

- The official `proxmox-backup-client` already covers the backup engine
  (file and block backups, encryption, catalog, mount, restore, prune).
- Several young community projects already wrap it with **CLI + desktop GUI**
  for a single machine. The closest is **BackupPilot**.
- Several other projects offer a **central web overview of many machines**
  (PBS Plus, PBCM, PBC Manager), but without a desktop GUI and with gaps in
  restore.
- **No project combines both** – local control on each desktop plus a simple
  household-wide status overview – in a mature, tested form.
- Proxmox itself posted a patch series *"client: add scheduled backup jobs"*
  (2026-10-14) that adds job configuration, status and a systemd user
  scheduler daemon to the official client. Not merged/released as of client
  4.2.10. If it lands, scheduling should be delegated to it.

## Desktop clients (single machine, CLI and/or GUI)

| Project | License | Stack | Last release | CLI | GUI | PBS access | Notes |
|---|---|---|---|---|---|---|---|
| [BackupPilot](https://git.onesystems.ch/BackupPilot/App) | AGPL-3.0-or-later | Rust, GTK4/libadwaita, D-Bus daemon | 1.2.4 (2026-10-10) | yes | desktop + tray | native PBS 4.2 crates, official client as fallback | systemd timers, overdue warnings, file browser/restore, encryption, deb/rpm/Flatpak. ~4 months old, one main developer, no multi-machine view. GitHub mirror is outdated. |
| [pbsgui / pbstui](https://github.com/nienhausdavid/pbsgui) | AGPL-3.0 | Python, PySide6, Textual | 0.9.2 (2026-10-24) | yes | desktop + TUI | wraps official client | systemd timers, dashboard, rescue ISO. Self-declared prototype; prune missing; system scope only unit-tested. |
| [pbs-desk](https://github.com/Bu66as/pbs-desk) | GPL-3.0 | Python, PySide6 | 0.9.2 (2026-11-07) | scripts | tray | wraps official client | Fedora/KDE focus, install script only, encrypted restore planned. |
| [Salvanza (proxmoxbackupclient_go)](https://github.com/tizbac/proxmoxbackupclient_go) | GPL-3.0 | Go, Wails | 1.2.0-rc.1 (2026-10-30) | yes | desktop (Wails) | own Go implementation of the PBS protocol | Most community traction (~475 stars) but Windows focus; no packaged Linux GUI. |
| [PBS Browser](https://codeberg.org/DuncanRobertson/pbs-browser) | GPL-2.0 (per LICENSE) | Python, PySide6 | 0.02-alpha | no | desktop | official client | Restore/browse only, no backups. |
| [proxmox-backup-gui](https://github.com/masterjuggler/proxmox-backup-gui) | GPL-3.0 | Python, PyQt6 | 1.0.0 (2024-11) | no | desktop | official client | Single script, inactive. |
| [PBSClientTool](https://github.com/cauldr0nx/PBSClientTool) | MIT per README, no LICENSE file | Bash | none | interactive | no | official client | Installer + systemd timer helper; inactive since 2025-11. |

## Central / multi-machine managers

| Project | License | Model | Overview | Restore | Notes |
|---|---|---|---|---|---|
| [PBS Plus](https://github.com/pbs-plus/pbs-plus) | MIT | Server extension on the PBS host; agent filesystem pulled via FUSE, official client runs on the server | yes (inside PBS web UI) | path restore to agent | Extra services/ports, modifies the PBS UI, must be stopped for PBS upgrades. Different architecture from a self-contained desktop client. |
| [Proxmox Backup Client Manager (PBCM)](https://github.com/stefgo/proxmox-backup-client-manager) | MIT | Central web server + Node.js agents running the official client | yes | whole archive only | No full CLI, no deb packages, no file browser inside backups. |
| [PBC Manager](https://github.com/bradyloveland/pbcmanager) | MIT | Web UI, setup via SSH, local systemd timers run the official client | yes | not found | Good local-timer model, restore missing. |

## Scripts, containers, packaging (no GUI, no overview)

zpbs-backup (ZFS-driven, deb/rpm), pbs-client-docker, pbs-linux-backup (LVM
snapshots), proxmox-backup-atomic (ZFS), pbs-wrapper, pbc-wrapper,
pbs-backup-client-script, an Ansible role, and unofficial client builds for
Fedora/RHEL/Arch/Alpine/openSUSE (e.g. `rdemsystems/unofficial-proxmox-backup-client`,
`ciroiriarte/pbs-client`). Several repositories have **no license** and are
therefore not reusable.

## Gaps a new or extended project could fill

1. **Local control + household overview in one product**: each desktop backs
   up on its own (CLI, GUI, timer), and one place shows per machine: last
   verified success, next run, overdue/offline, key escrow status.
2. **Backup server availability**: PBS instances that sleep or are only up in
   a time window (Wake-on-LAN, wake hook, retry/backoff, "missed because PBS
   was down" as a distinct state). Not handled by any desktop client found.
3. **Restore from the overview**: browse and restore files of any machine,
   not only whole archives.
4. **Retention transparency**: which server-side prune rules apply, what will
   be kept, last GC/verify.
5. **Tested desktop integration** on Linux Mint (Cinnamon/XFCE) and Ubuntu,
   user vs. system scope, credentials in the system keyring or systemd
   credentials, consistent sources (Btrfs/LVM snapshots), separate mount
   points (`/home`, `/boot/efi`).
6. **Proven recovery**: documented and tested restore onto replacement
   hardware, including encrypted backups.

## Implications for this project

- Do not reimplement the PBS protocol, chunking or encryption.
- Build on the official `proxmox-backup-client` (or official Rust crates);
  watch the upstream "scheduled backup jobs" series and adopt it when released.
- Contributing to BackupPilot (closest match, AGPL) was considered and
  rejected in favour of an own project with a narrow scope; see
  `docs/decisions/0002-own-project-narrow-scope.md`.
- License compatibility matters if code is shared: BackupPilot and pbsgui are
  AGPL-3.0, the managers are MIT.

## Sources

- Official client docs: <https://pbs.proxmox.com/docs/backup-client.html>
- PBS roadmap: <https://pbs.proxmox.com/wiki/Roadmap>
- Patch series "client: add scheduled backup jobs" (2026-10-14):
  <https://lore.proxmox.com/all/20261014105208.2747208-1-t.lamprecht@proxmox.com/>
- BackupPilot forum thread: <https://forum.proxmox.com/threads/backuppilot-desktop-backup-client-f%C3%BCr-proxmox-backup-server.183740/>
- pbs-desk forum thread: <https://forum.proxmox.com/threads/pbs-desk-open-source-desktop-backup-client-for-pbs-linux-early-windows-support.187416/>
- Project links in the tables above.
