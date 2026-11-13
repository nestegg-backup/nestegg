# Rescue stick: restore a computer onto an empty disk

When the system disk has died, or a computer has to be set up again from
scratch, the nestegg rescue stick puts the complete system back from the
Proxmox Backup Server or the backup stick itself: partitions, files, users,
settings, boot loader. A backup stick can also back up a computer whose own
system no longer starts, with all source disks kept read-only.

Decision and scope: [decisions/0006](decisions/0006-bare-metal-restore-rescue-stick.md),
target disk guard: [decisions/0010](decisions/0010-rescue-target-disk-guard.md),
paper key and certificate question: [decisions/0019](decisions/0019-rescue-paper-key-and-fingerprint-prompt.md).

## Before you need it

1. **Full system backups must be on.** New installations have
   `system = true` in `[backup]` (see `nestegg config`). Older
   configurations: add it and run one backup.
2. **Keep the key and the settings outside the computer**, e.g. on a second
   USB stick, and prove to nestegg that the copy is the key:

   ```sh
   sudo nestegg key export /media/usb/nestegg-key.json
   sudo nestegg key confirm-exported --file /media/usb/nestegg-key.json
   sudo nestegg rescue export-settings /media/usb/nestegg-rescue.toml
   ```

   A printed paper key (`sudo nestegg key paperkey --format html --output
   key.html`; print it, then delete the file) is the second copy to keep at
   home; its check code (`XXXX-XXXX`, top of the sheet) is what
   `confirm-exported --paper-code <code> --printed` asks for, together with
   your statement that the sheet is printed and stored away from the computer. Check the copies from time to time with
   `sudo nestegg key verify <copy>`. See
   [configuration](configuration.md#exporting-and-proving-the-key).

   The settings file contains the PBS address, datastore, namespace, token
   name and certificate fingerprint, never a password. You also need the
   token secret (password manager).

   The key can also be the **paper key**: `sudo nestegg key paperkey` prints
   it, `sudo nestegg key paperkey --output paperkey.txt` (or `--format html`)
   saves it as a file you can keep on the stick. The wizard takes either form
   (see "The encryption key" below). Keys protected with a password are not
   supported; nestegg's own keys have none.
3. **Write the rescue image to a USB stick** (all data on the stick is lost):

   ```sh
   sha256sum -c nestegg-rescue-amd64.iso.sha256
   sudo dd if=nestegg-rescue-amd64.iso of=/dev/sdX bs=4M conv=fsync status=progress
   ```

   Any tool for writing ISO images works (GNOME Disks, balenaEtcher, Ventoy).

## Restoring

1. Boot the computer from the stick (boot menu key, often F12, F11 or Esc).
   Secure Boot may need to be switched off for the stick. Switching it on
   again afterwards is fine either way: nestegg's stored secrets do not
   depend on the Secure Boot state.
2. The wizard starts by itself. Plug in a network cable, or leave the wizard
   with Ctrl+C, run `nmtui` for Wi-Fi and start it again with `nestegg rescue`.
3. Plug in the stick with the key and settings; the wizard finds them
   ("Looking for settings and keys on the attached drives ...") and names
   the settings file it uses. When several are found, it lists them and
   you choose one (or none). It looks at other disks read-only, without
   replaying journals, so nothing is written to them. It searches the top
   three directory levels of every partition and only opens files with
   the names of key files and paper keys (`.json`, `.txt`, `.html`, …, not
   files without extension), at most 64 files and 8 MiB per partition; a
   key elsewhere is given by its path.
4. Answer the questions: server, token secret, key, computer, backup,
   target disk, optional encryption. The wizard prints the fingerprint of
   the certificate the PBS shows and asks "Trust this certificate?".
   Compare it with the PBS dashboard ("Show Fingerprint"); only an
   explicit `y` trusts it, Enter means no. This also holds when the
   settings file has the same fingerprint: the wizard says which file it
   matches, but any attached drive can carry such a file, so it does not
   decide alone. When the fingerprint differs from the settings file, the
   wizard says so in a warning. Once trusted, the certificate is not asked
   about again in the same run. Answer no only if the PBS has a
   certificate from a trusted authority. The list of backups says for each
   one whether it is `encrypted` or `NOT ENCRYPTED`. The disk list shows model, size and
   serial number of every disk and marks it `empty`, `NOT EMPTY` or
   `HOLDS DATA OF THE BACKED-UP COMPUTER`, with what was found on it (see
   below). A disk is pre-selected only if exactly one disk is empty and big
   enough; otherwise you have to type its number.
   The key is checked against the backup you choose before the list of disks
   appears (see below).
5. Check the plan and type the disk name (e.g. `nvme0n1`) to start. Only now
   is the disk erased. The plan lists what is currently on the disk. A disk
   that is not empty needs a second confirmation: type `ERASE <name>`
   (e.g. `ERASE sdb`). Disks that are mounted, used as swap, or in use by
   active LVM, encryption or RAID are refused.
6. Remove the stick and restart.

The restored system contains a report of what was changed:
`/var/lib/nestegg/rescue-report.txt`. The previous versions of modified
disk/boot configuration files are kept below `/var/lib/nestegg/rescue-backup/`.
Network file backups are hidden copies next to the original, named
`.FILE.nestegg-rescue.bak` (with a unique suffix if a copy already exists);
their original permissions are preserved and
network renderers do not load these copies.

### Networking on replacement hardware

Rescue compares MAC-pinned selectors in netplan, NetworkManager keyfiles,
systemd `.link` files, `/etc/udev/rules.d/70-persistent-net.rules`, and
ifupdown interfaces files with the live machine's interfaces. It automatically
changes a pin only when there is exactly one old MAC across the discovered wired
selectors, that MAC is absent here, other selectors are clearly Wi-Fi, and
there is exactly one physical wired NIC. Literal block-style netplan
`ethernets`/`match` selectors, Ethernet NetworkManager profiles and `.link`
matches explicitly restricted to `Type=ether` are supported for repair.
Only MAC selector bytes change: Wi-Fi passwords and other settings are preserved.

Other cases are detection-only, including complex YAML, Wi-Fi, multiple
NICs/MACs, udev rules and ifupdown `hwaddress` (which can set an address
rather than select hardware). The final wizard/CLI summary and the report
name the selectors found, whether they were changed, and the manual fix.
Compare `ip -br link` after boot with the reported file; correct the MAC and
any interface-name references (`set-name`, profile interface name, interfaces
stanzas), then apply the configuration or reboot. Automatic MAC repair alone
cannot prove that every name or external network dependency is correct.

**Cloud-init can regenerate netplan at boot.** Rescue does not change its
policy or datasource. Update the datasource's network configuration as well,
or deliberately disable cloud-init network rendering with
`network: {config: disabled}` in a cloud configuration drop-in after reviewing
the generated network configuration. A repaired generated file alone is not
a durable override of a datasource that still specifies the old hardware.

### Status after a restore

Before the first boot, rescue records the selected snapshot and its timestamp,
clears the captured current-run record and runtime lock files, and treats that
snapshot as the last successful backup of its source target. Health and the
next due time use the **snapshot time**, not the restore time: restoring an
old backup can immediately be overdue. This does not claim a new upload or
read-back verification. Encryption-key export confirmation and unrelated
target history remain intact.

`nestegg status` shows the restore origin. JSON/API status adds nullable
`restored_from` with `snapshot`, `snapshot_time`, `restored_at` and `target`
(`pbs` or `stick`); it persists when later backups complete. Older state
without this field continues to work. See decision 0030.

### The encryption key

At the question "Encryption key" you can give:

- **A key file or a saved paper key**: the path, or just Enter when the
  wizard found one on the attached media (it lists them, numbered; the
  number works as well, e.g. for a file whose name has characters that
  cannot be typed, which the console shows as `?`). Both exported key
  files (`nestegg key export`) and paper keys saved as text or html
  (`nestegg key paperkey --output …`) work; only the part between `-----BEGIN
  PROXMOX BACKUP KEY-----` and `-----END PROXMOX BACKUP KEY-----` is read, a
  header above it and the QR code below it are ignored. A file with several
  key blocks is used with its first valid one; the wizard says how many it
  found and which one it uses. The `fingerprint` line is not needed and not
  used (the client computes it, and a damaged copy of it would spoil the
  key).
- **`type`**: type the paper key by hand. Only one line is needed, the
  `data` line:

  ```text
  "data": "RVhBTVBMRS1LRVktTk9ULUEtUkVBTC1LRVktMDEyMzQ=",
  ```

  Type the 44 characters between the quotes. Upper and lower case matter, so
  do `+`, `/` and the `=` at the end. Quotes, commas and blanks you add are
  ignored. The text stays visible while you type so you can compare it with
  the paper; most mistyped letters are not detected by the wizard itself
  but by the check below. A wrong last character before the `=` is found
  at once: it must be one of `A E I M Q U Y c g k o s w 0 4 8`. Mind `l` (small L), `I` (capital i) and `1`, and `O` and
  `0`. The other lines of the paper key (`kdf`, `created`, `modified`,
  `fingerprint`) are not needed.
- **`-`**: the backup is not encrypted.

**The key is checked before anything is erased.** After you choose the
backup, the wizard reads its manifest with the key: the client verifies the
manifest's signature with the key, and every archive that will be restored
(`inventory.pxar` and `system.pxar`) must be encrypted according to that
verified manifest. Only then it says "The key fits this backup" and reads
the disk layout. If the key does not fit, it shows the message of the
client (it names the fingerprint of the key the backup was made with and
the one you gave), says "Nothing was changed" and offers: enter or select
another key, choose another backup (an older backup may have been made with
an older key), try again, or quit. The pre-selected answer is the one that
can help: another key after a key problem, "try again" after a network or
server error, and "choose another backup" for a backup that can never be
restored (for example one that mixes encrypted and unencrypted archives, or
that has no disk layout; the wizard says so) and for a backup you declined.
The disk list comes only after this check passed. `nestegg rescue restore` does the same check and stops before
changing anything.

**A backup your key does not protect needs an explicit yes.** When you gave
a key but the chosen backup is not encrypted and signed with it, the wizard
warns: "this backup is NOT encrypted and NOT signed, so it is not protected
by your key: anyone with access to the backup server could have changed
it" (or names the archives that are only signed). Someone with write access
to the PBS could push such a backup, newer than yours, with a manipulated
system; the wizard pre-selects the newest backup. "Restore this backup
anyway?" defaults to no; then choose another backup. `nestegg rescue
restore` refuses such a backup unless `--allow-unprotected-backup` is
given. Without any key (`-`), unencrypted backups are restored as before.
A backup that is signed with your key but has an archive that is not even
signed (neither encrypted nor signed) is refused in every case; of the
archives of a split `system` archive the weakest one counts.

Names that come from attached drives or from the backup server (file names
of keys, disk labels and models, messages of the client) are shown with
control characters replaced by `?`, so they cannot rewrite lines on the
console.

### Keyboard layout

The rescue system starts with a US keyboard layout. On other keyboards
`+`, `/`, `=`, quotes and special characters sit on other keys, which matters
for the paper key, the token secret and the disk passphrase. The wizard shows
what you type for the paper key; passwords are hidden. To use another layout,
leave the wizard with Ctrl+C, then:

```sh
sed -i 's/^XKBLAYOUT=.*/XKBLAYOUT="de"/' /etc/default/keyboard   # your layout
setupcon -k --force
nestegg rescue
```

Checked in the rescue system (booted in a VM): after these commands the
virtual console maps the key `Z` of the US layout to `Y`, as the German
layout does. Typing on a physical keyboard was not tried.

### After restoring onto new hardware

The restored system brings back nestegg's stored token secret and key, but
they are sealed to the TPM of the computer that made the backup. On the same
computer (new disk only) they keep working. On a new mainboard or another
computer they cannot be decrypted, and backups report "stored secrets
unreadable". Run `sudo nestegg backup` once to find out, and if it reports
that, store them again with the key file and token secret you used for the
restore:

```sh
sudo nestegg credentials set-password                  # token secret
sudo nestegg key import /media/usb/nestegg-key.json    # same key, no new upload of old data
sudo nestegg backup
```

Importing the same key keeps deduplication against the old snapshots and
keeps them restorable from the web UI. Do not use `nestegg key create
--force` for this: a new key means a full upload and old snapshots need the
old key file.

### Without the wizard

```sh
nestegg rescue disks
nestegg rescue restore --settings /media/usb/nestegg-rescue.toml \
  --password-file /root/token --keyfile /media/usb/nestegg-key.json \
  --disk /dev/nvme0n1 --wipe /dev/nvme0n1 [--snapshot <path>|latest] \
  [--backup-id <other machine>] [--encrypt-passphrase-file /root/pass] [--dry-run]
```

`--keyfile` takes an exported key file or a saved paper key. With
`--keyfile`, a backup that is not encrypted and signed with that key is
refused unless `--allow-unprotected-backup` is given (see above). `--wipe`
must repeat the disk; `--dry-run` only prints the plan. A disk that
is not empty is refused unless `--erase-data-disk` is given.

### When is a disk "not empty"?

A disk counts as not empty when it, or a partition on it, carries any
signature that `lsblk`/`blkid` recognise (a file system such as ext4, ntfs
or vfat, swap, a software RAID member, a ZFS pool member, an LVM physical
volume, a LUKS or BitLocker container), or when it has partitions without a
recognisable signature (unknown or encrypted content). A partition table
without partitions counts as empty.

It is additionally marked as holding data **of the backed-up computer** when
one of its file system, LUKS or partition UUIDs appears in the snapshot's
inventory: `layout.json` (mounted file systems and swap), `fstab` and
`crypttab` (also unmounted or `noauto` entries) and the `lsblk` listing
(RAID and ZFS members, ntfs-3g disks, ...). The message says what it was,
e.g. that it held `/home` (it comes back from the backup, but everything
written after the backup is lost) or a data disk that is not part of the
system backup at all. The stick with the key or settings file is marked as
well. Software RAID arrays are not assembled by the rescue system; their
members are recognised by their signature only.

## What the restore does

| Old system | Restored system |
|---|---|
| UEFI or BIOS | Boot mode of the rescue boot (a change is allowed and noted) |
| EFI system partition | New 1 GiB ESP at the same mount point |
| Root on ext4, btrfs or xfs | Same type on one partition (ext2/3 become ext4) |
| btrfs subvolumes (`@`, `@home`, …) | Recreated with the same names; snapshot subvolumes empty |
| Separate `/home`, `/boot`, `/var` partitions | Merged into `/` |
| Swap partition / swap file | Recreated (capped at 16 GiB); with encryption a swap partition becomes a swap file (on btrfs in its own subvolume `@swap`, so snapshots keep working) |
| LUKS encryption | Only if chosen in the wizard: new LUKS2 root, unencrypted `/boot` |
| LVM | Not recreated (plain partitions) |
| Bind mounts (`/srv/data` from `/home/alice/data`) | fstab line kept as it is |
| Data disks, other partitions in fstab | Untouched; fstab entry gets `nofail` |

Then fstab, crypttab, resume device and GRUB settings are pointed to the new
UUIDs, the initramfs is rebuilt and GRUB is installed (UEFI: also in the
fallback path `EFI/BOOT/BOOTX64.EFI`, so the disk boots even when the firmware
loses its boot entries). On BIOS computers the new disk is also recorded
for later GRUB package updates. New file systems are created without the
newest ext4/xfs features, so older systems (Ubuntu 22.04, Linux Mint 21,
Debian 12) can boot and check them.

Not part of the full system backup and therefore not restored:
`/tmp`, `/var/tmp`, downloaded `.deb` files, swap files, excluded caches
(`backup.excludes`), and data disks (restore those with `nestegg restore`).

## Limits

- Debian-based systems with GRUB. Systems with systemd-boot, ZFS or bcachefs
  root, and backups without a Debian package list (Fedora, Arch, …), are
  refused before anything is erased.
- The target disk must hold the used data plus about 15 % (at least 2 GiB).
  On btrfs the size of `system.pxar` counts, not the space used by
  snapshots.
- btrfs subvolumes that are not mounted (e.g. `~/Projects` created as a
  subvolume) are backed up and come back as normal folders. Docker's and
  container subvolumes and snapshots are left out.

## Building the stick yourself

```sh
cargo build --release
packaging/rescue/build.sh target/release/nestegg target/nestegg-rescue-amd64.iso
```

Needs Docker (privileged container with Debian `live-build`) and network
access. The end-to-end test boots everything in VMs and drives the wizard over the
serial console, including wrong answers. The first variant gives the key as a
saved paper-key file on the media, the second types it in; both first try a
wrong key (which must be refused with the target disk untouched), check
that Enter does not trust the certificate even when the settings file on
the stick has its fingerprint, and that a second settings file with another
fingerprint is pointed out. Before the rescue VM starts, the test pushes two
newer backups of the same computer to the PBS: one that is only signed with
the right key (`--crypt-mode sign-only`, with the real disk layout) and an
unencrypted "decoy" (the newest). Both must be refused (non-interactive) or
declined by default (wizard) while a key is given; the signed one is
restored only with `--allow-unprotected-backup` (the test does a dry run),
and the wizard must say that a backup without disk layout cannot be restored
and offer "choose another backup" by default. A backup that mixes crypt modes
cannot be made with the client (one mode per run); that case is covered by
unit tests. `WIZARD=0` tests a complete non-interactive `nestegg rescue restore`
instead.

```sh
sudo tests/rescue/run.sh target/release/nestegg target/nestegg-rescue-amd64.iso debian-btrfs-uefi
sudo tests/rescue/run.sh target/release/nestegg target/nestegg-rescue-amd64.iso ubuntu-ext4-bios-luks
sudo WIZARD=0 tests/rescue/run.sh target/release/nestegg target/nestegg-rescue-amd64.iso ubuntu-ext4-bios-luks
```

CI runs these three weekly, on pull requests that touch the rescue code
(`.github/workflows/rescue.yml`) and before every release.

## Backups on this stick

When booted from a nestegg backup stick, `nestegg rescue` offers **Backups on
this stick**. Choose the computer, open its protected key copy with its password
(or use the paper key), and choose a snapshot. The existing restore plan and
final disk-erasure confirmation apply. This path needs no network, PBS login
or certificate approval. An ordinary rescue ISO still restores from a PBS.
The medium is detected by its pinned boot ESP and sibling data partition;
another attached drive with the same label is never selected automatically.

For automation:

```sh
nestegg rescue restore --source stick --backup-id desktop \
  --key-password-file /run/stick-key-password \
  --disk /dev/nvme0n1 --wipe /dev/nvme0n1
```

Use `--keyfile /media/paperkey.txt` instead of `--key-password-file` to recover
with an exported or saved paper key. `--password-file` is only for the remote
PBS source. Keep passwords in RAM with mode 0600; never put them in arguments.

In the live system, the local PBS image is the already booted squashfs at
`/run/live/medium/live/a/filesystem.squashfs` (or slot `b`). nestegg checks the
kernel boot arguments, the read-only ESP and the live root loop's backing file
before using it. The desktop continues to use only its checked fixed cache at
`/var/lib/nestegg/stick/pbs.squashfs`; attaching a stick cannot supply desktop
executable code. In both cases the same private, loopback-only PBS launcher is
used and stopped before the data partition is unmounted.

## Back up a computer that cannot start

Boot its backup stick and choose **Back up this computer**. Select the source
disk, unlock LUKS if necessary, confirm the computer name and choose its stick
key, paper key, or a new key. A new key requires a strong password for its
recovery copy on the stick; keep the offered paper key elsewhere too. Confirm
the displayed UTC clock against the newest snapshot and correct it if needed.

The computer's disks remain read-only. Dirty journals are recovered into a
bounded copy-on-write layer in RAM. Running out of RAM/COW capacity is a failed
backup and never permits pruning. No filesystem repair, source executable,
swap activation or hardware-clock write is performed. Inventory, package
selections, Snap/Flatpak state and disk layout describe the mounted computer.
Missing system mounts or an unsupported storage layout stop the backup.
Supported sources are ordinary ext2/3/4, xfs and btrfs filesystems, optional LUKS
and linear LVM volumes on the selected disk. RAID, thin/cache LVM, multi-disk
systems, external filesystem journals and system bind mounts need manual
recovery and are refused. ext2 must already be clean because it has no journal.
Btrfs mounts selected only by `subvolid=` are refused before upload; named
`subvol=` mounts use the same system/snapshot scope as normal desktop backups.

The non-interactive equivalent is `nestegg rescue backup --disk /dev/nvme0n1
--backup-id desktop --key-password-file /run/stick-key-password --confirm-clock`.
Use `--luks-passphrase-file` for automation, `--root-device` when more than one
installed system is detected, and `--time` for an explicit RFC3339 UTC time.
`--new-key-password-file` creates a new key and its protected recovery copy.
Every completed backup is read back, verified by PBS, then pruned using the
usual stick retention. Keep the stick attached until the command finishes.

### Disposable VM acceptance

```sh
sudo tests/stick/rescue.sh target/release/nestegg target/nestegg-rescue-amd64.iso
```

The matrix covers a desktop backup to the stick and offline backups of plain
ext4, LUKS plus LVM, a VM killed while writing, a system without nestegg, and
Btrfs with an additional application-data subvolume. The Btrfs case checks
that the database witness survives and skipped snapshot subvolumes are recreated.
Each offline case compares the entire source image's SHA-256 before and after;
the dirty case also checks that journal recovery allocated COW sectors in RAM.
Every snapshot is restored onto an empty disk and the result must boot with the
original test file checksum and owner. The desktop API is exercised as a normal
group member, including refusal of system files. All stick/rescue VMs have no
network interface. The tests use generated credentials and disposable images,
never a physical USB drive, and remove owned loops, mounts, containers and VMs.
