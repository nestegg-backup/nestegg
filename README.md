<img src="assets/icon.svg" width="64" height="64" alt="">

# nestegg

Unattended, encrypted backups of Linux desktops (Ubuntu, Linux Mint, Debian)
to a Proxmox Backup Server or to a USB backup stick – controllable from the
terminal and from a local web interface.

> **Status: not released.** Backup, restore, the web UI, notifications and
> the rescue stick and the backup stick are implemented and pass the
> automated tests (containers and virtual machines). It has not yet run
> against a real installation or a real USB drive; do not rely on it for your
> only backup.

nestegg does not implement any backup logic itself. It drives the official
`proxmox-backup-client`, so every backup is a normal PBS snapshot that stays
restorable without nestegg.

What it does:

- daily backups via systemd timer, with catch-up after the machine was off
- waking a sleeping backup server via Wake-on-LAN, "server unreachable" as
  its own state instead of a generic failure
- client-side encryption by default; backups only start once you have
  shown that you hold a copy of the key (file on a USB stick or paper key)
- every backup is read back from the server after the upload, and a backup
  that had to leave something out is shown as incomplete, not as fine
- a desktop notification (and optionally your own command, e.g. for a push
  message) when backups fail or have not happened for too long
- home directories, system configuration and a package inventory for a quick
  rebuild after reinstalling
- a local web UI for status, history, settings and restore, with an
  overview of the other computers of the household that back up to the same
  server
- a rescue stick (bootable ISO) that restores the whole computer onto an
  empty disk: partitions, files, boot loader ([docs/rescue.md](docs/rescue.md))
- a backup stick (USB stick or USB SSD) that works with or without a PBS:
  the desktop backs itself up onto it when it is plugged in, and booted from
  it a computer is restored onto an empty disk or backed up even when its own
  system no longer starts ([docs/stick.md](docs/stick.md))

See [docs/roadmap.md](docs/roadmap.md), [docs/architecture.md](docs/architecture.md)
and the [decision records](docs/decisions/).

## Development

```sh
cargo test                                   # unit and integration tests
cargo clippy --all-targets -- -D warnings
cargo build && tests/e2e/run.sh              # real PBS + client in containers
cargo deb -p nestegg && tests/e2e/systemd.sh # package + systemd services
tests/e2e/hub-join.sh target/debian/nestegg_<version>_amd64.deb # real one-command hub onboarding
tests/e2e/upgrade.sh old.deb new.deb         # package upgrade, remove, purge
```

The end-to-end scripts start a disposable Proxmox Backup Server from the
official packages in Docker; nothing outside the containers is touched.
`systemd.sh`, `hub-join.sh` and `upgrade.sh` need the host cgroup namespace and run their
systemd container with `CAP_SYS_ADMIN` and without seccomp/AppArmor
confinement (so the units' sandboxing really applies); only run them on a
disposable machine or CI runner. `upgrade.sh` installs the old package, backs
up, installs the new one over it and checks that setup and history survive;
the old package needs a lower version (CI builds it from the merge base with
`cargo deb --deb-version <version>~base`). Setup and configuration:
[docs/configuration.md](docs/configuration.md).

"Proxmox" is a registered trademark of Proxmox Server Solutions GmbH. nestegg
is an independent project and not affiliated with or endorsed by Proxmox.

## License

[AGPL-3.0-or-later](LICENSE)
