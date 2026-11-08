<img src="assets/icon.svg" width="64" height="64" alt="">

# nestegg

Unattended, encrypted backups of Linux desktops (Ubuntu, Linux Mint, Debian)
to a Proxmox Backup Server – controllable from the terminal and from a local
web interface.

> **Status: early development.** Nothing here is ready for real backups yet.

nestegg does not implement any backup logic itself. It drives the official
`proxmox-backup-client`, so every backup is a normal PBS snapshot that stays
restorable without nestegg.

Planned for the first releases:

- daily backups via systemd timer, with catch-up after the machine was off
- waking a sleeping backup server via Wake-on-LAN, "server unreachable" as
  its own state instead of a generic failure
- client-side encryption by default, with a mandatory key export
- home directories, system configuration and a package inventory for a quick
  rebuild after reinstalling
- a local web UI for status, history and restore; later an overview of all
  machines in a household
- a rescue stick (bootable ISO) that restores the whole computer onto an
  empty disk: partitions, files, boot loader ([docs/rescue.md](docs/rescue.md))

See [docs/roadmap.md](docs/roadmap.md), [docs/architecture.md](docs/architecture.md)
and the [decision records](docs/decisions/).

## Development

```sh
cargo test                                   # unit and integration tests
cargo clippy --all-targets -- -D warnings
cargo build && tests/e2e/run.sh              # real PBS + client in containers
cargo deb -p nestegg && tests/e2e/systemd.sh # package + systemd services
```

The end-to-end scripts start a disposable Proxmox Backup Server from the
official packages in Docker; nothing outside the containers is touched.
`systemd.sh` needs the host cgroup namespace and should only run on a
disposable machine or CI runner. Setup and configuration:
[docs/configuration.md](docs/configuration.md).

"Proxmox" is a registered trademark of Proxmox Server Solutions GmbH. nestegg
is an independent project and not affiliated with or endorsed by Proxmox.

## License

[AGPL-3.0-or-later](LICENSE)
