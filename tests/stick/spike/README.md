# Backup-stick phase-0 spike

These are destructive **disposable-image experiments**, not stick provisioning
or production code. They never accept a physical target disk. Run from the
repository checkout. Requirements: Docker, passwordless sudo, Python 3,
util-linux (unshare/nsenter/losetup), device mapper, ext4 tools, squashfs-tools,
GPT/FAT tools, GRUB installer, QEMU/KVM and OVMF. No host apt repositories,
security policies, existing containers or existing devices are changed.

```bash
work=$(mktemp -d)
mkdir "$work/build"
tests/stick/spike/build-root.sh "$work/build"
# Optional logs contain private runtime information; NEVER commit them.
mkdir "$work/logs"
sudo -n env KEEP_LOGS="$work/logs" tests/stick/spike/run.sh "$work/build"
sudo -n env KEEP_LOGS="$work/logs" tests/stick/spike/boot.sh \
  /path/to/nestegg-rescue-amd64.iso "$work/build/pbs.squashfs"
# The builder's extracted static client is copied into the boot experiment too.
# Remove only this invocation's files after inspecting its private results.
sudo -n rm -rf -- "$work"
```

Obtain the rescue ISO with `packaging/rescue/build.sh` when one is not already
available. The spike does not modify the maintained rescue image builder or
its live configuration. The existing rescue system and PBS payload remain
separate in this experiment; integrating PBS into the release is phase 1.

## Scripts and results

- `build-root.sh EMPTY_DIRECTORY`: uses the maintained E2E PBS Dockerfile,
  including its repository/key pin. Adds test tools in a separate image,
  disables the package-created enterprise source there, extracts the official
  static client, clears the PBS configuration and exports a squashfs. Masks
  PBS, SSH and mail units. Docker has network access only during builds.
- `run.sh BUILD_DIRECTORY`: root mount namespace; a fresh 12 GiB ext4 image,
  a loop and a linear device-mapper target. Starts PBS from squashfs using
  `namespace.sh` and `server.sh`; performs encrypted backup/restore, a second
  start, pruning, GC, verification and a 3 GiB random-data baseline. Substitutes
  an error target during a rate-limited upload, checks failure, restores the
  mapping, runs offline `e2fsck`, then verifies and decrypts an old snapshot.
  Tests interface-level IPv6 disablement and the single-UID mapping limit.
  Finally calls `reuse.sh`. A `FAIL` for the single-ID namespace ownership
  probe is an expected negative result, not successful rootless PBS support.
- `namespace.sh` / `server.sh`: internal helpers, not standalone host commands.
  A tmpfs overlay makes the image writable, but `/run` and the fresh PBS config
  have separate tmpfs mounts. `/dev` contains private character devices and a
  private devpts; no host disks. A new UTS, PID and loopback-only network
  namespace surround the chroot. Fixed owner identity, fresh secret and cert.
  The client joins **only the network namespace**, reading host-side inputs.
- `reuse.sh PRIVATE_EXT4_IMAGE`: copies the filesystem into a new GPT
  partition, gives only that owned partition to a new E2E PBS container,
  creates a private UUID symlink, adds `nestegg` with `--reuse-datastore true`,
  verifies the imported snapshots and unmounts. It never publishes a port.
  Requires the existing `nestegg-e2e-pbs` image; build it with the E2E fixture
  first if absent. No existing container is used or removed.
- `boot.sh RESCUE_ISO [PBS_SQUASHFS]`: creates a 6 GiB GPT image: BIOS boot
  partition, FAT ESP with GRUB and `/live`, ext4 data partition. Pins the
  medium by the FAT UUID. Boots SeaBIOS and OVMF with no network device.
  With PBS supplied, `vm-probe.sh` also starts it and performs encrypted
  backup/restore on the Debian VM host, tries an actual single-map userns
  launch, and repeats the BIOS boot with the **kernel** `ipv6.disable=1`.
  This last proxy failure is asserted as the expected negative result.
- `userns.sh`: optional host/container policy probe. Expected to return
  nonzero when the host's userns policy prevents mapping. The actual PBS
  identity failure is separately exercised inside the Debian boot VM.

Keep extra squashfs files below `/live/spike/`, **not directly in `/live/`**:
Debian live-boot stacks squashfs files found there, which would accidentally
replace the rescue root with the PBS payload.

All runtime mount trees are private before mounting anything. This prevents
parallel VMs/containers from inheriting a mount and pinning a supposedly
removed loop. Cleanup traps kill only the owned namespace/VM, unmount its
filesystems, remove its mapper, detach its loops and remove its temp directory. Clients use
registered private process groups; cleanup errors are reported and keep the
owned directory available for inspection instead of deleting a live mount.
An interruption by SIGKILL cannot run a trap: inspect the particular invocation's
processes/mounts/device ownership before manually cleaning it; never run a
blanket loop detach or Docker prune. Do not edit a running Bash script in place.

The scripts deliberately report raw private logs only to an explicitly chosen
`KEEP_LOGS` directory. The research report contains redacted observations and
public example commands. Real USB speed, flash firmware/power-loss behavior,
physical BIOS compatibility, Secure Boot and a product update transaction are
outside this spike's verified scope.
