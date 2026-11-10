# 0013 – A few harmless settings can be changed in the web UI

- Status: accepted
- Date: 2026-11-09

## Context

Until now the web UI showed the configuration read-only
(`GET /api/v1/config`). Every change meant `sudo` and an editor on
`/etc/nestegg/config.toml`, or `systemctl` for the timer. For everyday
settings such as the backup rhythm, that is more than most desktop users
want to do.

The web UI talks to the daemon, which runs as root. Every member of group
`nestegg` can reach the socket (see `docs/configuration.md`, "Web interface
and API permissions"), and browser access through `nestegg ui` is protected
only by loopback, a random port and a session token. Whatever the API lets
a group member change, a compromised browser session or a less trusted
household member can change too. The most dangerous change would be to
point the backups at another server: the root process would then send the
whole system, including `/etc` and every home directory, to a machine the
attacker controls.

## Decision

- **Editable through the API** (`PATCH /api/v1/config`), for every member
  of group `nestegg` and for root:
  - `interval_hours` (backup rhythm), 1 to 168 hours;
  - `overdue_after_hours`, 2 to 744 hours and greater than
    `interval_hours` (otherwise the dashboard reports "overdue" before the
    next backup is due; checked only when one of the two changes, so an
    existing file that breaks the rule can still get other changes);
  - `excludes`, the complete new list: at most 200 patterns of 1 to 256
    characters, no control characters, no leading or trailing spaces, no
    duplicates, and no pattern that matches everything (`*`, `**`, `/`,
    `/*`, `**/*`, `?*`, ...: every path component made of `*` and `?`
    only). Character classes are not analysed. Removing all excludes is
    allowed;
  - `wake_on_lan`, which sets the new key `wake.enabled` (default `true`,
    so existing files stay valid). Switching off keeps the MAC address.
    Switching on without a `[wake]` section is a 400 error: the MAC address
    is network information and is set in the terminal;
  - `automatic_backups`, which runs `systemctl enable --now` or
    `disable --now nestegg-backup.timer`, only when the state differs.
- **Terminal-only** (root): server, port, datastore, namespace, auth id,
  fingerprint, backup id, credentials, encryption and the encryption key,
  sources, `include_dev`, the system backup, the `[wake]` details and the
  overview credentials. Each of them can redirect backups, make them
  unreadable or widen what root reads. The API rejects these keys with a
  message that points to the terminal; unknown keys are rejected too.
- **Permissions:** the socket permissions decide, as for every other
  endpoint (group `nestegg`, decided before). No extra check in the
  handler. The daemon logs every change with the caller's user name and
  uid (from the socket credentials) to its journal, also refusals.
- **PATCH, not PUT:** only the given fields change, so a client cannot
  reset settings it does not know about, and new editable fields can be
  added later without breaking old clients. The response is the new
  `GET /config` view. `GET /config` also reports `editable`, `limits`,
  `wake_on_lan_configured`, `wake_on_lan` and `automatic_backups`, so the
  UI can render the form without hard-coded rules.
- **Writing the file:** `toml_edit` changes only the touched keys and keeps
  comments, order and formatting (also comments of exclude patterns that
  stay). The result must pass `Config::parse` and contain the requested
  values; then a new file in the same directory (exclusive, no symlinks)
  gets the old owner and mode, is synced and renamed over the old one. A
  symlinked configuration file is refused. A mutex in the daemon serialises
  changes, so concurrent requests for different settings do not lose each
  other's changes. `excludes` is replaced as a whole: of two clients that
  edit the list at the same time, the last one wins.
- **Timer failures:** the file is written first, then `systemctl` runs. If
  it fails (no systemd, no system bus), the old file is put back and the
  request fails with 500, so nothing is half-applied.
- **Runs in progress:** changes are accepted during a run. A run reads the
  configuration once when it starts and the file is replaced atomically,
  so it finishes with the old settings; disabling the timer stops the
  timer, not the running backup.

## Consequences

- Everyday settings no longer need `sudo`; the security-relevant ones still
  do, and the API cannot be used to send backups elsewhere.
- A group member can make backups less frequent, switch automatic backups
  off or exclude data (but not everything). This is accepted: the same
  people can already cancel runs, and the dashboard shows the state. The
  journal tells who did it.
- The daemon now writes `/etc/nestegg/config.toml`. Manual edits made while
  a change is being written can be overwritten; the window is small.
- The schedule still has no time of day: the hourly timer runs a backup
  when the last success is older than `interval_hours`, and
  `Status.schedule.next_run` is computed from that.
- Switching Wake-on-LAN off applies to backups, the connection check and
  the status. The rescue stick still wakes the server with the configured
  MAC: restoring onto an empty disk needs the server, and waking it there
  does no harm.
- Exclude patterns starting with `-` are refused, because
  `proxmox-backup-client` would read them as options and every run would
  fail.
- If `systemctl` switches the timer but cannot start or stop it, the
  settings are kept and the error says so; the file is only put back when
  the switch did not change.
- `tests/e2e/systemd.sh` changes settings through the socket of a real
  systemd installation and checks the file and the timer.

## Update 2026-11-10 (decision 0022): the API refuses the extremes

The editable settings stay as above, but two of them are now capped more
tightly through the API, so a group member cannot switch the protection off
quietly (see [decision 0022](0022-settings-changes-that-reduce-protection.md)):

- **`overdue_after_hours`** is capped at **168 hours (7 days)** through the API
  (was 744). A larger value that root set in the file is kept and can only be
  *lowered* through the API; `limits.overdue_after_hours.max` reports the
  effective maximum (168, or the file's value if larger).
- **`excludes`:** a *newly added* pattern that would drop a whole configured
  source or a whole top-level directory of an archive is refused (`/home`,
  `/home/*`, `/etc`, an anchored single component such as `/alice`, a bare
  source name such as `home`). A dangerous pattern already in the file is
  grandfathered. The "matches everything" rule stays as before.

A change by a group member that lowers the protection is now also **reported**
to the users and the hook (decision 0022); the journal log of every change
stays.
