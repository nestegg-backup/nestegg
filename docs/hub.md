# Household hub (stage 1)

The optional hub provisions computers and shows what the PBS knows about their
backups. Computers still send backups directly to the PBS. An unavailable hub
does not interrupt backups or restores. Stage 1 has no remote computer control.
The API contract is [hub-openapi.yaml](../api/hub-openapi.yaml), and the design
is [decision 0034](decisions/0034-hub.md).

## Set up the hub

Use an always-on Debian-family amd64 machine, not a PBS that sleeps. Install the
nestegg package and the official `proxmox-backup-client-static` as described in
[configuration](configuration.md#installing-the-backup-client). Package
installation creates the dedicated `nestegg-hub` account. It does not enable
or start the hub automatically.

On the PBS, create a household namespace and a dedicated `backup@pbs` user with
`DatastoreBackup` on `/datastore/backups/household`, without other roles. Keep
this user's password, not an API token secret, for initialization. Alternatively,
`--prepare-pbs` asks for a PBS administrator once and creates the namespace,
user with a random password, and ACL. That administrator password is neither
stored nor logged. Existing namespaces are reused; an existing user must
have the supplied/stored password and matching namespace ACLs. An existing
user with other ACLs is refused, never reset or silently repurposed. To reuse
an existing user during preparation, supply its password in a private file
with `--pbs-password-file`.

```sh
sudo nestegg hub init --name Home --url https://hub.example.com:8443 \
  --server pbs.example.com --datastore backups --namespace household \
  --user backup@pbs --package ./nestegg_0.1.0_amd64.deb \
  --paperkey /media/usb/household-master.html
```

Add `--prepare-pbs` for automatic PBS preparation, `--wake-mac
00:00:5e:00:53:01` for a sleeping PBS. Missing required values are requested at
the terminal. Use `--root-namespace` to explicitly select the datastore root.
Passwords are entered without echo; the administrator password
is entered twice and must contain at least 12 characters. Private
`--pbs-password-file`,
`--pbs-admin-password-file` and `--admin-password-file` inputs support
unattended setup; their contents are never command arguments. The master-key
passphrase always goes directly from the controlling terminal to the official
client at creation and to OpenSSL at proof, including when other inputs come
from files.

Unless `--fingerprint` is supplied, initialization fetches and displays the
PBS certificate fingerprint and requires explicit confirmation. Compare it
with the fingerprint shown by the PBS itself before accepting it. To use a
publicly trusted PBS certificate instead, select `--system-roots`.
Initialization verifies login, token creation, the token ACL, deletion and
namespace access before recording a working configuration.

The official client creates an encrypted RSA master key and asks for its
passphrase directly. Keep that passphrase separately from the printed sheet.
The HTML sheet includes a check code, the public-key fingerprint and recovery
instructions. The official QR rendering is larger than 2 MiB; master-copy
proofs therefore accept regular files up to 4 MiB. It is an RSA PEM copy, not
the symmetric JSON key used by standalone computers.

Prove the copy during initialization, or use `--defer-confirm` and do it later:

```sh
sudo nestegg hub master-key confirm --file /media/usb/household-master.html
# Or, after printing and deleting/moving the local paper file:
sudo nestegg hub master-key confirm --paper-code <CODE-ON-THE-SHEET>
```

A file proof compares the encrypted PEM and then decrypts that copy once with
OpenSSL. A paper-code proof decrypts the hub's pending private PEM. Enter the
passphrase freshly at that program's terminal prompt; nestegg never reads it.
A wrong passphrase records nothing and removes nothing. A local or
unclassifiable file additionally needs the statement that another copy exists off the hub
(`--accept-local-copy` without a terminal). A paper code needs the statement
that the sheet is printed and stored elsewhere (`--printed` without a terminal).
After successful decryption, nestegg removes private copies in `master/`, the
recorded paper file if local, and the supplied proof file if local, before
recording proof. If a copy cannot be removed, the message names the file to
move away or delete and proof remains pending. Keep the independent copy safe;
`--accept-local-copy` does not permit keeping a private copy on the hub.
Unplug removable media after proof. Until proof succeeds, join codes are refused.
`init` refuses to overwrite an existing configuration; `master-key confirm`
resumes pending proof after interruption.

Start the service after proof:

```sh
sudo systemctl enable --now nestegg-hub.service
sudo nestegg hub status
```

The service runs as `nestegg-hub`, with an empty capability set and a filesystem
sandbox. Unlike the backup and restore services of decision 0007, it never
needs access to desktop files. PID 1 decrypts only its PBS password using
`LoadCredentialEncrypted=password:...` and delivers the TLS key with
`LoadCredential=hub-tls-key:...`. This service configuration requires systemd
250 or newer. `--allow-plain` is an explicit fallback for development containers
without usable `systemd-creds`; the shipped service expects the encrypted
credential and cannot use that fallback unchanged. For such a setup, a local
service override can replace the encrypted directive with
`LoadCredential=password:/etc/nestegg-hub/credentials/password`. Never loosen
permissions on the credential directory.

Configuration is `/etc/nestegg-hub/hub.toml`; it has no secrets. The default
listener is `0.0.0.0:8443`. Set `certificate` and `private_key` there to use your
own certificate/key. If changing the private-key path, update the unit's
`LoadCredential=hub-tls-key:...` source accordingly. Restart the service after
configuration/certificate changes, and give computers the new pin if the
public key changed. `state_dir` and `credentials_dir` can be changed for
isolated instances; adjust the unit's state path and credential sources too.

## Add a computer

Open the hub URL in a browser and log in with the administrator password.
Browser trust of a self-signed certificate must be established separately;
CLI clients pin its public key. The UI build supplies `hub.html`; a build
without it displays a small placeholder while the API remains available.
The hub embeds `ui/dist/hub.html` and its assets from the same build directory
as the local interface. A normal multi-page Vite build must retain
`ui/dist/index.html` too, because that file selects the embedded asset directory.

Add or replace the served amd64 package, then generate a code in the interface
or terminal:

```sh
sudo nestegg hub package add ./nestegg_0.1.0_amd64.deb
sudo nestegg hub join-code --note 'laptop in the study'
```

Run the printed command on the new computer. It fetches `/join.sh` over pinned
HTTPS, installs the Proxmox repository/key (with the checksum documented in
configuration.md) and static backup client, fetches `/pkg/nestegg.deb` and
`/pkg/SHA256SUMS` over the same pinned connection, verifies the package, and
runs exactly:

```sh
nestegg join "$hub" --pin "$pin" --code "$code" "$@"
```

Additional script arguments support `--name <backup-id>` and `--adopt`.
Already installed computers can run `sudo nestegg join <url> --pin <pin>
--code <code>` directly. Codes expire after 30 minutes and enroll one computer.
The computer keeps a
random enrollment attempt in its private join journal. Until expiry, the same
attempt and backup name can redeem the code again, rotating the token secret
and machine credential. Other reuse is refused. Only the attempt hash is kept
on the hub. Connection failures after sending cause up to three automatic
requests with the same attempt. PBS and internal errors retain the attempt for a later retry;
a failure before sending, such as a wrong pin, leaves no enrollment journal.
If interrupted, run the same command again; keep the journal.
An activated setup completes on retry and keeps its encryption key.

The hub bounds total join work, including waiting for another provisioning
operation, to **180 seconds**. The computer's request timeout is **240 seconds**,
leaving 60 seconds for response delivery. Wake-on-LAN waits at most 60 seconds
within that budget; individual PBS requests have a 15-second timeout. State
locks cover disk transactions only, never PBS calls, wake waits or login delays.
Other requests remain responsive while provisioning waits. The code and
returned secrets
are disclosed only at creation. The PBS is woken for joining when configured;
listings never wake it and show the last cached PBS data when it is asleep.

A joined name is unavailable. A name with an existing backup group requires
explicit adoption; it can be adopted only when its owner is exactly the new
token ID, such as `backup@pbs!desktop` after an earlier hub removal. Groups
owned by another identity cannot be taken over with the hub's restricted role.

## Replacing a lost computer

1. Remove the dead computer in the hub; its snapshots stay on PBS.
2. Create a new join code. On the replacement, run its bootstrap command with
   `--name <old-name> --adopt` (or use the installed `nestegg join` command).
   If replacing an existing configuration, also pass `--replace-config`.
3. With existing snapshots and no local key, join installs configuration and
   credentials but creates no key and starts no backup. Status/check report the
   missing key. Install the old key with:

   ```sh
   sudo nestegg key recover --master-key /media/usb/master-private.pem --install
   ```

   Enter the master passphrase at the official client's prompt. The command
   reports snapshot time and fingerprint, verifies the recovered key against
   the signed manifest, installs it, and starts the first backup. One key reads
   both old and new snapshots. It never replaces a different existing key.
   If several key fingerprints occur in the group, it lists their time ranges
   and requires `--snapshot <RFC3339-time>`. Without `--install`, use
   `--output <new-file>` to export the recovered key instead.
4. Restore a file from an old snapshot and check it. New backups use the same
   group and recovered key. Alternatively, `sudo nestegg key create` starts a
   new key generation; old backups then require recovery with the master key.

The dead computer's token secret is gone. Access to its old group comes from
joining again under the same name, which creates a new secret for the same
PBS token identity. The master key alone does not grant PBS login access.
For the plain-client recovery and restore commands, see
[configuration](configuration.md#replacing-a-lost-computer).

`--replace-config` preserves sources, excludes, schedule, registered sticks,
notifications and other local settings. It changes only the hub's PBS target,
backup identity, wake settings, hub association and master public key. The old
configuration is kept beside it as `config.toml.before-join-<random>`.

## Stored data and permissions

- `/etc/nestegg-hub/credentials/password.cred`: the encrypted PBS user password,
  using the existing `systemd-creds` storage mechanism and no PCR binding.
- `/etc/nestegg-hub/tls.key`: root-only TLS private key. The certificate and
  configuration are public, without secrets.
- `/var/lib/nestegg-hub/state.json`: administrator argon2id password hash,
  password/session generation, hashes and expiry of codes, joined computer
  metadata and hashed machine credentials, master public PEM/fingerprint/proof
  time, package metadata, and cached PBS overview. It contains no token secrets.
- `/var/lib/nestegg-hub/master/`: pending encrypted private RSA key until proof,
  then public PEM only. An interrupted setup retains the encrypted private key
  until proof can resume. The paper copy goes to the administrator's chosen path.
- `/var/lib/nestegg-hub/packages/`: content-addressed package copies, version,
  architecture and SHA-256. A replacement is published atomically.

State transactions use a separate lock inode, atomic replacement and directory
fsync. Joins record a non-secret pending intent before creating a PBS token;
a retry compensates an interrupted attempt before creating a fresh token.
Server-side sessions expire after 30 minutes idle and disappear on restart.
Password checks use one global lane with a one-second delay per failure,
regardless of source address. At most four login requests are admitted at
once; excess requests receive `429` without checking a password. An admitted
correct login can wait behind at most three failed checks (about three
seconds), and is never refused by the per-address failure counter. Valid
sessions bypass login throttling. Per-address failures are tracked using the
actual peer;
the bounded table evicts its oldest entry instead of refusing new addresses.
Internal hub failures return `500 hub_error`; PBS failures remain
`503 pbs_unreachable` or `pbs_refused`. Failed requests and state changes emit
short service stderr records without request data or secrets. Packages stream
from disk in 64 KiB chunks, with at most four concurrent downloads. Session
cookies are `HttpOnly; Secure; SameSite=Strict`.
Browser mutations require `X-Nestegg-Hub: 1`; the code-authorized join and
Bearer-authorized leave endpoints need no browser session/header.

Change the administrator password with `sudo nestegg hub password`; this
invalidates existing sessions immediately. Keep a protected backup of the hub
configuration, credential storage and state, but keep the master paper key and
its passphrase separately. A hub backup may require re-entering the PBS password
after changing the host/TPM, just like desktop systemd credentials.

## Threat model and removal

A compromised hub can create tokens, read encrypted backups and add snapshots.
Its namespace role cannot delete backups or decrypt their contents. Anyone
with the hub administrator password or the PBS household user's password can
delete every computer's PBS token. This stops all household backups until the
computers join again; the existing backups remain. The PBS user, as owner of
its tokens' backup groups, may also add snapshots to any computer's group,
including misleading snapshots. It serves
a script and package run as root on new computers, so it can take over computers
that join after compromise. Protect it like the PBS. Stage 1 trusts the hub's
package checksum; checking a public release checksum is future work. Possession
of the master private key and passphrase unlocks every household computer's
backups. Losing them and a computer can make that computer's backups unreadable.

Remove a computer in the interface (`DELETE /api/hub/v1/machines/<machine_id>`),
or let it call `nestegg leave` using its machine credential. The hub deletes its
PBS token and machine credential; backup groups and snapshots remain. Removal
requires the PBS to answer; a failure leaves the computer recorded for retry.
An administrator removal does not contact the computer. Its next backup fails
until it rejoins with a fresh code. Stage 1's machine-authenticated leave has the
same token-revoking behavior as administrator removal, as specified by the API.
Leaving also removes the local token secret, so backups stop until the computer
joins again or receives its own token with `nestegg init`. The encryption key
and configuration are kept. To rejoin the same computer under its old name,
append `--name <old-name> --adopt --replace-config` to a fresh bootstrap
command; the existing encryption key is kept.

One requirement needs reconciliation. The authoritative bootstrap
command and `nestegg join --code` interface carry the one-time code in process
arguments; this conflicts with a blanket ban on codes in arguments. The hub
follows that required interface, while passwords, token secrets and machine
credentials never enter process arguments. Removing code arguments requires a
coordinated bootstrap/computer CLI contract change.

## End-to-end onboarding check

Build the UI and package as described in
[configuration](configuration.md), then run:

```sh
tests/e2e/hub-join.sh target/debian/nestegg_<version>_amd64.deb
```

The test creates its own uniquely named images, network, PBS, unprivileged hub
and two fresh systemd desktops, and removes them on exit, including failures.
It needs Docker and the same disposable-machine privileges as `systemd.sh`.
Only the desktops' bootstrap installs the backup client and nestegg. The test
uses the exact returned command in an ordinary user's new login session,
checks master-key recovery, cookie sessions and built assets, revokes tokens,
and rejoins the same computer with its original key and backup group. Joining
records one pending PBS backup, so even a recent earlier success cannot make
the first run after rejoining skip the backup; previous success evidence stays.

Docker's overlay root cannot supply a bare-metal system image. The test keeps
system mode enabled and checks that this limitation is disclosed as incomplete;
the home backup, verification and file restore must still succeed. This is not
a bare-metal restore test. The printed command also needs `curl` and `sudo`
before it can fetch the installer. In a shell without `pipefail`, a failed
`curl` can be hidden by the empty `sudo sh` succeeding; the test uses Bash with
`pipefail` and verifies that a wrong pin installs and writes nothing. Changing
that portable bootstrap command's exit behavior needs a separate contract
decision.
