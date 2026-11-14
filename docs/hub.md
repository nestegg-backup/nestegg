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
is entered twice. Private `--pbs-password-file`,
`--pbs-admin-password-file` and `--admin-password-file` inputs support
unattended setup; their contents are never command arguments. The master-key
passphrase always goes directly from the controlling terminal to the official
client, including when the other inputs come from files.

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

A copy on removable media is compared with the encrypted private key. A local
or unclassifiable location additionally needs the statement that another copy
exists off the hub (`--accept-local-copy` without a terminal). A paper check
code needs the statement that the sheet is printed and stored elsewhere
(`--printed` without a terminal); a remembered paper file still on the hub
blocks that path. Unplug the removable medium after proof. The hub deletes
its private master key before recording the proof. Until that proof succeeds,
it refuses to create join codes. `init` refuses to overwrite an existing
configuration; `master-key confirm` resumes pending proof after interruption.

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
CLI clients pin its public key. The UI lane supplies `hub.html`; a build
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
--code <code>` directly. Codes expire after 30 minutes, are used only once,
and stay usable when PBS provisioning fails. The code and returned secrets
are disclosed only at creation. The PBS is woken for joining when configured;
listings never wake it and show the last cached PBS data when it is asleep.

A joined name is unavailable. A name with an existing backup group requires
explicit adoption; it can be adopted only when its owner is exactly the new
token ID, such as `backup@pbs!desktop` after an earlier hub removal. Groups
owned by another identity cannot be taken over with the hub's restricted role.

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
Login failures are delayed and limited by the actual peer address, not a
forwarded header. Session cookies are `HttpOnly; Secure; SameSite=Strict`.
Browser mutations require `X-Nestegg-Hub: 1`; the code-authorized join and
Bearer-authorized leave endpoints need no browser session/header.

Change the administrator password with `sudo nestegg hub password`; this
invalidates existing sessions immediately. Keep a protected backup of the hub
configuration, credential storage and state, but keep the master paper key and
its passphrase separately. A hub backup may require re-entering the PBS password
after changing the host/TPM, just like desktop systemd credentials.

## Threat model and removal

A compromised hub can create tokens, read encrypted backups and add snapshots.
Its namespace role cannot delete backups or decrypt their contents. It serves
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

Two requirements need reconciliation at integration. The authoritative bootstrap
command and `nestegg join --code` interface carry the one-time code in process
arguments; this conflicts with a blanket ban on codes in arguments. The hub
follows that required interface, while passwords, token secrets and machine
credentials never enter process arguments. Removing code arguments requires a
coordinated bootstrap/computer CLI contract change. Also, decision 0034 says a
leaving computer keeps backing up, but the API explicitly revokes its PBS token;
stage 1 implements the API's revocation semantics.
