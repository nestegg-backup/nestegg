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
sudo nestegg key confirm-exported --paper-code <code on the paper>   # asks you to state that it is printed
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

The file never contains secrets. Unknown keys are rejected. It is owned by
`root:nestegg` with mode 0640, so members of group `nestegg` can read it (for
`nestegg status`) but others cannot; a token a `[notify] command` needs
belongs in a separate root-only file (mode 0600) that the command reads, not
here (see "Notifications").

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
  | "Overdue" after | `overdue_after_hours` | 2 hours up to `max(168, interval + 72)` hours, and more than the rhythm (so a weekly rhythm can be overdue after up to 10 days) |
  | Excludes | `backup.excludes` | up to 200 patterns of at most 256 bytes each, each one the backup client can parse; none that excludes everything (`*`, `**`, `/`, ...) and no change whose list, as the client applies it (matched inside each archive, later patterns win), would leave out a whole source, a whole user home directory (or a parent directory of one), or (in system mode) a whole top-level system directory |
  | Wake-on-LAN on/off | `wake.enabled` | only with a `[wake]` section; switching off keeps the MAC |
  | Automatic backups on/off | `nestegg-backup.timer` | `systemctl enable/disable --now` |

  The daemon changes only these keys in `/etc/nestegg/config.toml` and
  keeps your comments and layout. Each change is logged with the user who
  made it: `journalctl -u nestegg.service`. Changes made during a backup
  apply from the next run on.

  These are the limits a group member may set. root is not bound by them in
  the file: a longer "overdue after" (above 168 h) or a broad exclude set in
  `/etc/nestegg/config.toml` is kept, and can then only be lowered or
  narrowed through the web interface, never widened past it
  ([decision 0022](decisions/0022-settings-changes-that-reduce-protection.md)).
  The exclude guard is a guard against accidents and casual misuse, not a
  security boundary: a group member can still narrow the backup folder by
  folder, which is reported, named, to the owner. To judge whether a list
  empties a folder the daemon may read that folder's top-level entry names, and
  it does so with the requesting user's own permissions, never as root, so the
  guard's answer never reveals a name the requester could not list themselves.
  All such reads of one request together get a few seconds (four); a folder
  that cannot be read in that time counts as unread and is judged by made-up
  names instead; so does a home directory inside a source whose path is a
  symbolic link (the backup client stores such a link as a link). A source
  that is itself a symbolic link is read through the link, as the client
  backs up what it points to.
  nestegg matches patterns with
  the same `pathpatterns` library the backup client uses (an older release of
  the same code), so a pattern that the client cannot parse — an unclosed
  `[`…`]` class, a trailing `\` — is refused here with a plain message instead
  of breaking every backup later.
  A change by a group member that lowers the protection (automatic backups
  off, an exclude added, a `!` re-include removed, the exclude list reordered,
  "overdue after" or the interval raised, Wake-on-LAN off) is reported to the
  logged-in users and the notification hook (see "Notifications"); a change
  made as root in the file is not.
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
| `overdue` – "Backups are overdue" | No successful backup within `overdue_after_hours` (this includes a server that stays unreachable, and a setup that never succeeded). Also when the time of the last successful backup lies in the future (the clock of this computer was wrong): then its age is unknown, the text says so, and nestegg backs up again at the next check. | every 24 h |
| `automatic_off` – "Automatic backups are off" | The backup timer is switched off while nestegg is set up. Reported at once, independent of the overdue threshold. The text says how old the last backup is. | every 7 days |
| `incomplete` – "Backup is incomplete" | The last backup worked but left something out (for example the full system backup was skipped). | every 7 days |
| `damaged` – "Damaged backups on the server" | The server's own verification reports damaged snapshots of this computer. | every 24 h |
| `settings` – "Backup settings changed" | A group member lowered the protection through the web interface (automatic backups off, an exclude added or a `!` re-include removed, the exclude list reordered, "overdue after" or the interval raised, Wake-on-LAN off). Names who changed what; several changes are combined into one message. A change made as root in the file is not reported. | once |
| `recovered` – "Backups work again" | Everything is fine again after one of the above problems was reported. | once |

A different problem (another error, another kind) is reported at once. Each
message goes to every logged-in user and, if configured, to your command;
someone who logs in later still gets it once. A `settings` message is kept
until a member of group `nestegg` has received it (the full text): users
outside the group get the general text once each, but that does not close it,
so a member who logs in after a few days still sees who changed what. If no
member logs in for 30 days, it is dropped (one line in the journal) so the
record stays small. Problem messages (`failing`,
`overdue`, ...) are not judged while a backup is running, but a `settings`
change is still delivered then, so lowering the protection during a backup is
reported without waiting. A flurry of settings changes does not flood anyone:
the report waits out a short quiet period (about 20 seconds, at most two
minutes) so a burst becomes one run, everything pending is combined into one
message per recipient, and after the first message a recipient is told at most
once every few minutes — nothing is dropped, later changes are combined and
delivered once that time is over. An unreachable server alone is not reported
on every attempt, only when it becomes `overdue`.
A configuration file that cannot be read (a typo, an unknown key) is reported
as `failing` with the parser's message; the `[notify]` section is still
honoured in that case, if it is valid.

**Who sees what.** The desktop message to a member of group `nestegg` (and to
root) carries the full detail — the error text, the paths, who changed a
setting. Everyone else who is logged in gets only the kind of problem ("the
backup of this computer failed / is overdue / is incomplete / automatic
backups are off / the settings were changed") and the hint to tell whoever
looks after the computer, because the detail can contain file names and paths.
The command hook always gets the full text
([decision 0022](decisions/0022-settings-changes-that-reduce-protection.md)).

**Desktop** (`[notify] desktop = true`, the default): every logged-in user
with a session bus gets a notification (`notify-send` from the package
`libnotify-bin`, run as that user). `failing`, `overdue` and `damaged` are
marked critical. The texts are German when the system language is German
(`LANG=de…` in `/etc/default/locale` or `/etc/locale.conf`), otherwise
English. No `notify-send`, no session: nothing is sent and the check logs it
(`journalctl -u nestegg-notify.service`).

On a computer with several local users, the title and text of a desktop
notification are passed to `notify-send` as command-line arguments, so another
local user can read them from the process list for the moment it is shown —
including the full-detail version a group member sees. Systems that hide other
users' processes (`hidepid`) are not affected. nestegg uses `notify-send`
rather than talking to the notification service over D-Bus, which would avoid
this, to keep its dependencies small (decision 0017).

**Command** (`[notify] command`, off by default): a program with arguments
(a list, no shell), run as root with a 60 second limit, for every message:

| Variable | Content |
|---|---|
| `NESTEGG_EVENT` | `failing`, `overdue`, `automatic_off`, `incomplete`, `damaged`, `settings`, `outdated`, `recovered` or `test` |
| `NESTEGG_TITLE`, `NESTEGG_MESSAGE` | The short title and the text (also on stdin) |
| `NESTEGG_HOSTNAME` | Host name of this computer |
| `NESTEGG_LAST_SUCCESS` | Time of the last successful backup (RFC 3339), or empty (also when that time lies in the future) |

If the command fails or does not finish, the check logs it, `nestegg notify`
ends with a non-zero exit code so `systemctl --failed` shows it, and the
delivery is tried again at the next check; the desktop message is not held
back. A `settings` message that the command could not take is retried with a
growing pause (5 minutes, doubling, at most 6 hours). While that failure is
pending, every check ends with the same non-zero exit code and one line in the
journal, also the ones that wait out the pause, so the unit stays failed until
the command works or is switched off.

**The message contains text people typed.** `NESTEGG_TITLE` and
`NESTEGG_MESSAGE` can contain an exclude pattern, a file name or a user name a
group member chose, with control and invisible characters already replaced.
Still, pass these values as arguments or over stdin to the programs you call
(`curl --data-binary @-`, `mail`, ...); never build a shell command string out
of them (`sh -c "... $NESTEGG_MESSAGE ..."`), or a crafted value could run as
root. Each value is capped to a few kilobytes.

**Keep secrets out of `config.toml`.** The file is readable by group `nestegg`
(mode 0640, so members can run `nestegg status`), so do not put an access
token in `[notify] command`. Put it in a root-only file (mode 0600) that the
script reads. Example with a push service (ntfy; the host name is a
placeholder), reading its token from `/etc/nestegg/ntfy-token`:

```toml
[notify]
command = ["/usr/local/bin/nestegg-ntfy"]
```

```sh
#!/bin/sh
# /usr/local/bin/nestegg-ntfy  (chmod 755)
# Token in a root-only file, created with:
#   printf 'tk_...' > /etc/nestegg/ntfy-token && chmod 600 /etc/nestegg/ntfy-token
token="$(cat /etc/nestegg/ntfy-token)"
case "$NESTEGG_EVENT" in
  failing|overdue|automatic_off|damaged) priority=high ;;
  *) priority=default ;;
esac
{ printf '%s: %s\n\n' "$NESTEGG_HOSTNAME" "$NESTEGG_TITLE"; cat; } |
  curl -fsS --max-time 30 -H "Authorization: Bearer $token" \
       -H "Title: nestegg" -H "Priority: $priority" \
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
  print the file, then delete it (or move it to a USB stick). nestegg
  remembers where it wrote the file; confirming with the check code is refused
  while that file still lies on this computer (see below). The sheet shows the computer, date, server,
  datastore, the key fingerprint, a **check code** (`XXXX-XXXX`) and how to
  restore with it, then the key and a QR code. Without `--output` the command
  refuses to write into a terminal; piping to a printer
  (`nestegg key paperkey | lpr`) works. The check code appears in the paper
  key output only; no other command, log or page shows it.
  `nestegg key paperkey > key.txt` writes a file as well and is remembered
  like `--output`. nestegg finds out where the output goes before it writes
  anything: a file whose name is not valid UTF-8 (it could not be remembered)
  is refused, and so is a further file once 50 earlier paper key files still
  exist. Delete or move old ones first; nestegg never forgets a file that
  still exists. A file that is already remembered is not a new one: writing
  into it again (`> key.txt` on the same file) is allowed, also at 50 files.
  The command takes the run lock only for the moment it changes the state:
  while a backup is running it stops with "try again" before anything is
  written. Into a pipe, the paper key is recorded as exported only after all of
  it was written without an error (a reader that is already gone is an error:
  nothing is recorded, exit code 1; whether a reader that took the data also
  printed all of it, nestegg cannot know, which is why backups still wait for
  the confirmation with the check code from the paper). The lock is not held
  while the output goes into the pipe; it is taken again for the short update
  (waiting up to 30 seconds). If a backup holds it longer, or the key changed
  meanwhile, the paper key is out but not recorded: the command says so and how
  to record it (`nestegg key confirm-exported --paper-code ...`), and exits
  with a non-zero code.
- **Key file:** `sudo nestegg key export /media/usb/nestegg-key.json`. The
  file is read back and compared with the key. If it lies on a disk of this
  computer you get a warning: that copy is lost together with the disk and
  useless inside its own backup (which is encrypted with this key). Move it to
  a USB stick or a password manager. The same warning applies to a file inside
  what the backup contains (see below).

Then prove it. `confirm-exported` needs exactly one of:

```sh
sudo nestegg key confirm-exported --paper-code 1A2B-3C4D --printed   # from the sheet
sudo nestegg key confirm-exported --file /media/usb/nestegg-key.json
```

`--file` takes the key file or a saved paper key (text or HTML); the key
material is compared, not the bytes. A file on a disk of this computer is
only accepted after you state that another copy exists outside it: type `yes`
when asked, or pass `--accept-local-copy` without a terminal. A file on
removable media needs neither.

`--paper-code` proves that the code is right, not that a sheet of paper
exists: the code can be read off a file or the screen. So it needs your
statement that the sheet is printed and stored away from this computer: type
`yes` when asked, or pass `--printed` without a terminal. Without the
statement nothing is recorded. In addition, the command is refused while a
file written by `key paperkey --output` still exists on a disk of this
computer; the message names the file. Delete it after printing, or move it to
a USB stick. A file that is gone or lies on removable media does not block.
If the file cannot be deleted (read-only file system, no permission), it is a
copy of the key: the message then names the other way,
`sudo nestegg key confirm-exported --file <that file> --accept-local-copy`,
which needs the same statement that another copy exists outside this computer.
A paper key piped to a printer leaves no file; the statement covers that
case. (A copy of the file made by hand elsewhere cannot be found; the
statement is where you promise there is none.)

With a terminal and no option the command asks what you have. A wrong file or
code is an error and nothing is recorded. Keys confirmed by an earlier
version stay confirmed.

**What counts as "removable".** A USB stick or card (see decision 0018 for the
rules). Not if it is part of the backup: a path inside a configured source
(`backup.sources`, `backup.include_dev`) counts as on this computer whatever
the device is, because the copy then exists only together with the data it
protects, and inside the backup it is encrypted with the key it contains. In
system mode this includes everything on the file systems that go into
`system.pxar`. The backup does not cross mount points, so a USB stick mounted
*below* a source (for example under `/home/alice` when `/home` is a source) is
not part of the backup and counts as removable: only paths on the file system
of the source itself, or on one that is a source or listed in `include_dev`,
are inside. nestegg reads the mount table for this; if it cannot, everything
below a source counts as inside. `key export` warns about such a place; `key import` and
`confirm-exported` treat it like a disk of this computer. If the
configuration cannot be read, a removable copy cannot be told apart from one
inside the backup and is treated the same way.

**Unplug the stick.** A stick or card that simply stays plugged in cannot be
told apart from one that was taken away, and it is lost or damaged together
with the computer. After a successful confirmation with a copy on removable
media nestegg says so: unplug it and keep it away from this computer.

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
another removable medium counts as exported, because you hold that copy, as
long as the medium is not part of the backup. A
file on this computer's own disk (or inside the backup) does not: confirm it afterwards with
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
| `/etc/nestegg/config.toml` | Configuration (no secrets), `root:nestegg` mode 0640 |
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

## Registered backup sticks (preparation only)

`sudo nestegg stick add /dev/sdX --image nestegg-rescue-amd64.iso` preserves
existing settings and adds the ext4 data UUID under:

```toml
[stick]
data_uuids = ["01234567-89ab-cdef-0123-456789abcdef"]
```

UUIDs must be distinct and have ext4 UUID syntax; unknown keys are rejected,
including inside `[stick]`. Existing `[pbs]`-only files require no migration.
Phase one only prepares/registers media; it does not automatically back up to
these drives. A file with just `[stick]` is accepted by the stick commands;
the existing backup pipeline still requires `[pbs]`. Per-target settings and
stick-only backups arrive in phase two. See [stick preparation](stick.md).
