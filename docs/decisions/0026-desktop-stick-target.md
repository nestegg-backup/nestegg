# 0026 – Desktop backup targets and removable-media lifecycle

- Status: implemented; acceptance results are recorded below
- Date: 2026-11-11
- Implements: desktop phase two of [0023](0023-backup-stick.md)
- Extends: 0013, 0015, 0017, 0018, 0021, 0022 and the hardened phase one (0024/0025)

## Decision

1. `[pbs]` is optional. At least one of `[pbs]` and `[stick]` must exist.
   Both use the same configured sources, full-system layout capture, encryption
   credential, incomplete classification and decrypted read-back pipeline.
   Stick encryption cannot be disabled. Unknown configuration keys remain errors.
2. Keep the existing PBS fields in `state.json` and add a separate `stick`
   projection. A run without `target` is an old PBS run. Success, failure counts,
   read-back availability counts and retry deadlines are independent. The shared
   local history limit is applied independently to each target, and snapshot
   metadata fallback records must belong to that target. The shared
   run lock spans all selected targets. `all` tries the stick first; a plug-in
   event waits behind an existing run and rechecks whether its backup is due.
   Stick absence is `stick_not_plugged_in`, exit 75, and does not increment or
   reset failure counts. The stick overdue default is 168 hours.
3. The package ships a udev predicate and `nestegg-stick-pbs@.service`.
   Registration is checked from the strict desktop configuration, not a marker
   supplied by the device. Only a matching ext4 UUID starts the service. Escape
   the UUID in the instance name so `BindsTo=` names the actual by-UUID device
   unit. Hide partitions of registered media from UDisks, including the ESP.
   No rule or service is installed on the development host by acceptance tests.
4. In the run process, create a private mount namespace, cross-check the block
   device's UUID and ext4 type, refuse a pre-existing mount, take a per-medium
   lock and run offline `e2fsck -f -n`. This read-only check also covers unclean
   removal when the desktop has no removal record. Do not replay a journal or
   repair implicitly: report `filesystem_damaged` and require offline repair.
   A zero fsck exit code is insufficient: e2fsck 1.47 can return zero for a wrong
   free-block summary with `-n`. Reject declined repair prompts and skipped
   journal recovery as well; this was reproduced on a disposable regular image.
   Reject cloned UUIDs by probing the complete device tree, even if the
   by-UUID symlink points to one valid clone. Mount `nodev,nosuid,noexec,relatime`;
   launch only the verified fixed desktop cache at
   `/var/lib/nestegg/stick/pbs.squashfs` (0025), never a caller-selected image.
   Pin the root-owned `nestegg/` anchor by opened directory descriptor and
   `open_tree`/`move_mount`; PBS owns only `nestegg/store/` beneath it.
   Refuse old prototype ownership/layout rather than silently migrating it.
   Wait for API keys and its socket before proxy startup, and for namespace
   PID 1 to exit during shutdown.
   Stop PBS, flush the filesystem and unmount before saying safe to unplug.
5. Before upload and again before pruning, open the group's latest signed
   manifest using the current machine key through the official client. A
   different key, an unsigned manifest or an unreadable manifest stops the run
   without writing or pruning snapshots. Do not trust a mutable fingerprint in
   snapshot notes. A recovery key copy belonging to another key is also never
   overwritten just because two machines share a backup ID.
   Follow PBS's definition of a completed snapshot (its manifest exists): an
   interrupted newer upload does not hide the last completed snapshot's key.
6. Read available bytes from PBS's `status` (the backup user's availability,
   including reserved filesystem space), default minimum 256 MiB. Insufficient
   space is `stick_full`. Never expand pruning in response to space pressure.
   After read-back, invoke the official PBS manager to verify unverified
   snapshots, including the new snapshot. Every 30 days per physical UUID, force
   a complete datastore verification. Wait for the manager command to finish and
   require the new snapshot's PBS verification state to be `ok` before success.
   A failed PBS check is `verification_failed`, distinct from ext4 damage.
   A failed or cancelled stick run invalidates its full-verification timestamp;
   after repair the next run rechecks all backup data before pruning.
7. After verification prune only this machine's group: keep last 3, daily 7,
   weekly 4, monthly 6. A zero daily/weekly/monthly count omits that criterion;
   keep-last must be positive. Run synchronous GC with the launcher's
   `gc-atime-cutoff=1`. Keep PBS's atime safety check. Newly orphaned chunks are
   still delayed; acceptance measures reclaimed unique-chunk bytes after the
   cutoff rather than treating GC exit status as reclamation evidence.
8. `stick key-copy --uuid` uses a private controlling terminal to run the
   official client's `key change-passphrase --kdf scrypt`. No cryptography or
   key-file format is reimplemented. Require either 16 characters from at least
   three character classes or five distinct words totalling 24 characters; both
   forms require at least eight distinct characters. This is a minimum input
   rule, not an entropy guarantee. Passwords are read without echo or via a file
   descriptor. The helper never forwards terminal output. Only the encrypted
   copy goes to `nestegg-keys/<backup-id>.json` on the data partition.
9. Read the actual protected copy back, refuse symlinks/nonregular files,
   require scrypt, open it with the password using the official client, and
   compare decrypted key material. Record proof by UUID and current key digest.
   In stick-only setups it can satisfy 0018. With PBS configured the independent
   existing export proof stays required. Reopening with `stick key-check`
   refreshes a 180-day password reminder; the reminder never silently disables
   backups. Keep a paper key elsewhere. Changing a forgotten password without
   first opening the existing copy is deliberately not an overwrite shortcut.
10. Keep legacy API fields and add `status.targets`, `Run.target`, `stick_uuid`,
    and `/targets/{pbs|stick|all}/runs` for history and manual starts. The UI can
    request a stick backup without handling credentials. Extend settings with
    `stick_overdue_after_hours` and `remove_target`; removing the last target is
    refused. Removing a target or raising the stick threshold uses 0022's existing
    announcement mechanism. Direct root file edits retain 0022's trusted-root
    semantics. `api/openapi.yaml` remains the API authority.
11. Use distinct notification events for stick failures, overdue, incomplete,
    damaged, full and password reminders, so a simultaneous PBS problem cannot
    replace the stick problem in notification bookkeeping. Start and safe-to-
    unplug events use the existing bounded desktop/hook channels. No new network
    notification client or shell hook interpretation is introduced.

## Acceptance boundaries

All media tests use sparse image files and owned loop devices, never a real USB
stick. Physical power-loss behavior, flash endurance, firmware boot quirks and
restore onto physical hardware remain phase-six gates. The UI presentation is
phase five; this phase supplies its backward-compatible backend contract.

## Original phase-two acceptance evidence (2026-11-11)

Before integration with 0025, the Rust suite passed 505 tests (privileged
launcher tests were opt-in); clippy with warnings denied, formatting, the OpenAPI YAML parse and the
tracked-tree privacy scan pass. The unchanged UI builds and passes 308 tests.
The existing disposable PBS E2E passes 193 checks and the package/systemd E2E
passes 155 checks. The package-upgrade E2E passes 98 checks, including that old
history becomes PBS history and that an absent newly registered stick cannot
replace the old PBS success or its key proof.

The loop-device acceptance passes encrypted backup and read-back, per-snapshot
and monthly PBS verification, measured unique-chunk reclamation after prune/GC,
full-media notification, rejection of another key, absence without a failure,
mid-upload device I/O failure, independent filesystem damage detection, offline
repair and a successful subsequent backup. It also confirms that interrupted
uploads cannot permanently block the completed-snapshot key check.

The KVM test passes real loop detach/reattach through the guest's udev daemon,
automatic device-bound service startup, both partitions' UDisks flags, a manual
stick backup through the daemon API and incomplete/system-layout handling on a
live overlay. All test media and credentials are generated and disposable. No
real USB medium or host service/rule installation is used.


## Merge resolution and verification (2026-11-11)

Merge the desktop phase-two branch into the hardened phase-one branch with both
histories retained (parents `9a43a90` and `c555d94`). Keep SHA-256 pinning and the
fixed launcher cache, the descriptor-pinned root-owned anchor and the
`nestegg/store/` datastore. Registration keeps the phase-two dispatch and
stick-only full-system default, while configuration writes use only the shared
locked settings implementation, preserving existing owner and mode and creating
new configurations as root:nestegg 0640. Keep both API-startup and namespace-exit
waits. Protected key copies use the existing atomic 0600 file writer rather than
restoring the removed duplicate byte writer.

The desktop and VM fixtures install the fixed cache with its checksum on
explicit private RAM storage. A live overlay fails the provenance check by
design; product checks were not relaxed to accommodate it. Desktop sessions share
the complete-device UUID-clone check with media commands, with a real cloned-loop
regression. The EIO fixture exposes its ext4 filesystem through only its mapper,
not simultaneously through a raw partition alias. VM negative checks fail the
script explicitly, and failed guest runs print diagnostic logs even on `exit 1`.

The merged Rust suite passes 513 tests; its two normally ignored privileged
launcher tests are exercised by media acceptance. An additional root run passes
all 21 stick unit tests, including ownership and provenance branches. Clippy
with warnings denied, rustfmt, ShellCheck, OpenAPI parsing and the tracked-tree
privacy scan pass. UI dependency installation and build pass, with 308 UI tests.
PBS E2E passes 193 checks; package/systemd E2E passes 155 checks; upgrade E2E
passes 98 checks using the historical 0.1.0-1 baseline and merged 0.1.0-2 package.
An initial upgrade run used an unsuitable newer baseline that already contained
snapshot metadata; that fixture mismatch was resolved by using the established
historical baseline, without weakening the upgrade assertion.

`tests/stick/create.sh` passes all twelve KVM boots (SeaBIOS and OVMF for
created, updated, missing-descriptor, truncated-payload, interrupted-update and
retried media), all adversarial CLI guards, snapshot/layout preservation and
nonempty recreation. A follow-up recreation check seeds the merged
`nestegg/store/.chunks` layout and confirms that recreation removes the store
while keeping an empty mount anchor; the acceptance assertion now checks that
path explicitly. `tests/stick/desktop.sh` passes encrypted key proof,
backup/read-back, PBS verification, measured prune/GC reclamation, full-stick and
key-mismatch handling, absence without a failure, cloned UUID refusal, EIO during
upload, filesystem damage detection, offline repair and successful recovery.
`tests/stick/hotplug-vm.sh`, run through `desktop-vm.sh`, passes guest udev/device
activation, automounter flags, daemon API backup and incomplete layout handling.

The media tests use the existing phase-one rescue ISO as an OS/PBS fixture and
the merged CLI. The hotplug driver replaces the guest CLI with the merged binary.
No physical USB stick is available; physical power-loss and endurance gates
remain outside this acceptance.

All owned loop devices, private mounts, helpers, test containers, fixtures, logs
and newly generated build/UI output were removed after verification. Shared
pre-existing release fixtures and Docker build caches were left intact.
