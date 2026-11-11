# Prepare a backup stick

Phase one of [decision 0023](decisions/0023-backup-stick.md) prepares and updates
bootable media. Desktop backup scheduling and restoration **from this stick's
local datastore are not available yet**. Booting it still gives the existing
[rescue system](rescue.md), including restoration from a network PBS.

Use an external USB SSD of at least 128 GB. Cheap flash sticks can be slow and
wear out quickly. Keep another backup elsewhere: a stick beside the computer
can be lost in the same fire or theft. Secure Boot may need to be disabled.

## Create

Download the release ISO and its adjacent `.iso.sha256` file from the same
trusted release. A checksum detects damage; it is not a signature and does not
make an untrusted download safe. Do not obtain either file from an unknown stick.

```sh
sudo nestegg stick create /dev/sdX --image nestegg-rescue-amd64.iso
```

Replace `/dev/sdX` with the **whole external disk**, not a partition. Check the
model, capacity, serial and content displayed by the command. Type the disk name
(for example `sdX`). If it already has filesystems, signatures or partitions,
also type `ERASE sdX`. Creation destroys everything on that disk. Mounted disks,
swap, active encryption, LVM and RAID devices are refused by the rescue disk
guard, including the disk holding the running system.

The verified ISO is copied privately before it is mounted. Creation makes a GPT
with a BIOS boot partition, a FAT32 EFI system partition containing GRUB and two
live slots, and an ext4 `NESTEGG_DATA` partition with no reserved-block percentage.
The standard PBS datastore lives under `nestegg/`. Both GRUB and live-boot use
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
sudo nestegg stick add /dev/sdX --image nestegg-rescue-amd64.iso
sudo nestegg stick info /dev/sdX
```

`add` records the data partition's UUID in `[stick].data_uuids` in
`/etc/nestegg/config.toml`. It installs the desktop-trusted PBS image **from the
verified ISO**, never from the stick, and preserves existing settings and
comments. Repeating registration does not duplicate the UUID. `info` reports the
boot/data UUIDs, active slot and live filesystem hash, without modifying the stick.
The stick's identifying files are parsed as data, never executed as root.

With exactly one registered disk attached, the disk argument can be omitted from
`info`, `update` and `add`. With none or several attached, specify the whole disk.
Unmount the partitions first. Registration alone does not enable backups in
phase one. A registration-only configuration is accepted by the stick commands;
the existing backup commands still require `[pbs]` until phase two adds targets.

## Update

```sh
sudo nestegg stick update /dev/sdX --image nestegg-rescue-amd64.iso
```

Update validates the format and partition UUIDs, replaces only the inactive live
slot, compares the copied squashfs/kernel/initrd, and flushes the payload. It
publishes that slot's GRUB configuration checksum **last**. A fixed GRUB
dispatcher checks the two configurations and chooses the highest committed
generation; the previous active slot and configuration remain available while
the replacement is being written. No partitioning, formatting, datastore mount
or backup deletion occurs during update. The boot partition must have room for
the replacement; if it fills, the previous committed slot is retained.

If an update is interrupted, reconnect the disk and rerun the command. An
incomplete inactive slot is replaced. Do not unplug while a command is still
running. Flush ordering cannot compensate for broken device firmware, a corrupt
FAT filesystem or flash that loses already acknowledged writes; physical
power-loss and USB firmware behavior remain the hardware gate in phase six.
Do not run a filesystem repair while the stick is mounted or in use by PBS.

## Local PBS API for later phases

`nestegg::stick::launcher` supplies a privileged, reusable `LocalPbs` handle.
Its caller mounts only a registered ext4 partition with `nodev,nosuid,noexec`,
and passes the desktop-owned squashfs and a private runtime directory. Kernel
IPv6 sockets are required even for IPv4 loopback clients. Unsupported hosts get
an explicit error; the launcher does not alter host IPv6 or firewall settings.

The helper uses private mount, PID, network and UTS namespaces, only loopback,
a private character-device `/dev`, a tmpfs overlay, real tmpfs `/run`, and fresh
configuration owned by the image's own `backup` user. Only the partition's
`nestegg/` subdirectory is bind-mounted at `/mnt/stick/nestegg`. Certificates,
ACLs and secrets are recreated on every start; the fixed owner is
`nestegg@pbs!stick`. The TLS fingerprint comes from the new `proxy.pem`. Token
bytes reach the caller on a private readiness pipe and are kept in a CLOEXEC
memory file descriptor. No token is placed in argv, environment values or logs.
The returned client command joins **only** the network namespace; it can still
read the desktop's source files. Keep the handle alive until all clients exit,
then call `stop` (or drop it) to kill the private PID namespace and release mounts.
These namespaces isolate networking; trusted PBS code still runs with privilege.
The caller must supply the desktop-owned image, never code from removable media.

## Reproduce acceptance tests

```sh
cargo build --release
cargo test --test stick_launcher --no-run
packaging/rescue/build.sh target/release/nestegg target/nestegg-rescue-amd64.iso
sudo tests/stick/create.sh target/release/nestegg target/nestegg-rescue-amd64.iso
```

The test allocates a sparse image and its own loop devices, creates and registers
it through the real CLI, performs an encrypted PBS backup/restore and restart,
boots it with SeaBIOS and OVMF alongside a competing rescue ISO, updates it,
interrupts another update with SIGTERM after half
the squashfs copy, boots again, and retries the update. It compares the GPT and
retained snapshot files, checks the live service masks and cleans up its own
processes, mounts and loops. It needs KVM, QEMU, OVMF, Python 3 and squashfs-tools.
It uses no real USB device and proves no physical endurance or power-loss claim.
