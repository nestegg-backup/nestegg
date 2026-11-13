# 0016 – Restore permissions come from the backup metadata, not the notes

- Status: accepted
- Date: 2026-11-09
- Relates to: decision 0015 (metadata blob)

## Context

An unprivileged user (member of group `nestegg`) may browse and restore only
their own home directory. The daemon maps the home directory onto an
archive with the archive's source path: `/home/alice` and source `/home`
give `/alice` inside `home.pxar`. That source path came from
`archive_sources` in the snapshot notes.

Snapshot notes are not part of the snapshot: they are neither encrypted nor
covered by the manifest signature, and anyone with write access to the
backup group on the PBS can change them at any time (the backup token of
this computer, every other owner of the group, administrators). A forged
entry such as `"system.pxar": "/home/alice"` makes the whole system archive
alice's "home directory": she could browse and restore `/etc/shadow` and
every other home.

## Decision

For non-root requesters the source path of an archive comes, in this order,
from:

1. **the metadata blob of that snapshot** (`nestegg-meta.conf.blob`,
   decision 0015), restored with the configured key. It must parse and
   describe exactly this snapshot (backup id and time). The result is
   cached per snapshot (finished snapshots do not change). If encryption is
   on but the blob is not encrypted, it is not used (it could have been
   made without the key) and step 2 applies;
2. **no blob** (snapshot made by an older version): the local run record of
   exactly that snapshot (`/var/lib/nestegg/runs/*.json`, `archives[].source`).
   Run records are written by root only;
3. **neither:** `403 forbidden` with a message to ask an administrator
   (`sudo nestegg restore <snapshot> <archive> <target>`).

An archive that is not listed in the chosen source is refused like an
archive without the user's home ("does not contain your home directory").
A blob that cannot be read is an error; a blob that is invalid or belongs
to another snapshot is refused (403) and logged. Neither falls back to the
notes.

Notes are used for display only (`/machines`, the snapshot comment).
Checked call sites where snapshot data decides what a non-root user can
read or write: `GET /snapshots/{time}/browse` (scope of the catalog
listing) and `POST /restores` (scope of the restore patterns and of a whole
archive restore) both go through the same lookup; restore jobs are bound to
the requesting uid; `/machines`, `/snapshots` and `/status` only display.
Root requesters (CLI with `sudo`, API as uid 0) are not limited and not
affected.

## Consequences

- Forged notes no longer widen what a user can see; the end-to-end test
  forges them on a real PBS and checks that `etc.pxar` stays closed.
- The first browse or restore of a snapshot by a non-root user restores the
  blob (a few hundred bytes) from the PBS.
- Snapshots of older versions stay usable for users as long as their run
  record is kept (`keep_runs`, default 200). After that, or on a reinstalled
  computer without the old state, only root can restore them.
- A blob encrypted with an earlier key generation cannot be read with the
  current key; users then get an error for that snapshot (as before: its
  catalog needs the key too). root restores it with `--keyfile`.
- Legacy run records hold the configured source path (not canonicalised).
  New records hold the pinned source's resolved path, but do not carry the
  signed handle binding. Neither kind grants the new below-home scope.
- **Residual risk, unencrypted backups:** without encryption the manifest is
  not signed and the blob is not encrypted. Finished snapshots cannot be
  changed through the PBS API, but whoever can create snapshots in this
  backup group (the backup token, other owners, PBS administrators) can
  upload a new snapshot with a forged blob, and anyone with file system
  access to the datastore can change an existing one. For such a snapshot
  a user could be given a wider scope. With encryption on, a forged blob
  cannot be made without the key, and an unencrypted blob is not trusted.
  Use encryption (the default) where this matters.

## Update 2026-11-10: the key is always handed to the client

The first implementation decided whether to pass the key from the server's
file listing: `metadata::fetch` read the crypt mode from one `snapshot files`
call, and the blob restore (`RestoreKey::Auto`) asked the server a second
time and ran **without** the key when that answer said `none`. The daemon
then trusted the blob when the first listing said `encrypt`. A server, or
anyone who can write the datastore at file level, could therefore present a
forged, unencrypted blob (first listing `encrypt`, second `none`): it was
restored without the key, the client never checked a signature, and the
forged `archive_sources` were trusted. That defeated the guarantee above.

**Now, whenever encryption is configured, the key is always handed to the
client for the metadata blob and for the inventory archive of the read-back
(`Client::configured_key`). No decision about passing the key depends on what
the server lists.** The client itself then verifies the snapshot's manifest
signature with the key before it returns anything.

Confirmed with the real `proxmox-backup-client` (restoring the blob with
`--keyfd`):

- **manifest signed with our key** (the normal case, blob `encrypt` or
  `sign-only`): restores and returns the blob;
- **manifest signed with another key:** fails, `wrong key - unable to verify
  signature since manifest's key <A> does not match provided key <B>`;
- **unsigned manifest** (an unencrypted, possibly forged snapshot): fails,
  `manifest has no signature, but an encryption key is set - refusing to
  trust a potentially tampered, unsigned backup`;
- restoring **without** the key from a snapshot whose manifest was made with
  a key fails too (`missing key - manifest was created with key <A>`), so a
  forged `none` entry in the listing cannot trick a no-key restore of a
  signed snapshot either.

How the daemon maps this to the fallbacks above (`load_metadata`):

- the blob restores and describes this snapshot → trusted;
- the client refuses it as unsigned (`manifest has no signature …`) → the
  snapshot is not signed with our key, so it could have been made without the
  key: not trusted, fall back to the local run record (step 2), else `403`;
- any other read failure (wrong key, transport, file system) → a blob that
  cannot be read is an error;
- a blob that parses but describes another snapshot → refused (`403`).

With encryption **off** the behaviour is unchanged (`RestoreKey::Auto`); the
residual risk below still applies. The end-to-end test uploads a forged,
unencrypted snapshot into the machine's own group with the backup token while
encryption is on and checks that browsing and restoring it as a non-root user
stay forbidden.

## Update 2026-11-10: restoring the archive itself names an unprotected snapshot

Permissions no longer depend on an `Auto` key (the blob decides, above), but the
restored **content** still did: with encryption configured, restoring a data
archive with `RestoreKey::Auto` would restore an old or planted snapshot that is
neither encrypted nor signed without any hint, exactly as if it had been made
with the key. Unencrypted snapshots can be legitimate (made before encryption
was switched on), so they are not refused outright, but the restore now says so:

- With encryption configured, the restore first learns whether the archive is
  protected (encrypted or signed) from a single listing. A protected archive is
  restored with the configured key handed over explicitly, so the client
  verifies the manifest signature — never decided by the restore's own second
  listing. An archive that is neither encrypted nor signed is restored without a
  key, but the daemon adds a **warning** to the restore job's `warnings` (shown
  by the web UI), and the CLI **refuses** it unless `--no-key` is given, with a
  message that names `--no-key` (and `--keyfile` for an older key generation).
- With encryption off the behaviour is unchanged (`Auto`).

### Revision 2026-11-10: a log line, and a missing archive is not "unprotected"

- When the daemon restores an archive that is not protected by the key while
  encryption is configured, it also writes one line to its log (the journal)
  with the snapshot, the archive and who asked, besides the warning in the job.
- An archive that is not in the snapshot's file list at all is no longer called
  "not protected": the CLI says that the archive is not in the snapshot, and
  the daemon's restore job fails with that message. An archive that is listed
  without a crypt mode (an older server) gets its own message in the CLI too.

## Update 2026-11-13: archives sourced below a user's home

A recorded source may also be the home itself or any directory below it.
For example, source `/home/alice/Documents` grants alice the entire
`docs.pxar` archive: `/report.txt` in the archive means
`/home/alice/Documents/report.txt` at backup time. Browsing starts at `/`,
and a whole-archive restore copies its contents directly into the new
target, without adding a `home/alice/Documents` prefix.

This mapping is shared by PBS and desktop stick browsing/restoring. The
source still comes only from trusted snapshot metadata, never the request,
current configuration or mutable notes. The PBS legacy run-record fallback
keeps its previous ancestor/home-only mapping: configured paths below the
home could have been symlinks to system files at backup time. Only snapshot
metadata whose `bound_sources` includes the archive can grant this new
descendant scope; a canonical path alone is not evidence that the
client read that directory.
Comparisons use path components, require an absolute source and refuse `..`; `/home/alicex` and other users' homes do not qualify.
The source need not exist today. Home ownership checks, normalized archive
paths, signature verification and copying as the caller remain in force.
Archives sourced above the home still expose only the branch towards the
home, and archives outside it remain refused.

The UI keeps the refusal for each folder. It distinguishes archives without
home files from snapshots whose source cannot be established safely and
explains the administrator's `sudo nestegg restore` route in German and
English. A missing source proof is not described as missing user files.

## Update 2026-11-13: bind recorded sources to directory handles

Before metadata is written, every configured source is canonicalized, accepting
symlinks in the configuration as before, then opened component by component
from `/` with `O_NOFOLLOW | O_DIRECTORY`. Missing sources and symlinks introduced
after canonicalization fail closed (optional sources are skipped as incomplete).
There is no raw-path fallback. The recorded path comes from the opened
handle's `/proc/self/fd` link. Before recording it, the path is re-opened
without symlinks and its device and inode must match the original handle.
This rejects misleading paths from detached mounts (for example after a
lazy unmount); any failure skips an optional source as incomplete or fails a
required source. The client inherits a duplicate of the original
handle and reads `/proc/self/fd/N`, including for `system.pxar` (`/`). It
never receives the configured path for a second lookup. Renaming a source
and replacing it with a symlink after recording therefore cannot change
which root directory is uploaded. Descendant symlinks remain ordinary pxar
symlink entries; this pins archive roots, not a frozen filesystem snapshot.

`bound_sources` is an optional set of archive names in the signed metadata.
Only handle-bound sources grant the new below-home permission, for both PBS
and desktop stick requests. Older metadata without this field and legacy
local records retain the ancestor/home-only mapping. Existing snapshots
without the binding require root to restore a below-home source. Signing a
canonical path authenticates the recorded claim; it does not establish the
identity of the uploaded directory.

User file restores exclude the client's synthetic `/.pxarexclude-cli` at
the user-restore boundary. Rescue, inventory and read-back extraction keep
the client's ordinary full-archive behavior. Scope refusals carry stable
`reason` values (`restore_no_home`, `restore_source_unverified`,
`restore_outside_home`) with the existing `forbidden` code. PBS and stick
share these reasons; the UI does not infer them from English text.

## Alternatives rejected

- **Keep using notes, sign them with a key on this computer:** another key
  to manage and protect; the blob is already covered by the client's
  encryption and the manifest.
- **Use the current configuration's sources:** it may have changed since
  the snapshot was made; a moved source would map a home directory onto
  the wrong archive path.
- **Only the local run records:** lost on a reinstall, pruned after
  `keep_runs` runs; the blob travels with the snapshot.
- **Fall back to the notes for old snapshots:** that is the hole this
  decision closes.
