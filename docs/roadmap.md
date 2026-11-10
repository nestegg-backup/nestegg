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

## v0.1 – Terminal backup for one machine (implemented and tested in containers, not released)

Goal: a desktop backs itself up to a PBS reliably, controlled from the terminal.

- Configuration file: PBS repository, datastore, namespace, backup sources,
  exclusions, optional Wake-on-LAN target.
- Credentials outside the configuration file (systemd credentials, with a
  root-only file as fallback); never in logs.
- Commands: `init`, `backup`, `status`, `snapshots`, `history`, `restore`
  (file or directory), `check` (configuration, PBS reachability and login;
  a check that could not be made, such as the login while the PBS is
  asleep, counts as "not checked", never as passed: exit code 0 passed,
  1 failed, 2 not everything could be checked).
- Scheduling via systemd timer with catch-up after the machine was off.
- PBS availability: optional Wake-on-LAN, wait with a timeout, the wake-up
  packet repeated with growing pauses, "PBS unreachable" reported as its own
  state.
- Status file with last success, last attempt, next run.
- Tests: unit tests plus an end-to-end test against a disposable PBS in
  containers, on clean Ubuntu 24.04 and Debian 13 images. There is no
  official Linux Mint container image; an Ubuntu base with the Linux Mint
  22.2 repositories passes the same test, a real Mint desktop has not been
  tested yet.

## v0.2 – Desktop integration (implemented, not released)

- Local web UI (`nestegg ui`): status, run now, history, restore.
  Contract: `api/openapi.yaml`.
- Notifications when backups fail, are overdue, incomplete or damaged:
  `nestegg notify` with desktop messages and an optional hook command
  (decision 0017).
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

## Not planned

- An allowed time window for backups and the desktop keyring as a credential
  store were listed for v0.1 and are dropped: see
  `docs/decisions/0020-no-time-window-no-keyring.md`.

## Decided

Language, project name, license, scope defaults and the technical baseline
are recorded in `docs/decisions/0003-product-and-technical-baseline.md` and
`docs/decisions/0004-name-license-organisation.md`.
