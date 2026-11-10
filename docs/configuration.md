# Configuration and setup

## Quick start

On the PBS (once per machine, in the PBS web UI or shell):

1. Optional: create a namespace, e.g. `desktops`.
2. Create a user (e.g. `backup@pbs`) and an API token for this machine
   (e.g. `backup@pbs!desktop`). Copy the token secret.
3. Give **both** the user and the token the role `DatastoreBackup` on
   `/datastore/<datastore>/<namespace>` (or `/datastore/<datastore>`).
   This allows creating and restoring the machine's own backups, but not
   deleting them. Pruning stays a server-side job.

On the desktop:

```sh
sudo apt install proxmox-backup-client-static # see "Installing the backup client" below
sudo apt install ./nestegg_<version>_amd64.deb # the file: see "Getting the package" below the box

sudo nestegg init --server pbs.example.com --datastore backups \
     --namespace desktops --auth-id 'backup@pbs!desktop' \
     [--fingerprint <SHA-256>] [--wake-mac 00:00:5e:00:53:01]
sudo nestegg credentials set-password     # paste the token secret
sudo nestegg key create                   # reinstalling? keep the old key: sudo nestegg key import <file>
sudo nestegg key paperkey --format html --output key.html   # print it, then delete the file
                                          # - or: sudo nestegg key export /media/usb/nestegg-key.json
sudo nestegg key confirm-exported --paper-code <code on the paper>
                                          # - or: --file /media/usb/nestegg-key.json
sudo nestegg check
sudo systemctl enable --now nestegg.service nestegg-backup.timer nestegg-notify.timer
sudo usermod -aG nestegg "$USER"          # log out and in, then: nestegg ui
```

The paper key can be saved with `--output paperkey.txt` and is accepted by the
rescue stick as a file or typed in (see `docs/rescue.md`).

The fingerprint is only needed when the PBS uses its default self-signed
certificate. It is shown on the PBS dashboard ("Show Fingerprint").

Then make the first backup right away instead of waiting for the timer: see
"The first backup" below.

**Getting the package.** The `.deb` file comes from the release page of the
repository once a release exists. Until then, build it yourself on a machine
with the Rust and Node.js toolchains (see `README.md`; `cargo install
cargo-deb` once): `(cd ui && npm ci && npm run build) && cargo build --release
&& cargo deb -p nestegg --no-build`. The package is written to `target/debian/`.

## Installing the backup client

nestegg drives the official `proxmox-backup-client`. Ubuntu and Linux Mint do
not ship it, and Debian only has it in the Proxmox repositories, so `apt
install proxmox-backup-client` fails on a fresh system with "Unable to locate
package". Add the Proxmox client repository once:

```sh
# 1. The key Proxmox signs its packages with
sudo curl -fsSL -o /usr/share/keyrings/proxmox-archive-keyring.gpg \
     https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg
sha256sum /usr/share/keyrings/proxmox-archive-keyring.gpg

# 2. The repository (no subscription needed for the client)
printf '%s\n' 'Types: deb' 'URIs: http://download.proxmox.com/debian/pbs-client' \
     'Suites: trixie' 'Components: main' \
     'Signed-By: /usr/share/keyrings/proxmox-archive-keyring.gpg' \
  | sudo tee /etc/apt/sources.list.d/pbs-client.sources

# 3. The client
sudo apt update
sudo apt install proxmox-backup-client-static
proxmox-backup-client version
```

- Before you go on, compare the checksum printed in step 1 with the one in
  the Proxmox Backup installation documentation (section "SecureApt"). When
  this was written both that page and nestegg's own tests had
  `136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45`; the
  Proxmox page is the authority if the key is ever replaced.
- `Suites: trixie` is right on Ubuntu and Linux Mint too. The package
  `proxmox-backup-client-static` is one statically linked program without
  dependencies on the distribution, which is why the Debian 13 build runs on
  Ubuntu 24.04 and Linux Mint 22. This combination is what the end-to-end
  tests of this project install (Ubuntu 24.04, Debian 13, and an Ubuntu
  24.04 base with the Linux Mint 22.2 repositories).
- On Debian 13 the dynamically linked `proxmox-backup-client` from the same
  repository works as well; the nestegg package accepts either.
- Installing the nestegg package first is harmless: it only recommends the
  client, and `sudo nestegg check` says so when the client is missing.

## The first backup

`nestegg-backup.timer` checks once an hour whether a backup is due, so after
the setup the first backup would start by itself within the hour. Start it
now and watch it, so that problems show while you are still at the computer:

```sh
sudo nestegg backup            # or "Back up now" in the web UI (nestegg ui)
nestegg status                 # should say that the last backup worked
sudo nestegg key verify /media/usb/nestegg-key.json   # your copy opens that backup
```

- The first backup uploads everything and can take hours; later ones only
  upload what changed. The computer can be used meanwhile.
- After the upload nestegg reads the snapshot back from the server and
  compares it (decision 0015). `nestegg status` shows "incomplete" with the
  reason if something that is configured could not be backed up, for example
  the full system backup on a file system the rescue stick cannot recreate.
- Try one restore before you rely on it: `nestegg ui`, "Restore files", pick
  a small file and restore it into a new folder. With a full system backup
  also write the rescue stick once and boot it up to the point where it
  lists the backup (`docs/rescue.md`); nothing is changed before you confirm
  the final plan.
- `sudo nestegg notify --test` shows whether warnings about failed or
  missing backups would reach you (section "Notifications").

## `/etc/nestegg/config.toml`

The file never contains secrets. Unknown keys are rejected.

```toml
# Backup ID on the PBS (default: host name)
backup_id = "desktop"
# Dashboard shows "overdue" after this many hours without success
overdue_after_hours = 36

[pbs]
server = "pbs.example.com"      # host name or IP, no port
port = 8007
datastore = "backups"
namespace = "desktops"          # optional
auth_id = "backup@pbs!desktop"  # user@realm or user@realm!token
fingerprint = "<SHA-256>"       # optional, for self-signed certificates

# Optional: wake a sleeping PBS before each run
[wake]
mac = "00:00:5e:00:53:01"
broadcast = "255.255.255.255"   # or the subnet broadcast, e.g. 192.0.2.255
port = 9
timeout_seconds = 180
enabled = true                  # false: keep the MAC, but do not wake

[backup]
encryption = true               # client-side encryption (recommended)
inventory = true                # package lists, enabled units, disk layout
system = true                   # whole system as system.pxar (rescue stick)
# change_detection_mode = "metadata"   # legacy (default) | data | metadata
# rate_limit = "20MiB"
keep_runs = 200                 # local run history
include_dev = []                # extra mount points, e.g. ["/boot/efi"]
sources = [
  { archive = "home", path = "/home" },
  # { archive = "data", path = "/srv/data", require_mount = true },
  { archive = "etc", path = "/etc" },
  { archive = "root", path = "/root", optional = true },
  { archive = "usr-local", path = "/usr/local", optional = true },
]
excludes = ["**/.cache", "**/node_modules", "**/.local/share/Trash"]

[schedule]
interval_hours = 24

# Optional: how problems are reported (see "Notifications"). Root-only; the
# web UI cannot change it.
# [notify]
# desktop = true                          # notification for logged-in users
# command = ["/usr/local/bin/my-hook"]    # runs as root, no shell

# Optional: read-only token for the overview of all machines in the
# namespace (role DatastoreAudit or DatastoreReader on the namespace).
# Store its secret with: sudo nestegg credentials set-password --overview
# [overview]
# auth_id = "overview@pbs!desktop"
```

Notes:

- **Full system backup** (`system = true`, set by `nestegg init` unless
  `--no-system` is given): `/` is backed up as `system.pxar` together with the
  system's own mounts (`/home`, `/boot`, `/boot/efi`, `/var`, btrfs
  subvolumes of the root device, …). Sources that lie on these file systems
  become part of it; other sources stay separate archives: data disks,
  network shares and FUSE mounts, bind mounts, and every source with
  `require_mount = true`. `/tmp`, `/var/tmp`, downloaded packages and swap
  files are excluded (an exclusion is dropped, with a note in the log, when
  another source has the same path, because exclusions apply to every
  archive). In system mode only the system's own fstab entries must be
  mounted; a missing USB or network disk does not fail `system.pxar`. The
  disk layout is stored as `layout.json` in `inventory.pxar`. Only with this
  option can the rescue stick restore the computer onto an empty disk
  (`docs/rescue.md`). Unprivileged users can restore their own home
  directory from `system.pxar` in the web UI, like from `home.pxar`.

- **Mount points are not crossed.** If `/home` is a separate file system it
  is still backed up, because it is its own source. Other mount points below
  a source need an own source entry or `include_dev`.
- **Missing sources fail the run** unless marked `optional = true`. A missing
  data disk must not silently produce an incomplete "successful" backup.
  A missing optional source is skipped, but the backup is then reported as
  **incomplete** (health `incomplete`, reasons in `nestegg status`); the
  same applies when system mode is on but `system.pxar` had to be skipped,
  and when the inventory could not be written
  ([decision 0015](decisions/0015-complete-and-verified-backups.md)).
- **Every snapshot is read back.** After the upload the run checks that all
  archives are on the server, restores the small metadata blob
  (`nestegg-meta.conf`) and the inventory with the configured key and
  compares them with what was uploaded. A mismatch, or a server that stops
  answering during the check, fails the run (`verification_failed`).
  Snapshots that the server's own verification jobs marked as failed are
  listed by `nestegg status` ("Server check").
- **Unmounted disks fail the run.** If `/etc/fstab` lists a file system at or
  below a source (without `noauto`) and it is not mounted, the run fails
  instead of backing up the empty mount point. `require_mount = true` makes
  the source itself mandatory as a mount point, also without an fstab entry.
  Separate file systems mounted below a source that are not backed up (no
  own source, not in `include_dev`) are reported as warnings.
- **Excludes** use the `proxmox-backup-client --exclude` syntax
  (gitignore-like). Excluded directories are not read at all.
- **Schedule:** the timer checks hourly; a backup runs when the last success
  is older than `interval_hours`. If the PBS is unreachable, the run ends
  with state `pbs_unreachable` and the next hourly check tries again.
  A failed backup is retried at the next check; after two or more failures
  in a row the timer waits 2, 4, 8, 16 hours (at most `interval_hours`)
  before it tries again, so a problem that does not go away by itself does
  not wake the server every hour. Changing the configuration, storing
  credentials or a key, and every manual start (`sudo nestegg backup`, "Back
  up now") end that pause (decision 0021).
- **Wake-on-LAN:** with `[wake]` (and `enabled = true`, the default), a
  run that finds the PBS down sends the
  magic packet, repeats it after 10, 20 and 40 seconds and then every minute,
  and waits up to `timeout_seconds`.
  If the packet cannot be sent at all (for example a laptop that is offline
  or still reconnecting after sleep), the run logs a warning, keeps trying
  until the timeout and then also ends as `pbs_unreachable`, not as a
  failure.

## Web interface and API permissions

`nestegg ui` (run as a member of group `nestegg`) talks to the local daemon.
The daemon identifies the caller by the socket credentials:

- Everyone in the group can see the status, start and cancel backups and
  run the connection check. Who cancelled a backup is logged
  (`journalctl -u nestegg.service`) and named in the run record.
- Everyone in the group can change these settings in the web UI (or with
  `PATCH /api/v1/config`):

  | Setting | Key in `config.toml` | Allowed |
  |---|---|---|
  | Backup rhythm | `schedule.interval_hours` | 1 to 168 hours |
  | "Overdue" after | `overdue_after_hours` | 2 to 744 hours, more than the rhythm |
  | Excludes | `backup.excludes` | up to 200 patterns, none that excludes everything (`*`, `**`, `/`, ...) |
  | Wake-on-LAN on/off | `wake.enabled` | only with a `[wake]` section; switching off keeps the MAC |
  | Automatic backups on/off | `nestegg-backup.timer` | `systemctl enable/disable --now` |

  The daemon changes only these keys in `/etc/nestegg/config.toml` and
  keeps your comments and layout. Each change is logged with the user who
  made it: `journalctl -u nestegg.service`. Changes made during a backup
  apply from the next run on.
- Everything else stays terminal-only (root): server, port, datastore,
  namespace, auth id, fingerprint, backup id, credentials, encryption and
  the key, sources, the `[wake]` details, the overview credentials and the
  `[notify]` section (its command runs as root).
  Anyone who could change these could send the backups of this computer to
  another server ([decision 0013](decisions/0013-editable-settings-in-the-web-ui.md)).
  Edit the file with `sudo`, then run `sudo nestegg check`.
- Browsing and restoring is limited to the caller's own home directory
  (other homes and archives such as `/etc` are refused). Restored files are
  written with the caller's identity into a new directory. Which part of
  an archive is the caller's home comes from the metadata blob inside the
  snapshot or, for snapshots of older versions, from the local run record;
  never from the snapshot notes, which others can change
  ([decision 0016](decisions/0016-restore-scope-from-backup-metadata.md)).
  Snapshots with neither can only be restored by root. With encryption off
  the blob is not signed; see the decision for what that means.
- root (`sudo nestegg restore …`) may restore everything.

The overview of all machines reads each machine's last snapshot and the
status report stored in its notes (display only). Without `[overview]` credentials only the
machine's own backups are visible.

## Notifications

Nobody should have to open a dashboard to learn that backups stopped.
`nestegg-notify.timer` runs `nestegg notify` every hour (and the backup unit
runs it right after each backup, so a failure is usually reported within
seconds).
The package enables the timer; it does nothing while nestegg is not set up.
Switch it off with `sudo systemctl disable --now nestegg-notify.timer`.

| Message | When | Repeated |
|---|---|---|
| `failing` – "Backup failed" | The last backup failed. The message names the reason. | every 24 h |
| `overdue` – "Backups are overdue" | No successful backup within `overdue_after_hours` (this includes a server that stays unreachable, and a setup that never succeeded). | every 24 h; every 7 days while automatic backups are switched off |
| `incomplete` – "Backup is incomplete" | The last backup worked but left something out (for example the full system backup was skipped). | every 7 days |
| `damaged` – "Damaged backups on the server" | The server's own verification reports damaged snapshots of this computer. | every 24 h |
| `recovered` – "Backups work again" | Everything is fine again after one of the above was reported. | once |

A different problem (another error, another kind) is reported at once. Each
message goes to every logged-in user and, if configured, to your command;
someone who logs in later still gets it once. Nothing is reported while a
backup is running. An unreachable server alone is not reported on every
attempt, only when it becomes `overdue`.
A configuration file that cannot be read (a typo, an unknown key) is reported
as `failing` with the parser's message; the `[notify]` section is still
honoured in that case, if it is valid.

**Desktop** (`[notify] desktop = true`, the default): every logged-in user
with a session bus gets a notification (`notify-send` from the package
`libnotify-bin`, run as that user). `failing`, `overdue` and `damaged` are
marked critical. The texts are German when the system language is German
(`LANG=de…` in `/etc/default/locale` or `/etc/locale.conf`), otherwise
English. No `notify-send`, no session: nothing is sent and the check logs it
(`journalctl -u nestegg-notify.service`).

**Command** (`[notify] command`, off by default): a program with arguments
(a list, no shell), run as root with a 60 second limit, for every message:

| Variable | Content |
|---|---|
| `NESTEGG_EVENT` | `failing`, `overdue`, `incomplete`, `damaged`, `recovered` or `test` |
| `NESTEGG_TITLE`, `NESTEGG_MESSAGE` | The short title and the text (also on stdin) |
| `NESTEGG_HOSTNAME` | Host name of this computer |
| `NESTEGG_LAST_SUCCESS` | Time of the last successful backup (RFC 3339), or empty |

If the command fails or does not finish, the check logs it and tries again
an hour later; the desktop message is not held back. Example with a push
service (ntfy; the host name is a placeholder):

```toml
[notify]
command = ["/usr/local/bin/nestegg-ntfy"]
```

```sh
#!/bin/sh
# /usr/local/bin/nestegg-ntfy  (chmod 755)
case "$NESTEGG_EVENT" in
  failing|overdue|damaged) priority=high ;;
  *) priority=default ;;
esac
{ printf '%s: %s\n\n' "$NESTEGG_HOSTNAME" "$NESTEGG_TITLE"; cat; } |
  curl -fsS --max-time 30 -H "Title: nestegg" -H "Priority: $priority" \
       --data-binary @- https://ntfy.example.com/my-backups
```

By mail (needs a working `mail` command and a mail setup that delivers
outside this computer; the address is a placeholder):

```toml
[notify]
command = ["/usr/local/bin/nestegg-mail"]
```

```sh
#!/bin/sh
# /usr/local/bin/nestegg-mail  (chmod 755)
exec mail -s "$NESTEGG_HOSTNAME: $NESTEGG_TITLE" admin@example.com
```

Try it, and see what would be sent right now, with:

```sh
sudo nestegg notify --test       # a test message through all channels
sudo nestegg notify --dry-run    # what the check would send; changes nothing
```

The command can not be set from the web UI, because it runs as root
([decision 0017](decisions/0017-notifications.md)).

## Secrets

Stored in `/etc/nestegg/credentials/` (mode 0700):

| File | Content |
|---|---|
| `password.cred` / `password` | Password or API token secret |
| `overview-password.cred` / `overview-password` | Secret of the optional read-only overview token |
| `encryption-key.cred` / `encryption-key` | Encryption key (`proxmox-backup-client` key file without password) |

`*.cred` files are encrypted with `systemd-creds` (systemd 250 or newer). They
are sealed to this computer's TPM when it has one, plus systemd's host key
(`/var/lib/systemd/credential.secret`), but not to any PCR: Secure Boot
changes and firmware updates do not make them unreadable
([decision 0008](decisions/0008-credentials-without-pcr-binding.md)). If the
TPM is present but not usable, the host key alone is used. Without
`systemd-creds` (e.g. Ubuntu 22.04 / Linux Mint 21 with systemd 249), pass
`--allow-plain` to store a root-only file instead. Secrets reach
`proxmox-backup-client` through inherited file descriptors, never via command
line arguments, environment values or logs.

Credentials stored by an earlier development build were bound to PCR 7 (the
Secure Boot state). Store them again once, so they use the new sealing:

```sh
sudo nestegg credentials set-password            # and --overview, if used
sudo nestegg key import /media/usb/nestegg-key.json
```

**The encryption key is the only way to read encrypted backups.** If it only
exists on this computer and the disk fails, the backups are lost. That is why
backups refuse to run until `nestegg key confirm-exported` was executed with
proof that a copy exists (below). The reasoning is in
[decisions/0018](decisions/0018-key-export-must-be-proven.md).

### Exporting and proving the key

Two ways to keep a copy; use at least one, better both:

- **Paper key:** `sudo nestegg key paperkey --format html --output key.html`,
  print the file, delete it. The sheet shows the computer, date, server,
  datastore, the key fingerprint, a **check code** (`XXXX-XXXX`) and how to
  restore with it, then the key and a QR code. Without `--output` the command
  refuses to write into a terminal; piping to a printer
  (`nestegg key paperkey | lpr`) works. The check code appears in the paper
  key output only; no other command, log or page shows it.
- **Key file:** `sudo nestegg key export /media/usb/nestegg-key.json`. The
  file is read back and compared with the key. If it lies on a disk of this
  computer you get a warning: that copy is lost together with the disk and
  useless inside its own backup (which is encrypted with this key). Move it to
  a USB stick or a password manager.

Then prove it. `confirm-exported` needs exactly one of:

```sh
sudo nestegg key confirm-exported --paper-code 1A2B-3C4D    # from the sheet
sudo nestegg key confirm-exported --file /media/usb/nestegg-key.json
```

`--file` takes the key file or a saved paper key (text or HTML); the key
material is compared, not the bytes. A file on a disk of this computer is
only accepted after you state that another copy exists outside it: type `yes`
when asked, or pass `--accept-local-copy` without a terminal. A file on
removable media needs neither. With a terminal and no option the command asks
what you have. A wrong file or code is an error and nothing is recorded.
Keys confirmed by an earlier version stay confirmed.

Check a copy again at any time, for example after a year:

```sh
sudo nestegg key verify /media/usb/nestegg-key.json
```

It compares the copy with the stored key and, when the PBS answers and a
snapshot exists, opens the newest snapshot with the copy. Exit code 1 if the
copy does not match or does not open the backup; a PBS that is asleep or no
snapshot yet is reported as "not checked".

### When the stored secrets cannot be decrypted

A new mainboard (new TPM), a TPM reset in the firmware settings, or a restore
onto other hardware makes the `.cred` files unreadable. Backups then fail with
"stored secrets unreadable" (`credentials_unreadable`). Nothing on the PBS is
lost; store the secrets again:

```sh
sudo nestegg credentials set-password                  # token secret
sudo nestegg credentials set-password --overview       # only with [overview]
sudo nestegg key import /media/usb/nestegg-key.json    # the exported key file
```

`nestegg key import` checks the file with `proxmox-backup-client key show`
and stores it like `key create` does. A key imported from a USB stick or
another removable medium counts as exported, because you hold that copy. A
file on this computer's own disk does not: confirm it afterwards with
`sudo nestegg key confirm-exported` like any other copy. Importing the key
that is already stored (new hardware) keeps the confirmation you gave
before. It only accepts keys without a password (as written
by `nestegg key export`). Importing the key whose fingerprint is already
recorded needs no `--force`; a different key needs `--force`, because backups
made with the current key then need its exported copy to be restored.

## Limitations

- The daemon, the timer runs and the CLI must run in the same PID namespace
  (normal desktop installation). Running some of them inside a container
  that shares `/run/nestegg` and `/var/lib/nestegg` with the host is not
  supported: "is a backup running?" is detected via `/proc/locks`, which only
  shows locks of the own PID namespace. A second backup can still never run
  in parallel (exclusive lock), but status display may be wrong.
- Staged restores into user directories need temporary space for a second
  copy (see Restore).

## Files

| Path | Content |
|---|---|
| `/var/lib/nestegg/state.json` | Last run, last success, key status |
| `/var/lib/nestegg/notify.json` | What `nestegg notify` has already reported to whom (can be deleted) |
| `/var/lib/nestegg/runs/` | Run history (`<id>.json`) and logs (`<id>.log`) |
| `/var/lib/nestegg/inventory/` | Generated inventory, backed up as `inventory.pxar` |
| `/run/nestegg/api.sock` | Local API (group `nestegg`) |

## Removing the package

`sudo apt remove nestegg` stops and disables the daemon and the timers
(backup and notification check) and removes the program. **`apt purge` does not delete more:** the
configuration and the encryption key in `/etc/nestegg/` and the run history
in `/var/lib/nestegg/` stay on disk in both cases. Deleting the key would make
every existing backup unreadable if no exported copy is at hand.

Installing nestegg again picks up the kept setup and enables the units that
were enabled before the removal; nothing has to be set up again. If
`/etc/nestegg/config.toml` is gone by then, nothing is enabled.

To remove everything, first make sure the key is exported and kept somewhere
safe (`sudo nestegg key export <file>`), then:

```sh
sudo apt purge nestegg
sudo rm -r /etc/nestegg /var/lib/nestegg
sudo groupdel nestegg
```

The backups on the PBS are not touched by any of this.

## Restore

```sh
nestegg snapshots
sudo nestegg restore host/desktop/2026-11-08T21:30:12Z home.pxar /root/restore-home
sudo nestegg restore host/desktop/2026-11-08T21:30:12Z home.pxar /root/restore-docs \
     --pattern 'alice/Documents/**'
```

Restores always go into a new directory and never overwrite files.

- Into a root-owned location (e.g. `/root/restore-…`): runs as root and
  restores original owners and permissions.
- Into a directory other users can modify (e.g. your home, via `sudo`): the
  archive is first restored as root into a private staging directory under
  `/var/lib/nestegg/restore-staging/` (needs temporary space there; choose
  another root-owned location with `--staging-dir`), then copied by a process
  running as you into a temporary sibling directory, which is renamed to the
  target only when the copy is complete. Nothing can be redirected; the files
  are owned by you; ACLs, extended attributes, file capabilities and sockets
  are not copied (skipped entries are listed as warnings). Sparse files stay
  sparse.
- The key is used only for encrypted or signed archives. For backups made
  with an older key, pass its exported copy: `--keyfile old-key.json`.

The key confirmation (`nestegg key confirm-exported`) is bound to the exact
key that is stored. Replacing the key (`nestegg key create --force`) blocks
backups again until a copy of the new key has been proven (a copy of the old
key or an old paper key is refused).

Because
backups are normal PBS snapshots, they can also be restored with plain
`proxmox-backup-client` (with the exported key) or through the PBS web UI
(unencrypted backups only).
