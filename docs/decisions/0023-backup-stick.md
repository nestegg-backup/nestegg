# 0023 – Backup stick: back up to, boot from and restore from one USB drive

- Status: proposed for release; phases 0–5 implemented and tested in containers and VMs, phase 6 (real hardware) remains
- Date: 2026-11-11
- Changes: 0002 (nestegg also runs a PBS, locally and only for the stick),
  0006 (a second kind of stick), 0019 point 4 (password-protected key copy
  on the stick)

## Context

Today every backup goes to a Proxmox Backup Server. Households without a PBS
cannot use nestegg at all, and households with one have no simple copy that
lives outside the house. The maintainer wants one USB drive that does
everything:

1. the running desktop backs itself up onto it,
2. booted from the drive, the computer is restored onto an empty disk,
3. booted from the drive, a computer is backed up even when its own system
   no longer starts or nestegg was never installed on it.

The drive must work alone (no PBS anywhere) and also next to a PBS (second
copy). All three belong to the first release of the feature. A fresh
installation of a distribution without a backup is out of scope.

Facts this builds on (checked against the PBS documentation and source,
2026-11-11):

- `proxmox-backup-client` cannot write to a plain directory; it always talks
  to a PBS (localhost when no host is given).
- A datastore is a directory on ext4, xfs or zfs (not FAT; it needs 65538
  subdirectories in one directory). Since PBS 3.3 a PBS can reuse an
  existing datastore on a removable drive, found by device and relative
  path.
- `proxmox-backup-proxy` always listens on `[::]:8007`; there is no option
  for the address.
- Garbage collection has the datastore option `gc-atime-cutoff` (minutes,
  1–2880, default 1445).
- A password-protected key can be used without a terminal
  (`PBS_ENCRYPTION_PASSWORD_FD` and related variables), but
  `proxmox-backup-client key create --kdf scrypt` and `key change-passphrase`
  need a terminal.

## Phase-0 result (2026-11-11)

[The disposable loop/KVM spike](../research/backup-stick-spike.md) gives a
**GO for points 1–3 with privileged startup and kernel IPv6 support**. Ubuntu
24.04 and a Debian 13 VM started the trusted PBS squashfs with fresh RAM-only
configuration; encrypted backup/restore, owner preservation, maintenance jobs,
EIO recovery and removable-datastore reuse worked. SeaBIOS and OVMF booted a
UUID-pinned GPT medium. This is not feature acceptance; status stays proposed.

The proxy fails with `ipv6.disable=1` (no AF_INET6 support). Disabling only
interface IPv6 addresses is a different case and still works. The desktop must
probe kernel IPv6 socket support and report the stick target as unsupported
when absent, without changing host settings. A single mapped-root UID is also
insufficient: PBS needs its separate `backup` UID/GID. Rootless operation is not
part of this implementation; a subordinate-ID solution remains untested.

There was no real USB stick/SSD. Physical performance, endurance, firmware boot
quirks and actual unplug/power-loss recovery remain hardware gates in phase 6.
The measured EIO case required offline ext4 checking before successful verify
and decrypted restore; verify alone does not repair a filesystem.

## Decision

1. **The stick carries its own PBS for the booted case; the desktop uses its
   own copy.** The drive holds the rescue live system (Debian, now with
   `proxmox-backup-server` added) and an ext4 data partition with a PBS
   datastore in a fixed subdirectory (`nestegg/store/`, below a root-owned
   `nestegg/` mount anchor), so a real PBS can later
   reuse it as a removable datastore. When the desktop backs up to the
   stick, it starts the PBS from a copy of the live image kept on its own
   disk (`/var/lib/nestegg/stick/`), written by `stick create`, `stick
   update` or `stick add` from a root-owned release image on fixed local storage.
   SHA-256 verifies integrity, not authenticity; an independently obtained
   `--sha256` pin takes precedence over an adjacent checksum.
   **The desktop never runs code from the stick.** A stick is recognised by a
   UUID that anyone can copy, and its partitions can be rewritten on any
   computer, so running its system as root would hand root to whoever made
   the stick. No backup logic is reimplemented, and every backup stays a
   standard PBS snapshot (0002 points 2 and 5 still hold).
2. **Nothing but the datastore comes from the stick.** The PBS
   configuration (datastore entry, token, certificate) is created anew in a
   tmpfs at every start; the stick's data partition is bind-mounted at the
   datastore path only. A fixed token name (`nestegg@pbs!stick`) is used
   everywhere, because the server checks that the owner of a backup group
   stays the same. The certificate fingerprint is read from the freshly
   created `proxy.pem`. The writable image overlay is in tmpfs; `/run` must
   additionally be a real tmpfs because PBS checks its shared-memory storage.
   The config directory is a separate fresh tmpfs with the image's `backup`
   ownership. Preserve PBS's datastore directory permissions; restrict secret
   files individually instead of applying a restrictive global umask.
3. **The local PBS is never reachable from the network.** It runs in its own
   network namespace with only a loopback interface, plus mount and PID
   namespaces and a private `/dev`. The client joins only that network
   namespace and still sees the host's files. These namespaces keep the PBS
   off the network; they are not a security boundary against a malicious
   PBS, which is why point 1 keeps the code on the desktop. Phase 0 found
   that a single mapped-root user namespace cannot also run
   the `backup` identity. The launcher needs a privileged helper; rootless
   startup is not supported. Kernel IPv6 socket support is a prerequisite,
   including for IPv4 loopback clients; fail the host preflight clearly if it
   is absent. Do not alter the host's IPv6 configuration as a workaround.
   In the live image the PBS units, `ssh` and the mail service the PBS
   package pulls in are masked; the wizard starts the PBS the same way.
4. **nestegg writes the stick itself**: `nestegg stick create /dev/sdX` with
   the target disk guard of 0010 and a typed confirmation (GPT: BIOS boot
   partition, ESP with GRUB, two independent FAT32 live-slot partitions
   (0030), ext4 data partition
   `NESTEGG_DATA` made with `mkfs.ext4 -m 0`). The boot entry names its own
   medium (`live-media=` / UUID), so a second live stick is never picked.
   `nestegg stick update` writes the new live system into the free slot and
   publishes its checksum commit last (GRUB reads both slot generations), so a power cut or a pulled stick
   leaves the old system bootable. Backups are kept. An ISO written with
   `dd` or Ventoy remains the plain rescue stick of 0006 (restore from a PBS
   only). USB SSDs and external disks work the same; the documentation
   recommends a USB SSD of at least 128 GB, because cheap sticks are slow
   and wear out.
5. **Only registered drives are used automatically.** `nestegg stick add`
   records the data partition's UUID. Plugging in a registered drive starts
   `nestegg-stick-pbs@.service` (bound to the device with `BindsTo=`) and a
   backup when one is due, with a notification at the start and "safe to
   unplug" at the end. A udev rule hides the stick's partitions from the
   desktop's automounter (`UDISKS_IGNORE`). Unknown drives are never mounted
   by nestegg; the data partition is mounted `nodev,nosuid,noexec`. After
   unclean removal, check filesystem health and require an appropriate repair
   step before normal reuse; distinguish filesystem damage from failed PBS
   verification, which cannot repair ext4. Parsing
   a crafted ext4 file system remains a kernel risk, the same as any USB
   drive a desktop mounts.
6. **Targets.** The configuration may have a PBS, a stick, or both. Each
   target has its own last success, history, failure counter (0021) and
   overdue threshold (stick default: 7 days, because it is not always
   plugged in). "Stick not plugged in" is its own state like
   `pbs_unreachable`, not a failure. Both runs share the run lock; when the
   stick has just been plugged in, the stick goes first. Removing a target
   or raising the stick threshold counts as reducing the protection (0022).
   Status, web UI and notifications show both. Existing `[pbs]`
   configurations stay valid unchanged.
7. **Same guarantees on the stick.** Client-side encryption with the same
   per-machine key, the read-back of 0015, "incomplete" when something is
   left out, the full system backup with `layout.json`. Because no server
   verifies the stick on its own, nestegg runs the PBS verification of each
   new snapshot after the backup and a full verification of the stick once
   a month, so failing flash shows up as "damaged".
8. **The stick prunes itself.** After each backup nestegg prunes the
   machine's group (default: last 3, 7 daily, 4 weekly, 6 monthly) and runs
   garbage collection with a small `gc-atime-cutoff`, which is safe because
   nothing else writes to the stick at the same time. Keep PBS's atime safety
   check enabled; the cutoff still delays collection of newly orphaned chunks.
   Phase 2 must test actual space reclamation, not just successful GC startup.
   Before a backup it checks the space available to the PBS user; a stick that is too full
   fails with its own state ("stick full") instead of deleting more than the
   policy allows.
9. **One stick, several machines.** The stick can hold backups of every
   computer of the household. A backup into a group whose last snapshot was
   made with a different key is refused, so two computers with the same
   name never prune each other's backups.
10. **The key travels with the stick, protected by a password.** The stick
    holds a copy of each machine's key, encrypted with a password of a
    minimum strength the user chooses (`kdf` scrypt). Created through a
    pseudo-terminal or the official key crate, because the client's own
    command needs a terminal. The wizard asks for the password; the paper key
    remains the fallback. For a stick-only setup this copy, opened once with
    the password, counts as the proof of 0018, and nestegg asks for the
    password again from time to time so a forgotten one shows before it is
    needed. With a PBS configured, the existing proof stays required,
    because a stick kept at home burns together with the computer.
    Decision 0019 point 4 is relaxed for this copy only.
11. **Restore from the stick** is a new source in the rescue wizard
    ("Backups on this stick", detected automatically): no network, no token
    secret, no certificate question. Restores of single files in the web UI
    also work while the stick is plugged in.
12. **Backup from the booted stick** is a new choice in the wizard ("Back up
    this computer"):
    - It unlocks LUKS, activates LVM read-only (Ubuntu's encrypted default
      is LUKS with LVM) and mounts the system without writing to it: ext4
      with `noload`, xfs with `norecovery`, btrfs with `rescue=nologreplay`.
    - A file system that was not shut down cleanly (crash, power cut – the
      main reason to use this) gets a copy-on-write layer in RAM
      (dm-snapshot), so its journal is replayed into memory, never onto the
      disk.
    - `layout.json` and the inventory come from the mounted system
      (`dpkg --admindir`, `systemctl --root`, Snap and Flatpak lists from
      their state files).
    - The backup ID comes from the computer's nestegg configuration if there
      is one, otherwise from `/etc/hostname`, confirmed by the user. The key
      comes from the stick, the paper key, or is new.
    - The live system has no network time. The wizard shows the clock and
      the time of the newest snapshot of that computer, and asks for the
      correct time when the clock is earlier than that snapshot or obviously
      wrong, because the PBS refuses older backup times and a clock in the
      future spoils pruning.

## Plan

Each phase ends with tests that pass in CI or in VMs. Phase 0 decides
whether points 1–3 hold; if not, this decision is revisited before anything
else is built. The feature is released only after phase 6.

| Phase | Content | Done when |
|---|---|---|
| 0 – Spike | On Ubuntu 24.04 and Debian 13: start PBS from the live squashfs in namespaces with a fresh configuration (points 2, 3), also with `ipv6.disable=1` (the proxy has no IPv4 fallback) and in a user namespace. Back up, restore, prune, garbage collection and verification through it onto an ext4 loop device. Create and use a password-protected key without a terminal. Pull the drive during a run. Measure a full system backup onto a real stick and a USB SSD. Attach the data partition to a real PBS as a removable datastore. Boot a GPT stick on BIOS machines (`pmbr_boot`). | Results in `docs/research/backup-stick-spike.md`, go or no-go. |
| 1 – Writing the stick | PBS in the live image (pbs-no-subscription repository, masked units), `nestegg stick create / update / info / add`, desktop copy of the image, two live slots, guard and confirmation. | VM boots the stick in UEFI and BIOS mode; backups survive an update and an update cut off halfway. |
| 2 – Backup from the desktop | PBS unit bound to the device, stick target in the run pipeline, per-target state and failure counters, udev rules, safe-to-unplug, verification, pruning and garbage collection, "stick full", key-mismatch refusal, key copy on the stick, config migration. | End-to-end test with a loop device as stick (container), upgrade test from today's configuration. |
| 3 – Restore from the stick | Wizard source "this stick", password key, file restore in the web UI from a plugged-in stick. | `tests/rescue/run.sh` variant: desktop VM backs up to the stick, stick restores onto an empty disk, restored disk boots. |
| 4 – Backup from the booted stick | Wizard choice "back up this computer": LUKS, LVM, read-only and copy-on-write mounts, layout and inventory from a mounted root, backup ID, key and clock questions. | VM tests: back up a powered-off VM (also one with LUKS + LVM, one killed mid-write, one without nestegg) from the stick, restore it onto an empty disk, it boots. |
| 5 – UI and notifications | Two targets in status and history, "back up to stick now", fill level, stick overdue reminder, password reminder. | UI tests; checked in the browser. |
| 6 – Real hardware | Documentation, a real desktop with a real USB SSD: back up, boot, restore onto a spare disk, back up from the booted stick. | Recorded in `docs/research/`; status of this decision → accepted. |

## Implementation status

Phase one is specified in [0024](0024-stick-writing-and-launcher.md), with the
security and durability corrections in [0025](0025-stick-review-hardening.md).
Desktop phase two in [0026](0026-desktop-stick-target.md) builds on that hardened
format and launcher. Booted-stick restore and offline backup are specified
and verified in [0027](0027-offline-stick-rescue.md); desktop file restore is
available through the daemon API, with UI integration owned separately.
These are incremental acceptance results, not release or physical-hardware
acceptance.

## Consequences

- nestegg now ships and starts a PBS. It comes with the live image, and
  each desktop keeps a copy (about the size of the image) to run it from.
  The live image grows by the PBS packages.
- Whoever finds the stick cannot read file contents (client-side
  encryption), but sees machine names, snapshot times and sizes, and the
  plaintext snapshot notes with host name, operating system and source paths
  (0016). Encrypting the data partition with LUKS is possible later: the
  desktop can keep its LUKS key in systemd credentials (0008) and still back
  up without asking, only the booted stick would ask for the passphrase.
- A found stick allows guessing the key password offline; the minimum
  password strength is the protection. A forgotten password with no paper
  key loses the backups on the stick.
- A stick kept next to the computer does not protect against fire or theft.
  The documentation says so and suggests keeping it elsewhere, or two
  sticks in turn.
- The stick boots like the rescue stick of 0006: Secure Boot may have to be
  switched off. The image is checked against a supplied SHA-256 pin or adjacent checksum;
  an adjacent checksum alone does not authenticate it. Signed images are a
  later improvement.
- The PBS is AGPL-3.0 like nestegg; the image ships its packages unchanged
  from the Proxmox repository, with the usual source offer.
