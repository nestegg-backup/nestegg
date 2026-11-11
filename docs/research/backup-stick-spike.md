# Backup-stick phase-0 spike

Date: 2026-11-11. Decision: [0023](../decisions/0023-backup-stick.md).
Reproduction: [spike scripts](../../tests/stick/spike/README.md).

**GO for decision points 1–3 with privileged startup and kernel IPv6 support.**
The unchanged proposal needs these explicit prerequisites: a kernel booted with
`ipv6.disable=1` cannot run this PBS proxy, and a single mapped root UID cannot
run both PBS identities. This is feasibility evidence, not acceptance of 0023
or a hardware release sign-off. There was no USB stick or external SSD attached.

## Scope and evidence

All devices were newly allocated loops backed by private image files. All
containers, namespaces, certificates, tokens and encryption keys were disposable.
No host apt repository, security setting, existing container or physical disk
was changed. Raw logs were inspected privately and are not repository artifacts;
they contain generated credentials/identities and host information. Only generic
commands and redacted observations are recorded here.

Observed environments:

- Ubuntu 24.04 userspace, kernel `7.0.0-42-generic`, normal host boot.
- Debian 13 rescue system under KVM, actual Debian kernel
  `6.12.116+deb13-amd64`, SeaBIOS and OVMF, with no virtual network adapter.
- Debian trixie PBS root: `proxmox-backup-server 4.2.9-1`, packaged client
  `4.2.8-1`, official extracted static client `4.2.8-1`.
- Fresh build from `tests/e2e/pbs.Dockerfile`, `pbs-no-subscription`, identical
  pinned archive key. The squashfs occupied about 173 MiB; this minimal server
  root is not the size of an integrated rescue image.

The VM is a second kernel/userspace check; a Debian container sharing the Ubuntu
kernel was not substituted for a Debian-host result. VM backup/restore checks
were repeated in both boot modes. Full pruning/GC/EIO/speed/import tests ran on
the Ubuntu host. No real desktop filesystem or user data was backed up. An independent read-only
review checked the scripts and private logs; the resulting EIO assertions,
container device restrictions and interruption cleanup fixes were retested.
A targeted SIGTERM during an active run exited 143 and completed cleanup;
normal completion also checks for remaining owned loops/mappers/mounts/processes.

## 1. Squashfs, namespaces, temporary configuration and ext4

**Verified on Ubuntu and the Debian VM.** The builder uses Docker only to install
the official packages and export the root, then builds a read-only squashfs.
The package installs an enterprise apt source: remove that source inside the
throwaway build image before further apt operations. Nothing is added to host
apt. The maintained E2E Dockerfile and its pinned key remain unchanged.

Representative commands (the harness supplies owned paths and cleanup):

```bash
mkfs.ext4 -m 0 -L NESTEGG_DATA "$owned_loop"
mount -o nodev,nosuid,noexec "$owned_loop" "$work/data"
unshare --mount --pid --net --uts --fork --kill-child namespace.sh ...
mount -t overlay overlay -o "lowerdir=$lower,upperdir=$upper,workdir=$overlay_work" "$root"
mount -t tmpfs tmpfs "$root/run"
mount -t tmpfs tmpfs "$root/etc/proxmox-backup"
mount --bind "$work/data/nestegg" "$root/mnt/stick/nestegg"
```

The upper/work directories themselves reside in tmpfs. A distinct `/run` tmpfs
is mandatory: PBS rejects `/run/proxmox-backup/shmem` on overlayfs, even when the
overlay's upper layer is in RAM. Configuration, ACLs, token secrets, certificate,
logs and job state are regenerated or ephemeral. Only the datastore is persistent.
Creation uses PBS's expected directory permissions (normal `umask 022`); setting
`umask 077` globally initially made reuse fail the `.chunks` permission check.
Keep secrets explicitly mode 0600 instead. The checked `.chunks` ownership/mode
was UID/GID 34 and mode 0750 as created by the PBS tools. Product code must read the image's `backup` identity,
not assume a desktop's account database matches it.

The namespace has only `lo`, a private proc/sys view and private character-only
`/dev` plus devpts. Observed listeners inside it were the API on loopback port 82
and the proxy wildcard on 8007. On Ubuntu, host `ss` showed no listener on port
8007 while the private proxy was serving requests. There is no veth, physical
interface or published Docker port. Direct daemon startup follows the E2E
fixture and needs no systemd. Units for PBS, SSH and mail are masked in the root.

**0023:** points 1–3 are feasible; explicitly require real tmpfs `/run`, correct
PBS filesystem ownership/permissions, a root helper and fresh config reuse.

## 2. Encrypted client backup, restore, restart, prune, GC and verify

**Verified.** The static client was extracted from its official package to the
throwaway build directory. Its process entered only the PBS network namespace;
it retained the host mount/PID namespaces and read synthetic host-side files.
Neither a host install nor an apt source change was needed.

```bash
nsenter -t "$pbs_pid" -n env PBS_PASSWORD_FD=9 PBS_FINGERPRINT="$fresh_fingerprint" \
  PBS_ENCRYPTION_PASSWORD_FD=8 "$static_client" backup files.pxar:"$source" \
  --backup-id desktop --keyfd 7 7<"$key" 8<"$pass" 9<"$token"
proxmox-backup-manager datastore create stick /mnt/stick/nestegg \
  --reuse-datastore true --tuning gc-atime-cutoff=1
proxmox-backup-manager garbage-collection start stick
proxmox-backup-manager verify-job create spike --store stick --ignore-verified false
proxmox-backup-manager verify-job run spike
```

The repository identity was fixed at `nestegg@pbs!stick`, with ACLs granted to
both the user and its token. Each launch generated a new token secret and TLS
certificate; their values changed between starts. The fingerprint was read with
`openssl x509 -sha256 -fingerprint` from that launch's `proxy.pem`, not taken
from the data image. The group owner remained `nestegg@pbs!stick`; another backup
to the same group passed after the whole writable PBS root was discarded.
Restore matched the source byte for byte. The Debian VM independently completed
a small encrypted backup/restore in both firmware modes.

Two desktop snapshots were pruned to one. A full server verify succeeded for
both the retained desktop and the later 3 GiB snapshot. GC with cutoff one minute
passed its atime safety check and both mark/sweep phases. In this fixture it
removed zero chunks: the early snapshots shared their chunks, and newly orphaned
chunks are still protected by the cutoff. Immediate space reclamation was not
proved; a small cutoff is not zero retention time. Never disable PBS's atime
safety check or assume arbitrary `noatime` mounts are safe.

**0023:** fresh config and owner assumption confirmed. GC invocation confirmed;
space-pressure scheduling and actual garbage reclamation need phase-2 coverage.

## 3. Normal boot, IPv6 sysctl and kernel disablement

**Verified, with a failing unsupported case.** Normal Ubuntu and Debian boots
started the proxy. In the Ubuntu test network namespace, setting all/default/lo
`net.ipv6.conf.*.disable_ipv6=1` still allowed the proxy to start and answer its
IPv4 loopback readiness request. Those sysctls remove IPv6 addresses; they do
not reproduce removal of the kernel's IPv6 socket family.

A third BIOS VM boot actually passed `ipv6.disable=1`. The kernel reported that
IPv6 was administratively disabled, live-boot still found its pinned medium,
and the PBS proxy failed with `Address family not supported by protocol
(os error 97)`. It did not provide an IPv4 fallback. Namespace sysctls cannot
restore an address family disabled at kernel boot.

**0023:** require a kernel with working AF_INET6 before offering the local PBS.
Detect this explicitly and return an actionable unsupported-host state. Do not
change host sysctls, kernel options or firewall rules automatically. Supporting
`ipv6.disable=1` would require a separately reviewed upstream proxy change or a
different architecture. It is not included in this GO.

## 4. Unprivileged mapped root

**Failed with a single-UID/GID mapping.** The Ubuntu host denied ordinary
unprivileged `unshare --user --map-root-user` at `uid_map`; its AppArmor userns
restriction was enabled. A privileged container did not make that unprivileged
attempt succeed. The host policy was left intact.

The Debian VM did allow the actual unprivileged mapping and attempted both PBS
daemons with fresh tmpfs config/run/log paths. The API rejected the config owner
(`0 != 34`); switching to `backup` for the proxy failed `setresuid` with
`Invalid argument`. The separate ownership probe likewise failed `chown 34:34`
inside a single-ID namespace. PBS needs both root and the `backup` UID/GID.

**0023:** no rootless support claim. A full subordinate-UID map with shifted
image/datastore ownership was not implemented or tested. It would also need to
preserve portability to a real PBS; a single root mapping does not achieve that.
Privileged filesystem mounting/device management is still needed by the proposal.

## 5. Password-protected scrypt key without a user terminal

**Verified.** Python `pty.fork()` supplied a private controlling terminal to
`proxmox-backup-client key create --kdf scrypt`. It answered both password prompts
without printing/logging the password. The resulting key recorded the Scrypt
parameters `n=65536`, `r=8`, `p=1`. The encrypted key and its password were passed
separately via `--keyfd` and `PBS_ENCRYPTION_PASSWORD_FD` for unattended backup
and restore. The caller had no interactive terminal. All key material was deleted.

**0023:** the pseudo-terminal route in point 10 works. Using the Rust key crate
was not necessary and was not evaluated.

## 6. Pull-the-drive simulation

**Verified for injected EIO, with filesystem repair required.** A dedicated
linear device-mapper target sat above the owned ext4 loop. During an active,
rate-limited encrypted backup of new random data, it was switched to an `error`
target using suspend/load/resume. This produced real filesystem I/O failures;
`losetup -d` alone would only request lazy detachment of a mounted loop.

The client failed inserting a chunk with EIO or EROFS after ext4 went read-only; the interrupted job did not become
a completed snapshot. The server was stopped, the filesystem unmounted, the
linear mapping restored and offline `e2fsck -fy` run. It replayed the journal
and corrected free-block/inode counts. Then fresh PBS startup, a forced full
verify of both prior complete snapshots and a decrypted restore succeeded.
The reusable E2E PBS later verified the same retained snapshots as well.

**0023:** detect unclean removal and handle filesystem checking/repair before
normal reuse. PBS verify detects damaged backup data; it is not an ext4 repair
tool. This experiment does not prove recovery without fsck, recovery of the
interrupted backup, every possible interruption point, or real flash-controller
power-loss behavior. The fault check requires a storage error rather than a timeout and asserts that
only the two prior complete snapshots remain; the retained snapshots were verified again.

## 7. Speed baseline

**Verified only on the local disk; USB not possible here.** A new 3 GiB file from
`/dev/urandom` was backed up to a new group with encryption/TLS and PBS's default
sync behavior. Isolated runs took 11–12 seconds (roughly 256–279 MiB/s
logical input); runs alongside other validation took 14–20 seconds (154–219 MiB/s). This includes the client/server path, not random-data generation.
No global cache dropping or other host tuning was performed. OS caches and the
loop's underlying local storage affect the result.

**0023:** this rejects no obvious loopback performance blocker. Full-system
metadata load, USB stick/SSD write speed, endurance, flush reliability and real
capacity behavior remain untested because neither real medium was attached.
Do not use this number as a USB performance promise; keep phase 6 mandatory.

## 8. Real PBS removable datastore reuse

**Verified with the official E2E PBS container.** With the local PBS stopped and
its filesystem unmounted, the partition image was copied into a fresh GPT
partition and exposed only to a new container with mount capability, no network or
published ports. Its `/dev/disk/by-uuid` link was private to that container.
No formatting of the existing filesystem was performed.

```bash
proxmox-backup-manager datastore create removable nestegg \
  --backing-device "$filesystem_uuid" --reuse-datastore true
```

PBS recorded `path: nestegg`, mounted the backing filesystem and bound its
subdirectory under `/mnt/datastore/removable`. A forced server verify of the
existing desktop and 3 GiB snapshots completed with zero errors. PBS then
unmounted it. The first probe used a whole-loop filesystem; the final reusable
probe adds a GPT partition around a copy, preserving the actual filesystem.
Only one PBS accessed the data at a time.

**0023:** fixed relative subdirectory and standard-datastore portability confirmed.
Hot-plug automount on an appliance, multiple stores on one medium and a physical
USB device were not tested.

## 9. GPT, GRUB, live-boot, BIOS and UEFI

**Verified in KVM with SeaBIOS and OVMF.** Reused the existing rescue ISO rather
than rebuilding it. Created a sparse GPT image with a 1 MiB BIOS boot partition,
1536 MiB FAT ESP, and ext4 `NESTEGG_DATA` (`mkfs.ext4 -m 0`). Installed GRUB for
`i386-pc` into the GPT BIOS partition and `x86_64-efi` into the fallback ESP path.
Copied `/live` and searched the ESP by filesystem UUID in GRUB. The kernel used
`live-media=/dev/disk/by-uuid/<ESP-UUID>`.

Both firmware modes reached the serial login, mounted exactly `/dev/vda2` as
`/run/live/medium`, found `live/filesystem.squashfs`, and ran the PBS VM probe
against the ext4 data partition. BIOS also found the medium with kernel IPv6
disabled. No `pmbr_boot` flag was set; SeaBIOS did not need it. Physical BIOS
quirks remain untested, so this does not rule out needing the flag on hardware.

An exploratory payload placed directly in `/live/` exposed live-boot's squashfs
stacking behavior: it booted the PBS root over the rescue root. The corrected
harness keeps the extra PBS payload in `/live/spike/`. Phase 1 should integrate
PBS into the intended live filesystem rather than stack an accidental second root.
Two update slots, interruption-safe GRUB switching, a second competing live stick,
Secure Boot and restoration of a real machine were not tested in phase 0.

## Source cross-check

Live results take precedence over assumptions and unversioned source. Read on
2026-11-11: [official removable datastore documentation](https://pbs.proxmox.com/docs/storage.html#removable-datastores),
[official client environment variables](https://pbs.proxmox.com/docs/backup-client.html#environment-variables),
[current official proxy source](https://git.proxmox.com/?p=proxmox-backup.git;a=blob;f=src/bin/proxmox-backup-proxy.rs;hb=HEAD),
and [current official datastore configuration source](https://git.proxmox.com/?p=proxmox-backup.git;a=blob;f=src/api2/config/datastore.rs;hb=HEAD).
These support the UUID/relative-path reuse, FD inputs and IPv6 wildcard behavior;
the installed binaries and actual Debian kernel boot establish the observed limits.

## Go / no-go and required decision changes

**GO for points 1–3 on supported hosts:** keep trusted PBS code on the desktop,
create configuration in RAM each time, use the fixed token owner and isolate
network access with a privileged namespace launcher. **NO-GO for rootless startup
or hosts booted with `ipv6.disable=1` under the current implementation.**

0023 stays **proposed**. Add the IPv6 preflight/unsupported state, explicit
`/run` tmpfs and PBS ownership requirements, rootless limitation and unclean
filesystem recovery handling. Keep GC's atime checks. Carry real USB speed,
endurance, physical boot compatibility and actual restore to later hardware tests.
The loop/VM spike justifies phases 1–2 with those boundaries; it does not justify
shipping the complete backup-stick feature yet.
