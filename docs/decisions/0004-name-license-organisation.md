# 0004 – Name, license, organisation

- Status: accepted
- Date: 2026-11-08

## Name: nestegg

A nest egg is money set aside for emergencies – which is what a backup is.
Command line: `nestegg backup`, `nestegg status`.

Checked on 2026-11-08: free as a crates.io crate and as a Debian/Ubuntu
package name; on GitHub only an unrelated, small WebM library uses the name.
The project name must never contain "Proxmox" (registered trademark); the
documentation may say "for Proxmox Backup Server" descriptively.

## License: AGPL-3.0-or-later

Same license as `proxmox-backup-client` and the Proxmox crates, so they can be
used directly later. The network clause fits a tool with a web interface.

## Organisation

The public repository will live in a dedicated GitHub organisation rather than
a personal account. Candidate organisation names that were free on
2026-11-08: `nestegg-backup`, `nesteggbackup` (`nestegg` itself is taken by a
user account). The public repository is created fresh in `nestegg-backup`
instead of transferring this one (decision 0035).
