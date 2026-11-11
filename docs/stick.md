# Prepare a backup stick

Desktop backups to registered sticks are available. Preparing bootable media
and the isolated PBS launcher implement phases one and two of
[decision 0023](decisions/0023-backup-stick.md). Restoration from the stick's
local datastore and the two-target graphical UI are later phases. Booting the
stick currently still gives the existing [rescue system](rescue.md) for network
PBS restores.

Use an external USB SSD of at least 128 GB. Cheap flash sticks can be slow and
wear out quickly. Keep another backup elsewhere: a stick beside the computer
can be lost in the same fire or theft. Secure Boot may need to be disabled.

## Create

Download the release ISO and its adjacent `.iso.sha256` file from the same
trusted release. Install both into root-owned storage on a fixed desktop disk
before invoking the writer; user-owned or group/other-writable files and media
on FAT, USB, removable disks or loop devices are refused. For example:

```sh
sudo install -D -m 0600 nestegg-rescue-amd64.iso /var/cache/nestegg/releases/rescue.iso
sudo install -D -m 0600 nestegg-rescue-amd64.iso.sha256 /var/cache/nestegg/releases/rescue.iso.sha256
```

A checksum detects damage; it is not a signature and does not
make an untrusted download safe. Do not obtain either file from an unknown stick.

```sh
sudo nestegg stick create /dev/sdX --image /var/cache/nestegg/releases/rescue.iso
```

Supply `--sha256 <SHA256>` with a hash obtained independently, for example from
the release page, to pin the image. This takes precedence over the adjacent
file; it never bypasses ownership or storage checks.

Replace `/dev/sdX` with the **whole external disk**, not a partition. Check the
model, capacity, serial and content displayed by the command. Type the disk name
(for example `sdX`). If it already has filesystems, signatures or partitions,
also type `ERASE sdX`. Creation destroys everything on that disk. Mounted disks,
swap, active encryption, LVM and RAID devices are refused by the rescue disk
guard, including the disk holding the running system.

The integrity-checked ISO is copied into the root-only desktop cache before it is mounted. Creation makes a GPT
with a BIOS boot partition, a FAT32 EFI system partition containing GRUB and two
live slots, and an ext4 `NESTEGG_DATA` partition with no reserved-block percentage.
The root-owned `nestegg/` mount anchor contains the PBS-owned `store/` datastore. Both GRUB and live-boot use
the boot partition's UUID, so booting does not select another attached live disk.
The desktop's own PBS squashfs is installed under `/var/lib/nestegg/stick/`.
Creation does not register the disk or start a backup.

Required host tools: `util-linux` (including mount, unshare, nsenter, losetup and
setpriv), `gdisk`, `dosfstools`, `e2fsprogs`, GRUB utilities, `iproute2`, bash and
udev. GRUB's BIOS/UEFI modules come from the verified ISO. ISO files containing
individual live files of 4 GiB or more are refused because FAT32 cannot hold them.

Writing the ISO directly with `dd` or using Ventoy remains supported as a plain
rescue medium. It does not create this GPT/data layout and cannot be registered
as a backup stick.

## Register and inspect

```sh
sudo nestegg stick add /dev/sdX --image /var/cache/nestegg/releases/rescue.iso
sudo nestegg stick info /dev/sdX
```

`add` records the data partition's UUID in `[stick].data_uuids` in
`/etc/nestegg/config.toml`. It installs the desktop-trusted PBS image **from the
verified ISO**, never from the stick, and preserves existing settings and
comments. Repeating registration does not duplicate the UUID. `info` reports the
boot/data UUIDs, active slot and live filesystem hash, without modifying the stick.
The stick's identifying files are parsed as data, never executed as root.

Cloned data UUIDs on multiple devices are refused rather than resolved through
an arbitrary by-UUID symlink, including for desktop backup and key-copy sessions.
With exactly one registered disk attached, the disk argument can be omitted from
`info`, `update` and `add`. With none or several attached, specify the whole disk.
Unmount the partitions first. After registration, reconnect the medium so udev
can start its backup service. Only registered UUIDs are used. A newly created
stick-only configuration enables full-system backups; an existing configuration
keeps its explicit backup settings. Complete the encryption-key proof before the
first backup.

## Update

```sh
sudo nestegg stick update /dev/sdX --image /var/cache/nestegg/releases/rescue.iso
```

Update validates the format and partition UUIDs, replaces only the inactive live
slot, compares the copied squashfs/kernel/initrd, and flushes the payload. It fsyncs every written file and directory, then
publishes that slot's GRUB configuration checksum **last**. A fixed GRUB
dispatcher checks the two configurations and chooses the highest committed
generation, with a second menu entry for the other committed slot; the previous active slot and configuration remain available while
the replacement is being written. No partitioning, formatting, datastore mount
or backup deletion occurs during update. The boot partition is mounted read-only until the marker, descriptors and
payload checks pass. A free-space check includes the reclaimable inactive slot
before remounting read-write; insufficient space leaves both slots untouched.

If an update is interrupted, reconnect the disk and rerun the command. An
incomplete inactive slot is replaced. Do not unplug while a command is still
running. Flush ordering cannot compensate for broken device firmware, a corrupt
FAT filesystem (both slots share the same FAT metadata) or flash that loses already acknowledged writes; physical
power-loss and USB firmware behavior remain the hardware gate in phase six.
Do not run a filesystem repair while the stick is mounted or in use by PBS.

## Local PBS launcher

`nestegg::stick::launcher` supplies a privileged, reusable `LocalPbs` handle.
Its caller mounts only a registered ext4 partition with `nodev,nosuid,noexec`,
and passes a private runtime directory. The launcher accepts no image path; it
uses only `/var/lib/nestegg/stick/pbs.squashfs`, verifies its recorded SHA-256 and
rejects FAT/removable backing storage. Kernel
IPv6 sockets are required even for IPv4 loopback clients. Unsupported hosts get
an explicit error; the launcher does not alter host IPv6 or firewall settings.

The helper uses private mount, PID, network and UTS namespaces, only loopback,
a private character-device `/dev`, a tmpfs overlay, real tmpfs `/run`, and fresh
configuration owned by the image's own `backup` user. Only the partition's
`nestegg/` anchor is opened without following symlinks, cloned with `open_tree`
and attached with `move_mount` at `/mnt/stick/nestegg`. Certificates,
ACLs and secrets are recreated on every start; the fixed owner is
`nestegg@pbs!stick`. The TLS fingerprint comes from the new `proxy.pem`. Token
bytes reach the caller on a private readiness pipe and are kept in a CLOEXEC
memory file descriptor. No token is placed in argv, environment values or logs.
The returned client command joins **only** the network namespace; it can still
read the desktop's source files. Keep the handle alive until all clients exit,
then call `stop` (or drop it) to kill the private PID namespace, wait for its
exit and release mounts. Startup waits for the API authentication keys and
socket before starting the proxy.
These namespaces isolate networking; trusted PBS code still runs with privilege.
The data root and `nestegg/` must remain root-owned and not group/other-writable.
PBS owns only `nestegg/store/`. Older prototype sticks with PBS-owned anchors or
without payload manifests require explicit migration or recreation; this writer
does not silently alter existing datastore ownership.

## Back up from the desktop

For a stick-only setup, register the medium, create the machine key, then make
and prove its protected copy (replace the documentation UUID with `stick info`):

```sh
sudo nestegg key create
sudo nestegg stick key-copy --uuid 01234567-89ab-cdef-0123-456789abcdef
sudo systemctl enable --now nestegg.service nestegg-backup.timer nestegg-notify.timer
sudo nestegg backup --target stick
sudo nestegg status
```

The password is entered without echo. Use a strong password you can recall:
at least 16 characters from three character classes, or five distinct words
with at least 24 characters in total. Repeated characters alone do not qualify.
An automated caller may supply `--password-fd` instead; passwords never go in
arguments, the configuration, the journal or notification messages. The copy is
scrypt-protected by the official client, read back from the medium, reopened and
compared with the machine key. No unprotected key is written to the stick.

For status and the UI without sudo, follow the group-membership step in
[configuration and setup](configuration.md), then log in again.

Keep a paper key separately. The copy can satisfy the export proof only when
there is no PBS configured. A setup with both targets still needs the existing
independent paper/export proof. The stick prompts for a password re-check after
180 days; repeat it with:

```sh
sudo nestegg stick key-check --uuid 01234567-89ab-cdef-0123-456789abcdef
```

`backup --target pbs`, `--target stick` and `--target all` select the destination;
the default is `all`. Both targets use the same machine key and sources. Full
system mode includes `system.pxar` and `layout.json`. An incomplete backup stays
incomplete on the stick as well. A group whose latest snapshot belongs to a
different key is refused before upload or pruning; use a distinct backup ID for
another machine with the same name. Its existing recovery key is not overwritten.

The package's udev rule starts the device-bound service when a registered stick
is plugged in and a backup is due. Unregistered drives are not mounted by
nestegg. All partitions of a registered medium are hidden from the desktop
automounter. The data mount is private and `nodev,nosuid,noexec,relatime`.
The start notification says to keep the stick plugged in. **Wait for safe to
unplug**: that message follows verification, retention, server shutdown, flushing
and unmounting. A failed run never sends the success message.

Before writing, the filesystem is checked offline without repairing it. After
unclean removal, a filesystem error is reported separately from failed PBS
verification. Unmount the medium and repair ext4 with `e2fsck` before reuse;
never repair a mounted filesystem or an active datastore. PBS verification checks
backup data and cannot repair ext4. Unplugging during a run can lose that run.

Each new snapshot receives decrypted read-back and PBS verification. Once a
month each physical medium receives a forced full datastore verification.
Retention defaults to last 3, daily 7, weekly 4 and monthly 6 snapshots of this
machine. GC keeps the atime safety check and a one-minute cutoff, so space from
newly orphaned chunks is not reclaimed immediately. A full stick reports
`stick_full`; nestegg does not silently delete more backups. See
[configuration](configuration.md) for thresholds and retention overrides.

## Reproduce acceptance tests

```sh
cargo build --release
cargo test --test stick_launcher --no-run
packaging/rescue/build.sh target/release/nestegg target/nestegg-rescue-amd64.iso
sudo tests/stick/create.sh target/release/nestegg target/nestegg-rescue-amd64.iso
# Disposable trusted PBS fixture, without installing host packages:
mkdir /tmp/stick-fixture
tests/stick/spike/build-root.sh /tmp/stick-fixture
sudo tests/stick/desktop.sh target/release/nestegg /tmp/stick-fixture/pbs.squashfs \
  /tmp/stick-fixture/client/usr/bin/proxmox-backup-client
sudo tests/stick/desktop-vm.sh target/release/nestegg target/nestegg-rescue-amd64.iso \
  /tmp/stick-fixture/pbs.squashfs /tmp/stick-fixture/client/usr/bin/proxmox-backup-client
```

The test allocates a sparse image and its own loop devices, creates and registers
it through the real CLI, performs an encrypted PBS backup/restore and restart,
boots it with SeaBIOS and OVMF alongside a competing rescue ISO, updates it,
interrupts another update with SIGTERM after half
the squashfs copy, boots again, and retries the update. It compares the GPT and
retained snapshot files, checks the live service masks and cleans up its own
processes, mounts and loops. It needs KVM, QEMU, OVMF, Python 3 and squashfs-tools.
It also tests descriptor/payload corruption, cloned UUIDs, full ESP, real CLI
mount/holder refusal, early-copy interruption and fsync ordering. It uses no real USB device and proves no physical endurance or power-loss claim.


For block-layer durability replay, build the external test tool from its pinned
upstream revision (it is not a runtime dependency):

```sh
tool_dir=$(mktemp -d)
git clone https://github.com/josefbacik/log-writes.git "$tool_dir"
git -C "$tool_dir" checkout --detach 7b70d8a6863c5de30933d42a7672d35d01d2dc6c
make -C "$tool_dir"
sudo modprobe dm-log-writes
sudo tests/stick/durability.sh target/release/nestegg target/nestegg-rescue-amd64.iso "$tool_dir/replay-log"
```

The replay test owns and removes its sparse files, device-mapper mappings,
loops and mounts. The caller manages the module and external tool. It verifies
actual flushes (including a syncfs negative control) and replays flush/FUA/fsync
boundaries with a deliberately damaged inactive payload. It does not simulate
arbitrary sector tears or prove physical flash-controller reliability.


Additional CLI regressions:

```sh
sudo tests/stick/identity-change.sh target/release/nestegg target/nestegg-rescue-amd64.iso
sudo tests/stick/btrfs-input.sh target/release/nestegg /path/to/mkfs.btrfs
```

The first changes an owned loop's capacity after typed confirmation, verifies
refusal before partition writes, and checks cleanup. The second requires
`btrfs-progs` only for test setup and rejects a root-owned ISO on a Btrfs loop,
whose anonymous mount number must resolve to its real backing device. All Btrfs
members must have known local ancestry; degraded filesystems and unknown
anonymous/network storage are refused. Known virtual-machine disks are trusted
within the guest's storage boundary, not as proof of the hypervisor's hardware.

The desktop test allocates its own image/loop, checks a stick-only encrypted
backup, key proof, read-back, PBS verification, actual prune/GC reclamation after
the atime cutoff, cloned-UUID refusal, full-stick reporting, key mismatch and
injected device-mapper
EIO during a run followed by offline repair. It cleans up its loops, namespaces,
mounts and helpers. It uses synthetic source data, never the host's real files.
The fixtures supply the fixed image cache and its checksum on private RAM
storage; the VM live overlay is deliberately not trusted as code provenance.
The EIO mapping exposes only one filesystem device, so it does not itself
create an ambiguous UUID alias.
The VM driver runs `hotplug-vm.sh` inside the disposable guest and checks real
udev reconnection, device-bound systemd activation,
automounter flags on both partitions, manual backup through the stick API and
honest incomplete/layout handling on the live overlay. The phase-one test
additionally checks BIOS/UEFI boot and interrupted updates.
Neither proves physical flash endurance or real USB power-loss behavior.
