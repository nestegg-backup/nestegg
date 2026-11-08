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

## v0.1 – Terminal backup for one machine (implemented, not released)

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

## v0.2 – Desktop integration (API implemented, UI in progress)

- Local web UI (`nestegg ui`): status, run now, history, restore,
  notifications when a backup is overdue. Contract: `api/openapi.yaml`.
- `.deb` packages.

## v0.2.x – Rescue stick (implemented, not released)

- Full system backup (`backup.system`, default for new installations) with
  the disk layout recorded in every snapshot.
- Bootable rescue ISO (UEFI and BIOS) with a restore wizard that recreates a
  bootable system on an empty disk (`nestegg rescue`), optional LUKS2.
- Release artifacts as the download page: `.deb` and rescue ISO with
  checksums, attached to tagged releases by CI.
- Decision: `docs/decisions/0006-bare-metal-restore-rescue-stick.md`,
  guide: `docs/rescue.md`.

## v0.3 – Overview of several machines (API implemented)

- One place that shows every machine: last verified success, next run,
  overdue or offline, encryption key backed up yes/no.
- Read-only from the PBS (Audit role) plus the status report each machine
  uploads with its snapshot; no central service (decision 0003).

## Decided

Language, project name, license, scope defaults and the technical baseline
are recorded in `docs/decisions/0003-product-and-technical-baseline.md` and
`docs/decisions/0004-name-license-organisation.md`.
