# 0018 – The key export must be proven, not claimed

- Status: accepted
- Date: 2026-11-09

## Context

Backups are encrypted with a key that nestegg keeps on the computer. Without
that key every backup is worthless, so backups refuse to run until the user
ran `nestegg key confirm-exported`.

That command only checked that `key export` or `key paperkey` had run before.
Three gaps made the confirmation a claim instead of a fact:

- `key paperkey` without `--output` printed a QR code into the terminal. It
  scrolled away and counted as an export.
- `key export` into a folder on the computer's own disk left the only copy on
  the disk that is meant to be protected. Inside the backup the file is
  encrypted with the very key it contains.
- Nobody checked that the copy the user holds is the key at all: wrong file,
  empty file, a paper key printed from an older key.

## Decision

**Paper key.** `nestegg key paperkey` writes its own header, then the
unmodified output of `proxmox-backup-client key paperkey`. The header (plain
lines in the text format, a block at the start of `<body>` in the HTML format)
names the computer (backup id and host name), the date, the PBS server and
datastore and the key fingerprint, shows a **check code**, and says in three
lines how to use the sheet for a restore. The header stays outside the
`BEGIN/END PROXMOX BACKUP KEY` block, because readers of paper keys (the
rescue stick) only look between the markers.

The check code is the first 8 hex digits, upper case, of
`SHA-256("nestegg-paper-check:" + data)`, shown as `XXXX-XXXX`, where `data`
is the `data` field of the key file. It appears in the paper key output
only (the file written with `--output`, or what is piped to a printer),
never in another command, a log or the status page. Typing it later shows
that the user has that sheet (or its file) in front of them. Without `--output`, a terminal on stdout is refused
with a hint; a pipe (for example to a printer) is allowed.

**Export.** `nestegg key export <file>` reads the file back after writing (with
the rights of the invoking user, like the write) and compares it with the
stored key; any difference is an error and nothing is recorded. It then says
where the copy is. For a file on a disk of this computer it prints a warning
that the copy is lost together with the disk and is useless inside its own
backup, and asks to move it to a USB stick or a password manager.

**Confirmation needs proof.** `nestegg key confirm-exported` takes exactly one
of:

- `--file <path>`: a key file or a saved paper key (text or HTML, the JSON
  between the markers). The key material (`data` and `kdf`) is compared, not
  the bytes, because a paper key is pretty-printed. A copy on a disk of this
  computer needs a statement that another copy exists outside it: `yes`
  typed at a terminal, or `--accept-local-copy` without one. A copy on
  removable media needs neither.
- `--paper-code <XXXX-XXXX>`: the check code of the printed paper key. With a
  terminal and neither option the command asks which of the two the user has
  and prompts for the path or the code.

A wrong file or code is an error and nothing is recorded. A success is
recorded as before (`key_exported_digest`, bound to the stored key), so a key
confirmed by an older version stays confirmed after the upgrade; the upgrade
test checks that.

**Verification at any time.** `nestegg key verify <file>` repeats the
comparison and, if a snapshot of this computer exists and the PBS answers,
restores `index.json` of the newest snapshot with the copy as key file: the
client checks the manifest signature with the key, so this only works with
the key the snapshot was made with. It prints a verdict for both parts and
exits with 1 if the copy does not match or does not open the backup. A PBS
that does not answer, no snapshot yet, or an unencrypted newest snapshot are
reported as "not checked" and do not fail the command.

**Where is a file?** `findmnt` gives the device behind the path, `lsblk -s`
the layers below it (a LUKS volume on a USB stick has the stick below it).
Only the whole disks at the bottom count (`TYPE="disk"`), and all of them
must be external: a volume group, RAID or btrfs that spans the internal disk
and a USB disk may keep the file on the internal one. A disk is external if
it is removable (`RM`), is on the USB or IEEE 1394 transport, or is
hot-pluggable on a bus that is not an internal one (SATA/ATA/NVMe/SAS/MMC
controllers often flag every port hot-pluggable).
Everything else, including what cannot be decided (RAM disk, network share,
overlay file system, tools missing), counts as a disk of this computer: the
cost of a wrong guess is one more question, not a lost key.

The state keeps `key_last_exported_digest`; it is still recorded by `key
export` and `key paperkey` but no longer decides anything.

**`key import` is no way around the proof.** It used to mark every imported
key as exported ("you already have the file"), so `key export x` followed by
`key import x` confirmed a key whose only copy lay on the internal disk.
Now: importing the key that is already stored creates no confirmation (one
given earlier for exactly this key stays valid, which is the "stored copy
unreadable after new hardware" case of decision 0008); a new key counts as
exported only when the file comes from removable media, otherwise
`key confirm-exported` is needed like for any other copy.

**Which files are read.** A copy must be a regular file of at most 1 MiB;
FIFOs, terminals and devices are refused instead of blocking or filling the
memory. The place nestegg itself keeps the key in (the credentials and
temporary directories) is refused as a "copy". `key verify` reports "not
checked" while a backup is running, because the snapshot being written has
no manifest yet.

## Consequences

- A confirmation means that a copy of exactly this key was shown to the
  program, or that the user knows the check code of a paper key made from
  it and states that the sheet is printed and stored away from the computer
  (see the amendment of 2026-11-10: the code alone proves no sheet).
- The check code protects against a wrong or outdated sheet, not against
  someone who can read the stored key; it is not a secret and no
  authentication.
- Scripts that called `confirm-exported` without options break and must pass
  `--file <copy>` (plus `--accept-local-copy` if the copy is on the
  computer's disk). The three end-to-end scripts and the rescue test source
  were changed accordingly.
- The upgrade test calls the old package with the old syntax and the new one
  with the new syntax.

## Alternatives rejected

- **Keep the claim, add a warning.** Users click through warnings; the
  confirmation would stay meaningless.
- **Make the user retype the whole key.** Too error-prone; a check code and
  a file comparison prove the same thing.
- **Refuse every copy on an internal disk.** Password managers and encrypted
  vaults legitimately live on the same disk next to a second copy; the user
  can say so.
- **Check the copy against the PBS at confirmation time.** The server may be
  asleep or unreachable during setup, and before the first backup there is
  nothing to open. That is why `key verify` exists as a separate check.

## Amendment 2026-11-10: the paper code and "removable" must not claim more than they prove

A review of the confirmation found two statements that were taken as proof
and are not.

### The check code does not prove a printed sheet

`nestegg key paperkey --output ~/key.html`, opening the file on the screen
and typing the code (or `key paperkey | less`) was enough for `--paper-code`.
The code proves that the user saw the paper key, not where it is. A real
proof of "printed" is impossible, so the code is now treated like a local
file: it needs an explicit statement, and the one way that obviously
contradicts the statement is closed.

- **Statement.** `confirm-exported --paper-code` asks, at a terminal, to type
  `yes` to state that the sheet is printed and stored away from this
  computer. Without a terminal the flag `--printed` is required. No statement:
  nothing is recorded, and the message says why. (`--printed` is only valid
  with `--paper-code`; `--accept-local-copy` stays for `--file`.)
- **Remembered files.** `key paperkey --output <file>` records the canonical
  path of the file in the state (`paperkey_files`, with the digest of the key
  the sheet was made from; at most 50 entries, entries for other keys are
  dropped). The path is not secret. `confirm-exported --paper-code` is refused
  while one of these files still exists at a place that needs a statement
  (same rule as for `--file`: a disk of this computer, an unclassifiable
  place, or inside the backup). The message names the file and says to delete
  it after printing or to move it to a USB stick. A file that is gone, or
  lies on removable media, does not block. A wrong code is reported before
  this check. A paper key written to stdout leaves nothing to remember; the
  statement covers that case.
- **Message.** After writing, `key paperkey` says: print it, then delete the
  file (or move it to a stick); confirming is refused while the file lies on
  this computer.

What this does not catch: a copy of the file that the user made by hand
elsewhere on the same computer. That is the case the statement is for.

### "Removable" excludes what the backup contains

`DeviceFacts::is_external` only looks at the device. An external disk that is
itself a configured source, or any path inside what the backup contains,
exists only together with the data it protects, and inside the backup it is
encrypted with the very key it contains. New rule, for `key export`,
`key import` and `confirm-exported` alike: a path inside a configured source
(`backup.sources` and `backup.include_dev`, canonical paths) counts as on this
computer whatever the device is (`Location::InBackup`). In system mode this
also holds for everything on the file systems that go into `system.pxar`
(the root file system, the system mounts of the disk layout and bind mounts
of them; the deepest mount point above the path decides, so a stick mounted
below `/media` or a data disk with its own mount does not count as inside).
`key export` warns accordingly, `key import` of a new key from such a place
does not count as exported, and `confirm-exported --file` needs the statement.

The file is resolved (symlinks) before the comparison with the sources, so a
link on a USB stick to a file inside a source is recognised by where the data
really is. (`findmnt -T` already follows links when it finds the device; only
the comparison with the source paths needed the resolved path.) If the
configuration exists but cannot be read, or the
disk layout cannot be read in system mode, nestegg cannot tell whether a
removable copy lies inside the backup; it is then treated as not classifiable
(statement needed). No configuration at all means nothing is backed up, so
nothing is inside it.

### The limit: a stick that stays plugged in

A card or stick that simply stays in the computer cannot be told apart from
one that was removed after the copy was made, and it is lost or damaged
together with the computer. nestegg cannot fix that. After a successful
confirmation with a copy on removable media (a file, or a remembered paper
key file on a stick) it says plainly: now unplug it and keep it away from this
computer. The same sentence follows a `key import` that counts as exported.
The confirmation therefore means "a copy of exactly this key was on removable
media outside the backup when it was confirmed", not "it is somewhere safe
now".

### Other ways to "confirmed"

All code paths that can set `key_exported_digest` were checked: `key create`
and `key import` clear it, `key import` of the stored key keeps an earlier
confirmation only, `confirm-exported` is the only place that sets it
otherwise, and neither the daemon nor the web UI writes it. Closed in this
change: a link to a file inside a source (it would have passed a comparison
of path names); a copy on a removable medium
that is part of the backup (all three commands, including the `key import`
shortcut of a new key); `include_dev` paths (a backup content, not only
`backup.sources`); an unreadable configuration. Left as it is, on purpose:
`--file <paper key file> --accept-local-copy` (an explicit statement, like the
new one for the paper code).

### Consequences

- Scripts that confirmed with `--paper-code` and no terminal must add
  `--printed`, and must have deleted or moved the paper key file first.
  Scripts that use `--file` are unchanged.
- The state has one new field, `paperkey_files`; older state files load
  without it.

### After review (2026-11-10)

- `nestegg key paperkey > file` (output redirected to a file) is remembered
  like `--output file`; only output that goes to another program leaves
  nothing to check.
- What the remembered file can and cannot show: the check is "does the file
  still exist at the path it was written to". A file that was renamed,
  moved or hard-linked elsewhere on the same disk is not found. That is the
  same limit as for a copy made by hand, and the reason the statement is
  required in every case; the check only catches the common case of a file
  that was simply left where it was written.
- A paper key file whose name is not valid UTF-8 is refused before it is
  written (the path could not be kept in the state).
- `confirm-exported` looks at what the backup contains and which drives are
  mounted after the questions, so a stick that is plugged in while the
  prompt waits is seen.
- `--printed` cannot be combined with `--file`, nor `--accept-local-copy`
  with `--paper-code`.

## Amendment 2026-11-10 (second round): small points around the key commands

Found in a further review of the code; each was checked against the code
first.

- **A paper key file that cannot be deleted.** A remembered file on a
  read-only mount, or one the user may not delete, would block `--paper-code`
  for good. The refusal now names the way out: the file is a copy of the key,
  so `confirm-exported --file <that file> --accept-local-copy` accepts it with
  the same statement that another copy exists outside this computer. No new
  command removes the entry: that path already exists, needs no root-only
  state editing, and the statement is the same one the paper code needs.
- **The list of remembered files is never trimmed while the files exist.** It
  used to push out the oldest entry at 50, so a 51st file made the check
  "no paper key file left on this computer" miss a file that still lay there.
  Now `key paperkey` first drops entries whose file is gone (only a clear "no
  such file" counts; any other error counts as present), and if 50 existing
  files remain it refuses to write another one and lists them. The same test
  for existence is used by `confirm-exported`.
- **Standard output is examined before the key is written.** For `key
  paperkey > file` the shell has already created the file. nestegg now reads
  where descriptor 1 points before it renders anything: a regular file with a
  path that cannot be stored (not UTF-8) is refused with the key unwritten
  (the empty file the shell made is harmless). A terminal check, `/dev/null`,
  a pipe, a socket or a deleted file leave nothing to remember; the key is
  written as before.
- **Run lock.** The lock was already taken for the whole `key paperkey`
  command, which held it while the client rendered the paper key and while the
  output went into a pipe (a printer queue can take its time). It is now
  taken after the paper key is rendered and before anything is written, and
  released before a pipe gets the output. A running backup gives "a backup is
  running; try again when it has finished. Nothing was written." The stored
  key is read again under the lock; if it changed meanwhile, nothing is
  written.
- **A stick mounted below a source is not inside the backup.** Checked with
  the real tools: `pxar create` (the archiver `proxmox-backup-client backup`
  uses) prints "skipping mount point" for a file system mounted below the
  source and leaves its content out unless `--all-file-systems` is given,
  which for the client means `--include-dev`. nestegg's `mounts.rs` already
  relies on that. The rule for the configured sources is therefore the one the
  system mode already used: a path is inside the backup if it lies under a
  source (or `include_dev` path) with no other mount point between them. A
  stick mounted below `/home` is outside, as is a data disk that is not
  listed; a file system that is itself a source or `include_dev` entry is a
  root of its own. If the mount table cannot be read, everything below a
  source counts as inside (the previous behaviour).
- **Rescue console.** `show_network()` passes the output of `ip` through the
  same `printable` filter as the rest of the console.
