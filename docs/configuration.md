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
sudo apt install proxmox-backup-client        # or proxmox-backup-client-static
sudo apt install ./nestegg_<version>_amd64.deb

sudo nestegg init --server pbs.example.com --datastore backups \
     --namespace desktops --auth-id 'backup@pbs!desktop' \
     [--fingerprint <SHA-256>] [--wake-mac 00:00:5e:00:53:01]
sudo nestegg credentials set-password     # paste the token secret
sudo nestegg key create                   # reinstalling? keep the old key: sudo nestegg key import <file>
sudo nestegg key paperkey                 # print it - or: sudo nestegg key export /media/usb/nestegg-key.json
sudo nestegg key confirm-exported
sudo nestegg check
sudo systemctl enable --now nestegg.service nestegg-backup.timer
sudo usermod -aG nestegg "$USER"          # log out and in, then: nestegg ui
```

The fingerprint is only needed when the PBS uses its default self-signed
certificate. It is shown on the PBS dashboard ("Show Fingerprint").

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

## Web interface and API permissions

`nestegg ui` (run as a member of group `nestegg`) talks to the local daemon.
The daemon identifies the caller by the socket credentials:

- Everyone in the group can see the status, start and cancel backups and
  run the connection check.
- Browsing and restoring is limited to the caller's own home directory
  (other homes and archives such as `/etc` are refused). Restored files are
  written with the caller's identity into a new directory.
- root (`sudo nestegg restore …`) may restore everything.

The overview of all machines reads each machine's last snapshot and the
status report stored in its notes. Without `[overview]` credentials only the
machine's own backups are visible.

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
([decision 0007](decisions/0007-credentials-without-pcr-binding.md)). If the
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
backups refuse to run until `nestegg key confirm-exported` was executed.

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

`nestegg key import` checks the file with `proxmox-backup-client key show`,
stores it like `key create` does and counts it as exported, because you
already have the file. It only accepts keys without a password (as written
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
| `/var/lib/nestegg/runs/` | Run history (`<id>.json`) and logs (`<id>.log`) |
| `/var/lib/nestegg/inventory/` | Generated inventory, backed up as `inventory.pxar` |
| `/run/nestegg/api.sock` | Local API (group `nestegg`) |

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
backups again until the new key has been exported and confirmed.

Because
backups are normal PBS snapshots, they can also be restored with plain
`proxmox-backup-client` (with the exported key) or through the PBS web UI
(unencrypted backups only).
