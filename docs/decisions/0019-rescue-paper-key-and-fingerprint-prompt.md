# 0019 – Rescue wizard: paper keys, key check before erasing, no default "trust"

- Status: accepted
- Date: 2026-11-09
- Changes: decision 0006 (rescue stick), key handling of the wizard

## Context

Three problems of the rescue wizard:

1. At "Does it match the fingerprint on the PBS dashboard?" the default
   answer was yes. Pressing Enter pinned whatever certificate the network
   presented, which defeats the question.
2. The wizard only read an exported key *file* (JSON). Users are told to
   print a paper key (`nestegg key paperkey`), which the stick could not
   use. The paper key is exactly the thing someone has in their hands when
   the computer is dead and the key file was never copied anywhere.
3. A wrong key was noticed when the disk layout of the chosen backup was
   read. That was before any disk was touched, but the error ended the
   wizard instead of letting the user try another key.

Findings with the real `proxmox-backup-client` 4.2.8 (Debian trixie and
Ubuntu 24.04 images):

- A key file with only `created`, `modified` (both RFC 3339 strings) and
  `data` restores backups; `kdf` and `fingerprint` may be missing. `data`
  is 44 base64 characters (32 bytes), not longer. So one line of the paper
  key is enough.
- A wrong key fails at the start of a restore, before any data is read:
  `wrong key - unable to verify signature since manifest's key e4:84:… does
  not match provided key 87:2f:…`; no key: `missing key - manifest was
  created with key …`.
- `proxmox-backup-client key paperkey` prints `-----BEGIN PROXMOX BACKUP
  KEY-----`, the pretty-printed key JSON, `-----END PROXMOX BACKUP KEY-----`
  and a QR code (text blocks, or an `<img>` in html output of about 180 KB).

## Decision

1. **No default for trust.** "Trust this certificate?" defaults to no; only
   `y`/`yes`/`j`/`ja` pins the fingerprint. A fingerprint equal to the one in
   the saved settings is still accepted without a question. The other yes/no
   questions were checked: "Use this disk anyway?" and "Encrypt the restored
   disk?" default to no; "Remove the rescue stick and restart now?" defaults
   to yes, since it neither trusts, erases nor overwrites anything. The
   decision lives in a function with the question injected, so a unit test
   presses Enter.
2. **Three ways to give the key**, all in the one "Encryption key" step:
   a path to an exported key file *or* a saved paper key (text or html), the
   word `type` to type the paper key by hand, `-` for an unencrypted backup.
   Media are searched for both kinds of files (`.json`, `.txt`, `.html`, … up
   to 1 MiB, two directory levels down). The parser (`rescue/paperkey.rs`)
   reads only what is between the markers (the first complete JSON object
   after the last BEGIN marker before an END marker, so header lines that
   mention the markers do not matter), tolerates CRLF, blanks and html
   entities, and accepts a missing END marker only when the key text itself
   is complete.
3. **Typing asks for the `data` line only** (44 characters). Quotes, commas,
   blanks and a leading `"data":` are ignored; a forgotten final `=` is
   added. Errors name the position of the first impossible character and the
   expected length. The input stays visible (unlike a passphrase): 44
   characters are easy to mistype, and the check against the backup only
   says "wrong", not where. The prompt warns about the US keyboard layout of
   the console and about look-alikes (`l I 1`, `O 0`). A pasted full JSON
   block is accepted when it arrives in one line.
4. **Only keys without password.** A key with a `kdf` (scrypt, pbkdf2) is
   refused with a message that says how to remove the password from a copy.
5. **The key is checked against the chosen backup before the disk step**,
   for every source of the key. The wizard reads `inventory.pxar` of the
   chosen snapshot with the key (the same read that provides the disk layout
   anyway), and prints "The key fits this backup". If that fails, it shows
   the client's message and a menu: enter or select another key (default for
   key problems), choose another backup, try again, quit. Nothing was
   changed at that point; no disk was listed yet. `rescue restore` already
   stopped at the same read.
6. **The test drives all of it** (`tests/rescue/run.sh`, wizard mode now the
   default): the debian variant gets the key as a saved paper-key file on the
   media, the ubuntu variant types the data line with quotes and a comma;
   each variant first gives a wrong key the other way round, and a wrong key
   with `--wipe` to the non-interactive restore must leave the target disk
   all zeros.

## Consequences

- A person with only the printed paper key can restore. They type 44
  characters instead of copying a file.
- A wrong key costs a menu choice, not a restart of the wizard.
- A key of a different generation is found out before the disk step, not
  during the restore. A backup that is not encrypted ignores any key (the
  wizard says so).
- The default of the wizard test changed to the interactive wizard; the
  non-interactive command is still available as `WIZARD=0`.

## Alternatives rejected

- Asking for the whole JSON block: a console takes one line per prompt, and
  most of the block (`created`, `modified`, `fingerprint`) is not needed.
- Showing a fingerprint of a typed key for comparison: it is derived with
  the key derivation of the client; reimplementing it would copy part of the
  client. The check against the backup is authoritative anyway.
- Hiding the typed key: makes typos invisible; shoulder-surfing a rescue
  console is a smaller risk than a key that cannot be corrected.
- Verifying with a separate request before choosing the backup: the key can
  only be checked against a snapshot, so it happens when one is chosen.
