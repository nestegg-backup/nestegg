# Roadmap

Scope and reasoning: `docs/decisions/0002-own-project-narrow-scope.md`.
Each milestone is usable on its own. Details are refined before work starts.

## Optional – Hands-on comparison (not blocking)

Install BackupPilot and pbsgui on an Ubuntu or Linux Mint desktop and run
them against an isolated test namespace on a PBS (dedicated, low-privilege
API token; no retention that touches other groups). Record what works well
and what fails: first backup, scheduled run, file restore into an alternative
directory, encrypted backup and restore, behaviour while the PBS is
unreachable or asleep, desktop integration, credential storage. Results go
to `docs/research/` and feed into v0.1 design. Work on v0.1 does not wait
for this.

## v0.1 – Terminal backup for one machine

Goal: a desktop backs itself up to a PBS reliably, controlled from the terminal.

- Configuration file: PBS repository, datastore, namespace, backup sources,
  exclusions, optional Wake-on-LAN target, allowed time window.
- Credentials outside the configuration file (systemd credentials or the
  system keyring); never in logs.
- Commands: `init`, `backup`, `status`, `list`, `restore` (file or directory),
  `check` (configuration and PBS reachability).
- Scheduling via systemd timer with catch-up after the machine was off.
- PBS availability: optional Wake-on-LAN, wait with timeout and backoff,
  "PBS unreachable" reported as its own state.
- Status file with last success, last attempt, next run.
- Tests: unit tests plus an end-to-end test against a disposable PBS in a VM
  or container, on clean Ubuntu LTS and Linux Mint images.

## v0.2 – Desktop integration

- Graphical interface: status, run now, restore, notifications when a backup
  is overdue.
- `.deb` packages.

## v0.3 – Overview of several machines

- One place that shows every machine: last verified success, next run,
  overdue or offline, encryption key backed up yes/no.
- Architecture to be decided (read-only status from the PBS itself vs. a small
  central service).

## Open decisions before v0.1

- Implementation language (Rust aligns with the official Proxmox crates;
  Python is faster to start with).
- Project name and license.
- Backup scope defaults: home directory only, or whole system with
  file-level or block-level backups.
