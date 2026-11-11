#!/usr/bin/env bash
# Desktop phase-two acceptance. Only an owned sparse image/loop is ever used.
# Usage: sudo tests/stick/desktop.sh BINARY TRUSTED_PBS_SQUASHFS STATIC_CLIENT
set -Eeuo pipefail
[[ $EUID == 0 ]] || { echo 'Run with sudo -n' >&2; exit 2; }
bin=$(realpath "${1:?binary required}")
image=$(realpath "${2:?trusted PBS squashfs required}")
client_bin=$(realpath "${3:?official static client required}")
if [[ ${NESTEGG_DESKTOP_TEST_NS:-0} != 1 ]]; then
    exec unshare --mount --propagation private env NESTEGG_DESKTOP_TEST_NS=1 "$0" "$@"
fi
work=$(mktemp -d /tmp/nestegg-stick-desktop.XXXXXX)
loop=; clone_loop=; mapper=; backup_pid=
unset SUDO_UID SUDO_GID SUDO_USER CREDENTIALS_DIRECTORY
cleanup() {
    local status=$? failed=0
    trap - EXIT ERR; trap '' INT TERM
    set +e
    if [[ -n $backup_pid ]]; then kill -TERM "$backup_pid" 2>/dev/null || true; wait "$backup_pid" 2>/dev/null || true; fi
    # Our workers mount in child namespaces; no host mount was ever created.
    if [[ -n $mapper ]]; then
        for _ in {1..50}; do
            if dmsetup remove "$mapper" 2>/dev/null; then mapper=; break; fi
            sleep .1
        done
        [[ -z $mapper ]] || failed=1
    fi
    if [[ -n $clone_loop ]]; then losetup -d "$clone_loop" || failed=1; fi
    if [[ -n $loop ]]; then losetup -d "$loop" || failed=1; fi
    for _ in {1..50}; do
        if ! losetup -ln -O BACK-FILE | grep -Fq "$work/"; then break; fi
        sleep .1
    done
    if losetup -ln -O BACK-FILE | grep -Fq "$work/"; then failed=1; fi
    umount /dev/disk/by-uuid || failed=1
    if (( failed )); then echo "ERROR: owned resource cleanup incomplete: $work" >&2; exit 1; fi
    if [[ -n ${KEEP_LOGS:-} ]]; then mkdir -p "$KEEP_LOGS"; cp "$work/"*.log "$KEEP_LOGS/"; fi
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
trap 'echo "ERROR: desktop stick test failed at line $LINENO" >&2' ERR
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$work/"{uuid,credentials,state/stick,runtime,source,data,home}
# Cargo may relink its output while another acceptance job is compiling.
# Freeze this run's executable just as we freeze its trusted PBS image.
cp "$bin" "$work/nestegg"
bin=$work/nestegg
chmod 0700 "$work/credentials" "$work/runtime"
# Bind only inside this harness namespace, preserving all host /dev links.
mount --bind "$work/uuid" /dev/disk/by-uuid
truncate -s 3G "$work/stick.img"
loop=$(losetup --find --show --partscan "$work/stick.img")
printf 'label: gpt\n, , L\n' | sfdisk "$loop" >"$work/partition.log" 2>&1
partprobe "$loop"
part=${loop}p1
for _ in {1..50}; do [[ -b $part ]] && break; sleep .1; done
mkfs.ext4 -q -m 0 "$part"
uuid=$(blkid -p -s UUID -o value "$part")
ln -s "$part" "$work/uuid/$uuid"
mount -o nodev,nosuid,noexec "$part" "$work/data"
mkdir "$work/data/nestegg"
umount "$work/data"
# The launcher always opens its verified fixed desktop cache. Bind it only in
# this private mount namespace; environment overrides cannot select code.
mount -t tmpfs -o mode=0755 tmpfs /var/lib
mkdir /var/lib/nestegg
mount --bind "$work/state" /var/lib/nestegg
cp "$image" "$work/state/stick/pbs.squashfs"
sha256sum "$work/state/stick/pbs.squashfs" | cut -d' ' -f1 >"$work/state/stick/pbs.squashfs.sha256"
export HOME="$work/home"
export NESTEGG_CONFIG="$work/config.toml" NESTEGG_STATE_DIR="$work/state" NESTEGG_RUNTIME_DIR="$work/runtime" NESTEGG_CREDENTIALS_DIR="$work/credentials" NESTEGG_PBS_CLIENT="$client_bin" NESTEGG_SYSTEMD_CREDS="$work/no-creds" NESTEGG_SYSTEMCTL="$work/no-systemctl"
# Both the password and key are newly generated fixture secrets, never logged.
openssl rand -base64 30 >"$work/password"
cat >"$work/hook" <<EOF
#!/bin/sh
printf '%s\n' "\$NESTEGG_EVENT" >> '$work/events'
EOF
chmod 0700 "$work/hook"
cat >"$NESTEGG_CONFIG" <<EOF
backup_id = "desktop"
[stick]
data_uuids = ["$uuid"]
minimum_free_bytes = 1048576
[stick.retention]
keep_last = 1
keep_daily = 0
keep_weekly = 0
keep_monthly = 0
[backup]
sources = [{archive="files", path="$work/source"}]
system = false
inventory = true
[notify]
desktop = false
command = ["$work/hook"]
EOF
"$bin" key create --allow-plain >"$work/key.log" 2>&1
"$bin" stick key-copy --uuid "$uuid" --password-fd 7 7<"$work/password" >"$work/key-copy.log" 2>&1
"$bin" stick key-check --uuid "$uuid" --password-fd 7 7<"$work/password" >"$work/key-check.log" 2>&1
head -c 16777216 /dev/urandom >"$work/source/random"
"$bin" backup --target stick >"$work/backup.log" 2>&1
python3 - "$work/state/state.json" <<'PY'
import json, sys
s=json.load(open(sys.argv[1]))
r=s['stick']['last_success']
assert r['target']=='stick' and r['verified'] and not r['incomplete'], r
assert s.get('last_success') is None, 'stick must not overwrite legacy PBS state'
assert s['stick']['full_verified_at']
assert s['stick_media'][r['stick_uuid']]['password_checked_at']
assert not s.get('key_exported_digest'), 'stick-only proof must not confirm the independent PBS proof'
PY
grep -q '^stick_start$' "$work/events"
grep -q '^stick_safe_to_unplug$' "$work/events"
echo 'PASS stick-only key copy, encrypted backup, read-back, PBS verify and clean unplug notification'
"$bin" run --trigger plug --target stick >"$work/not-due.log" 2>&1
grep -q 'not due yet' "$work/not-due.log"
echo 'PASS recent stick backup is not repeated on plug-in'
# The by-UUID link still points to the original medium. A second attached
# filesystem with the same UUID must nevertheless stop desktop sessions.
cp --reflink=auto --sparse=always "$work/stick.img" "$work/clone.img"
clone_loop=$(losetup --find --show --partscan "$work/clone.img")
partprobe "$clone_loop"
for _ in {1..50}; do [[ -b ${clone_loop}p1 ]] && break; sleep .1; done
if "$bin" stick key-check --uuid "$uuid" --password-fd 7 7<"$work/password" >"$work/cloned-uuid.log" 2>&1; then exit 1; fi
grep -q 'cloned data UUID' "$work/cloned-uuid.log"
losetup -d "$clone_loop"; clone_loop=
rm "$work/clone.img"
echo 'PASS desktop sessions refuse cloned UUIDs despite an unambiguous by-UUID symlink'

# Save the first usage, then make the first snapshot's unique chunks orphaned.
mount -o nodev,nosuid,noexec "$part" "$work/data"
before=$(du -sb "$work/data/nestegg/store/.chunks" | cut -f1)
umount "$work/data"
head -c 16777216 /dev/urandom >"$work/source/random"
"$bin" backup --target stick >"$work/second.log" 2>&1
# PBS keeps newly orphaned chunks behind its atime cutoff. Wait rather than
# disabling the safety test or falsifying chunk timestamps.
sleep 70
"$bin" backup --target stick >"$work/reclaim.log" 2>&1
mount -o nodev,nosuid,noexec "$part" "$work/data"
after=$(du -sb "$work/data/nestegg/store/.chunks" | cut -f1)
count=$(find "$work/data/nestegg/store/host/desktop" -mindepth 1 -maxdepth 1 -type d | wc -l)
umount "$work/data"
[[ $count == 1 ]]
# New and old random files have comparable size: retained usage must be less
# than the two complete unique generations. The manager log also states removal.
(( after < before * 3 / 2 ))
echo 'PASS prune policy and actual unique-chunk space reclamation after the safety cutoff'
# A corrupt snapshot of another machine must not prevent this machine's new
# snapshot, retention, GC, or success. Use the real client for both groups.
mount -o nodev,nosuid,noexec "$part" "$work/data"
find "$work/data/nestegg/store/.chunks" -type f -size +1M -printf '%P\n' | sort >"$work/chunks-before"
umount "$work/data"
sed -i 's/backup_id = "desktop"/backup_id = "other"/' "$NESTEGG_CONFIG"
head -c 16777216 /dev/urandom >"$work/source/random"
"$bin" backup --target stick >"$work/other-group.log" 2>&1
sed -i 's/backup_id = "other"/backup_id = "desktop"/' "$NESTEGG_CONFIG"
mount -o nodev,nosuid,noexec "$part" "$work/data"
find "$work/data/nestegg/store/.chunks" -type f -size +1M -printf '%P\n' | sort >"$work/chunks-after"
foreign=$(comm -13 "$work/chunks-before" "$work/chunks-after" | head -1)
[[ -n $foreign ]]
cp -a "$work/data/nestegg/store/.chunks/$foreign" "$work/foreign-chunk"
python3 - "$work/data/nestegg/store/.chunks/$foreign" <<'PYDAMAGE'
import sys
with open(sys.argv[1],'r+b') as f:
    f.seek(-1,2); byte=f.read(1); f.seek(-1,2); f.write(bytes([byte[0]^1]))
PYDAMAGE
umount "$work/data"
python3 - "$work/state/state.json" "$uuid" <<'PYDAMAGE'
import json,sys
p=sys.argv[1]; s=json.load(open(p)); m=s['stick_media'][sys.argv[2]]
m['full_checked_at']=None; m['full_verified_at']=None
with open(p,'w') as f: json.dump(s,f)
PYDAMAGE
head -c 16777216 /dev/urandom >"$work/source/random"
"$bin" backup --target stick >"$work/foreign-damaged.log" 2>&1
python3 - "$work/state/state.json" "$uuid" <<'PYDAMAGE'
import json,sys
s=json.load(open(sys.argv[1])); v=s['stick']; m=s['stick_media'][sys.argv[2]]
assert v['last_run']['state']=='succeeded' and v['last_run']['verified'],v
assert v['stick_damaged'] and any(p.startswith('host/other/') for p in v['server_verify_failed']),v
assert m['full_checked_at'] and not m['full_verified_at'],m
PYDAMAGE
"$bin" backup --target stick >"$work/foreign-no-rescan.log" 2>&1
if grep -q 'monthly PBS verification' "$work/foreign-no-rescan.log"; then exit 1; fi
mount -o nodev,nosuid,noexec "$part" "$work/data"
[[ $(find "$work/data/nestegg/store/host/desktop" -mindepth 1 -maxdepth 1 -type d | wc -l) == 1 ]]
cp -a "$work/foreign-chunk" "$work/data/nestegg/store/.chunks/$foreign"
umount "$work/data"
python3 - "$work/state/state.json" "$uuid" <<'PYDAMAGE'
import json,sys
p=sys.argv[1]; s=json.load(open(p)); s['stick_media'][sys.argv[2]]['full_checked_at']=None
with open(p,'w') as f: json.dump(s,f)
PYDAMAGE
"$bin" backup --target stick >"$work/foreign-recovered.log" 2>&1
python3 - "$work/state/state.json" <<'PYDAMAGE'
import json,sys
v=json.load(open(sys.argv[1]))['stick']; assert not v['stick_damaged'] and not v['server_verify_failed'],v
PYDAMAGE
echo 'PASS foreign snapshot damage reported without blocking success, retention or GC; monthly retry bounded'
# Interrupt only the official upload, leaving real orphan chunks. A later
# full-stick attempt must reclaim them with GC alone, without a success or prune.
mount -o nodev,nosuid,noexec "$part" "$work/data"
find "$work/data/nestegg/store/.chunks" -type f -size +1M -printf '%P\n' | sort >"$work/pre-failed-upload"
umount "$work/data"
cat >"$work/failing-client" <<EOF
#!/bin/bash
if [[ \$1 == backup ]]; then
    "$client_bin" "\$@" &
    pid=\$!
    sleep 3
    kill -KILL "\$pid" 2>/dev/null || true
    wait "\$pid" 2>/dev/null || true
    exit 1
fi
exec "$client_bin" "\$@"
EOF
chmod 0700 "$work/failing-client"
head -c 67108864 /dev/urandom >"$work/source/random"
sed -i '/system = false/a rate_limit = "4MiB"' "$NESTEGG_CONFIG"
if NESTEGG_PBS_CLIENT="$work/failing-client" "$bin" backup --target stick >"$work/failed-upload.log" 2>&1; then exit 1; fi
grep -q 'reclaiming orphaned chunks.*no prune' "$work/failed-upload.log"
sed -i '/rate_limit =/d' "$NESTEGG_CONFIG"
mount -o nodev,nosuid,noexec "$part" "$work/data"
find "$work/data/nestegg/store/.chunks" -type f -size +1M -printf '%P\n' | sort >"$work/post-failed-upload"
comm -13 "$work/pre-failed-upload" "$work/post-failed-upload" >"$work/orphan-chunks"
[[ -s $work/orphan-chunks ]]
retained=$(find "$work/data/nestegg/store/host" -name index.json.blob | wc -l)
umount "$work/data"
sleep 70
sed -i 's/minimum_free_bytes = 1048576/minimum_free_bytes = 9223372036854775807/' "$NESTEGG_CONFIG"
if "$bin" backup --target stick >"$work/full-gc.log" 2>&1; then exit 1; fi
grep -q 'reclaiming orphaned chunks.*no prune' "$work/full-gc.log"
mount -o nodev,nosuid,noexec "$part" "$work/data"
[[ $(find "$work/data/nestegg/store/host" -name index.json.blob | wc -l) == "$retained" ]]
while IFS= read -r orphan; do [[ ! -e $work/data/nestegg/store/.chunks/$orphan ]]; done <"$work/orphan-chunks"
umount "$work/data"
sed -i 's/minimum_free_bytes = 9223372036854775807/minimum_free_bytes = 1048576/' "$NESTEGG_CONFIG"
"$bin" backup --target stick >"$work/after-failed-upload.log" 2>&1
echo 'PASS orphan chunks from failed upload reclaimed by full-stick GC alone, without pruning snapshots'
# Damage backup data without damaging ext4, proving this is PBS verification
# failure rather than the filesystem-health gate. Restore the exact fixture
# chunk afterwards; never invent or repair PBS data in product code.
mount -o nodev,nosuid,noexec "$part" "$work/data"
chunk=$(find "$work/data/nestegg/store/.chunks" -type f -size +1M -print -quit)
[[ -n $chunk ]]
chunk_relative=${chunk#"$work/data/"}
cp -a "$chunk" "$work/original-chunk"
python3 - "$chunk" <<'PY'
import sys
with open(sys.argv[1], 'r+b') as f:
    f.seek(-1,2); byte=f.read(1); f.seek(-1,2); f.write(bytes([byte[0]^1]))
PY
umount "$work/data"
python3 - "$work/state/state.json" "$uuid" <<'PY'
import json,sys
p=sys.argv[1]; s=json.load(open(p)); s['stick_media'][sys.argv[2]]['full_verified_at']=None; s['stick_media'][sys.argv[2]]['full_checked_at']=None
with open(p,'w') as f: json.dump(s,f)
PY
if "$bin" backup --target stick >"$work/pbs-damaged.log" 2>&1; then exit 1; fi
python3 - "$work/state/state.json" <<'PY'
import json,sys
s=json.load(open(sys.argv[1])); assert s['stick']['last_run']['error']['kind']=='verification_failed'
PY
mount -o nodev,nosuid,noexec "$part" "$work/data"
cp -a "$work/original-chunk" "$work/data/$chunk_relative"
umount "$work/data"
"$bin" backup --target stick >"$work/pbs-recovered.log" 2>&1
echo 'PASS damaged chunks fail PBS verify independently of clean ext4, then reverify after recovery'

# Space checking must use PBS user availability, without weakening retention.
mount -o nodev,nosuid,noexec "$part" "$work/data"
available=$(df -B1 --output=avail "$work/data" | tail -1)
fallocate -l "$((available - 524288))" "$work/data/full-fixture"
umount "$work/data"
if "$bin" backup --target stick >"$work/full.log" 2>&1; then exit 1; fi
python3 - "$work/state/state.json" <<'PY'
import json,sys
s=json.load(open(sys.argv[1])); assert s['stick']['last_run']['error']['kind']=='stick_full'; assert s['stick']['consecutive_failures']==1
PY
"$bin" notify --dry-run >"$work/full-notify.log" 2>&1
grep -q 'stick_full' "$work/full-notify.log"
mount -o nodev,nosuid,noexec "$part" "$work/data"
rm "$work/data/full-fixture"
umount "$work/data"
grep -q 'reclaiming orphaned chunks.*no prune' "$work/full.log"
echo 'PASS stick full has its own state, notification and GC without prune'
# Another machine with the same ID has a valid proof of its own key; it still
# must be refused before upload/prune and must preserve the first snapshot.
cp "$work/credentials/encryption-key" "$work/original-key"
"$bin" key create --allow-plain --force >"$work/new-key.log" 2>&1
if "$bin" stick key-copy --uuid "$uuid" --password-fd 7 7<"$work/password" >"$work/new-key-copy.log" 2>&1; then exit 1; fi
"$bin" key export "$work/new-key-export" >"$work/new-key-export.log" 2>&1
"$bin" key confirm-exported --file "$work/new-key-export" --accept-local-copy >"$work/new-key-proof.log" 2>&1
if "$bin" backup --target stick >"$work/mismatch.log" 2>&1; then exit 1; fi
python3 - "$work/state/state.json" <<'PY'
import json,sys
s=json.load(open(sys.argv[1])); assert s['stick']['last_run']['error']['kind']=='key_mismatch'
PY
cp "$work/original-key" "$work/credentials/encryption-key"
"$bin" stick key-copy --uuid "$uuid" --password-fd 7 7<"$work/password" >"$work/original-copy.log" 2>&1
echo 'PASS group key mismatch refused'
rm "$work/uuid/$uuid"
code=0; "$bin" backup --target stick >"$work/absent.log" 2>&1 || code=$?
[[ $code == 75 ]]
python3 - "$work/state/state.json" <<'PY'
import json,sys
s=json.load(open(sys.argv[1])); assert s['stick']['last_run']['state']=='stick_not_plugged_in'
PY
ln -s "$part" "$work/uuid/$uuid"
echo 'PASS unplugged stick is availability, not a failure'
# Device-mapper EIO simulates actual failed storage IO, unlike lazy loop detach.
mapper=nestegg-desktop-test-$$
sectors=$(blockdev --getsz "$part")
start=$(lsblk -nro START "$part")
# Expose the filesystem only through the fault-injection mapping. Keeping its
# raw partition visible as well would intentionally trip cloned-UUID refusal.
losetup -d "$loop"; loop=
loop=$(losetup --find --show "$work/stick.img")
[[ ! -b ${loop}p1 ]]
echo "0 $sectors linear $loop $start" | dmsetup create "$mapper"
rm "$work/uuid/$uuid"; ln -s "/dev/mapper/$mapper" "$work/uuid/$uuid"
head -c 134217728 /dev/urandom >"$work/source/random"
sed -i '/system = false/a rate_limit = "4MiB"' "$NESTEGG_CONFIG"
"$bin" backup --target stick >"$work/unplug.log" 2>&1 &
backup_pid=$!
for _ in {1..120}; do
    if [[ -f $work/state/current.json ]] && python3 - "$work/state/current.json" "$work/state/runs" <<'PY'
import json,sys,pathlib
r=json.load(open(sys.argv[1])); log=pathlib.Path(sys.argv[2])/(r['id']+'.log')
sys.exit(0 if r['state']=='uploading' and log.exists() and 'Starting backup:' in log.read_text() else 1)
PY
    then break; fi
    kill -0 "$backup_pid"
    sleep .5
done
sleep 2
dmsetup suspend --noflush "$mapper"
echo "0 $sectors error" | dmsetup load "$mapper"
dmsetup resume "$mapper"
code=0; wait "$backup_pid" || code=$?; backup_pid=
[[ $code != 0 ]]
# Restore only this harness's mapping after every writer has exited.
dmsetup suspend --noflush "$mapper"
echo "0 $sectors linear $loop $start" | dmsetup load "$mapper"
dmsetup resume "$mapper"
# EIO may fail before ext4 commits damaged metadata. Test the health gate
# deterministically as a separate fault: corrupt ONLY our owned filesystem's
# free-block summary while it is offline. PBS verify cannot repair this.
debugfs -w -R 'ssv free_blocks_count 1' "/dev/mapper/$mapper" >"$work/metadata-fault.log" 2>&1
code=0; "$bin" backup --target stick >"$work/dirty.log" 2>&1 || code=$?
[[ $code == 1 ]]
python3 - "$work/state/state.json" <<'PY'
import json,sys
s=json.load(open(sys.argv[1])); assert s['stick']['last_run']['error']['kind']=='filesystem_damaged'
PY
code=0; e2fsck -fy "/dev/mapper/$mapper" >"$work/repair.log" 2>&1 || code=$?
[[ $code == 0 || $code == 1 ]]
sed -i '/rate_limit =/d' "$NESTEGG_CONFIG"
"$bin" backup --target stick >"$work/recovered.log" 2>&1
echo 'PASS unplug mid-upload, filesystem damage distinct from PBS verify, offline repair and recovery'
echo 'PASS desktop phase-two acceptance (disposable image; no real USB hardware)'
