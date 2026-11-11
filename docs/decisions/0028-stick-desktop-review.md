# 0028 – Desktop stick execution and retention hardening

- Status: accepted
- Date: 2026-11-11
- Extends: 0024–0026

## Process and mount safety

The key passphrase helper uses `/usr/bin/python3 -I`, starts in `/`, and
removes inherited `PYTHON*` variables. A root command must not import Python
modules from the caller's working directory or environment.

The desktop run's mount namespace is recursively slave: host automounts and
later source mounts propagate into it, while its stick mounts never propagate
back. The data mount explicitly uses `errors=remount-ro`, overriding an
untrusted ext4 superblock's error policy.

The read-only `e2fsck -f -n` runs through util-linux `setpriv` with no Linux
capabilities and no privilege gains on exec. It still runs as the device's
owner (root) to open root-owned block devices. This is privilege reduction,
not a complete filesystem-parser sandbox. A separate systemd transient unit
would escape the calling test/desktop mount namespace and is not available
in all supported invocation environments; adding another sandbox dependency
is deferred. Filesystem repair remains explicit and offline.

Opening a stick distinguishes mounted media, ambiguous cloned UUIDs, a busy
device lock, missing media, filesystem damage, and internal helper failures.
Only evidence of filesystem corruption is labelled `filesystem_damaged`.

## Validation

Unit tests exercise isolated imports and error classification. A privileged
temporary-tmpfs test proves incoming mount propagation and the absence of
outgoing propagation. Desktop and device-bound VM acceptance cover the real
mount, filesystem check and backup pipeline.
