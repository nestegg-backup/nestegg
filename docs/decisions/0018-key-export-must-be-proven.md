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
is the `data` field of the key file. It is printed on the paper key only,
never by any command, log or status page. Typing it later proves that the
user holds that sheet. Without `--output`, a terminal on stdout is refused
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
A copy is on removable media if any layer is removable (`RM`), is on the USB
or IEEE 1394 transport, or is hot-pluggable on a bus that is not an internal
one (SATA/ATA/NVMe/SAS/MMC controllers often flag every port hot-pluggable).
Everything else, including what cannot be decided (RAM disk, network share,
overlay file system, tools missing), counts as a disk of this computer: the
cost of a wrong guess is one more question, not a lost key.

The state keeps `key_last_exported_digest`; it is still recorded by `key
export` and `key paperkey` but no longer decides anything.

## Consequences

- A confirmation means that a copy of exactly this key was shown to the
  program, or that the user could read the check code off a sheet of paper
  made from it.
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
