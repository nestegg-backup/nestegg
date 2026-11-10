# 0012 – Removing the package keeps the setup and the enabled units

- Status: accepted
- Date: 2026-11-09

## Context

`apt remove` and `apt purge` already kept `/etc/nestegg/` and
`/var/lib/nestegg/` on purpose (`postrm`): the encryption key there may be the
only copy, and losing it makes every backup on the PBS unreadable.

The units were not treated the same way. `prerm` disabled the daemon and the
timer on removal, and `postinst` never enables anything, because a first
installation is not set up yet. After removing and installing the package
again, the configuration was complete but no backups ran, with no hint. The
package upgrade test (`tests/e2e/upgrade.sh`) showed this.

Debian's `deb-systemd-helper` keeps such state for packages that enable their
units at installation. nestegg does not do that (the user enables the units
after `nestegg init`), and the helper forgets the state on purge, while
nestegg's setup survives purge.

## Decision

- On removal, `prerm` writes the enabled units among `nestegg.service` and
  `nestegg-backup.timer` to `/var/lib/nestegg/enabled-units` before disabling
  them.
- `postinst` enables the units listed there (and starts them when systemd
  is running), then deletes the file. It accepts only those two unit names,
  and it enables nothing when `/etc/nestegg/config.toml` no longer exists.
  Restarting a running daemon on upgrade does not depend on the file.
- `preinst` deletes the file on an upgrade of an installed package. There it
  can only be stale (for example left by an older version that was installed
  after the removal), and acting on it would re-enable a unit the user has
  disabled since.
- Purge keeps this file like the rest of the state. Installing again after a
  purge therefore also resumes backups.
- The user documentation (`docs/configuration.md`, "Removing the package")
  says what remove and purge keep and how to delete everything.

## Consequences

- Remove, then install, resumes backups exactly as configured before; a unit
  the user had disabled stays disabled.
- Users who want nestegg gone completely have to delete the two directories
  themselves. This is deliberate and documented.
- `tests/e2e/upgrade.sh` checks re-enabling after remove and purge, a unit
  disabled before removal, and a stale file on upgrade.
