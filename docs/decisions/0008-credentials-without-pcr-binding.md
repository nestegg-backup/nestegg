# 0008 – Credentials sealed to the TPM without PCR binding

- Status: accepted
- Date: 2026-11-09
- Changes: decision 0003 (row "Credentials": what "TPM-bound" means)

## Context

nestegg stores the PBS token secret and the encryption key as `systemd-creds`
credentials (`/etc/nestegg/credentials/*.cred`). It called
`systemd-creds encrypt` without `--tpm2-pcrs`. On a computer with a TPM2,
systemd then seals the credential to the TPM **and to PCR 7**, which measures
the Secure Boot state and its key databases.

PCR 7 changes in ordinary situations:

- Secure Boot is switched off to boot the rescue stick and left off (or
  switched on again with different settings).
- A firmware or `dbx` update arrives through fwupd or the vendor.
- The system disk is restored onto a replacement mainboard (new TPM; this
  case breaks any TPM binding, with or without PCRs).

Afterwards every `.cred` file failed to decrypt. Every hourly backup failed
with a generic internal error, and the only supported way out was
`nestegg key create --force`: a new key, a full re-upload without
deduplication, and older snapshots that the web UI could no longer restore.
There was no way to put back the exported key file the user already had.

The threat PCR binding addresses (someone boots a different OS on the same
hardware and reads the disk) is not what nestegg's secrets need to resist:
`/etc/nestegg` is root-only, the token can only create backups, and the
encryption key must exist outside the computer anyway (backups refuse to run
until it was exported). Users who need protection against offline attacks
use full-disk encryption, which also covers these files.

## Decision

1. **Seal to the TPM, but to no PCR.** nestegg calls
   `systemd-creds encrypt --name=<name> --tpm2-pcrs= - <file>`. The empty
   list is documented ("binds the encryption key to no PCRs at all") and
   exists since systemd 250, the first release that has `systemd-creds` at
   all. `--with-key` stays at systemd's default `auto`: TPM2 plus host key
   when a TPM2 is present, host key only otherwise. Verified on systemd 255:
   the credential header carries PCR mask 0 instead of 0x80 (PCR 7).
2. **Fallback: host key only.** If the TPM exists but cannot be used (driver
   problem, device not accessible), `auto` fails; nestegg then retries with
   `--with-key=host` and says so. The secret is still encrypted at rest.
   Only if `systemd-creds` is missing or fails completely does nestegg use a
   root-only plain file, and only with `--allow-plain` (unchanged).
   Ubuntu 22.04 / Linux Mint 21 ship systemd 249 without `systemd-creds` and
   therefore use the plain file.
3. **`nestegg key import <file>`** stores an exported key again. It checks
   the file with `proxmox-backup-client key show`, accepts only keys without
   a password, stores it through the same path as `key create`, records
   fingerprint and digest, and marks it as exported (the user holds the
   file; narrowed by decision 0018: only for a file on removable media, and
   importing the key that is already stored keeps the earlier confirmation
   instead of creating one). It refuses to replace a different key without `--force`; the key
   whose fingerprint is already recorded can be imported without it, which
   is exactly the "stored copy unreadable after new hardware" case.
4. **A dedicated error kind `credentials_unreadable`.** A `.cred` file that
   exists but does not decrypt is reported with this kind (API, UI, run
   records) and a message naming the fix:
   `sudo nestegg credentials set-password` and
   `sudo nestegg key import <exported key file>`.

## Rejected alternatives

- **Keep the PCR 7 binding.** A firmware update must not cut a backup tool
  off from its own key; that failure arrives without warning, on every
  machine at once after a vendor update, and in exactly the situation
  (rescue) where the backups matter most.
- **`--with-key=host` only.** Works after a mainboard change (in system mode
  the host key is part of the backup), but drops the TPM entirely: a copy of the disk
  alone would then decrypt the credentials. With the TPM sealing, a stolen
  disk without its computer is not enough, and the mainboard case is covered
  by `credentials set-password` plus `key import`.
- **A signed PCR policy (`--tpm2-public-key`, systemd 252+).** Needs signed
  unified kernel images, which the target distributions do not use, and
  does not exist on systemd 250/251.

## Consequences

- Secure Boot changes and firmware updates no longer affect backups.
- A new mainboard, a TPM reset or a restore onto other hardware still makes
  the credentials unreadable. The run then fails with
  `credentials_unreadable`; re-entering the token secret and importing the
  exported key repairs it without a new key. docs/rescue.md describes this.
- If a system has `/etc/systemd/tpm2-pcr-public-key.pem` (signed-UKI setups),
  systemd 252+ additionally binds new credentials to that signed policy by
  default. nestegg does not override it, because the matching options do not
  exist on systemd 250/251; such setups opted into PCR policies on purpose.
- nestegg is unreleased, so existing credentials are not migrated.
  Development installations re-run `nestegg credentials set-password` and
  `nestegg key import` once (docs/configuration.md).
