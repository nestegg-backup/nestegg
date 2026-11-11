#!/usr/bin/env bash
# Disposable namespace/loop/PBS experiment. Run as root, with build-root.sh output.
# KEEP_LOGS=<existing directory> copies private logs only (never commit them).
set -euo pipefail
# Cleanup may run while a function redirects stdout into a private job log.
exec 3>&1
[[ $EUID == 0 ]] || { echo 'Run with sudo -n' >&2; exit 2; }
if [[ ${SPIKE_PRIVATE_MOUNTS:-0} != 1 ]]; then
  exec unshare --mount --propagation private env SPIKE_PRIVATE_MOUNTS=1 "$0" "$@"
fi
here=$(cd "$(dirname "$0")" && pwd)
build=$(realpath "${1:?usage: sudo run.sh BUILD_DIRECTORY}")
work=$(mktemp -d /tmp/nestegg-stick-run.XXXXXX)
chmod 0700 "$work"
loop=; mapper="nestegg-stick-spike-$$"; mapped=0; launcher=; leader=; bg=
stop() {
  if [[ -n $leader ]]; then kill -KILL "$leader" 2>/dev/null || true; fi
  if [[ -n $launcher ]]; then wait "$launcher" 2>/dev/null || true; fi
  launcher=; leader=
}
cleanup() {
  local status=$? cleanup_failed=0 record pid
  trap - EXIT
  trap '' INT TERM
  set +e
  # Each client has its own session, including foreground clients started in
  # command substitutions. Records remain visible to the top-level trap.
  for record in "$work/clients/"*; do
    [[ -f $record ]] || continue
    pid=${record##*/}
    kill -TERM -- "-$pid" 2>/dev/null || true
  done
  for record in "$work/clients/"*; do
    [[ -f $record ]] || continue
    pid=${record##*/}
    kill -KILL -- "-$pid" 2>/dev/null || true
  done
  [[ -z $bg ]] || { kill "$bg" 2>/dev/null; wait "$bg" 2>/dev/null; }
  stop
  if mountpoint -q "$work/data"; then
    umount "$work/data" || cleanup_failed=1
  fi
  if [[ $mapped != 0 ]]; then
    if dmsetup remove --retry "$mapper"; then mapped=0; else cleanup_failed=1; fi
  fi
  if [[ $mapped == 0 && -n $loop ]]; then
    losetup -d "$loop" || cleanup_failed=1
  fi
  if mountpoint -q "$work/lower"; then
    umount "$work/lower" || cleanup_failed=1
  fi
  if [[ -n ${KEEP_LOGS:-} ]]; then
    mkdir -p "$KEEP_LOGS" || cleanup_failed=1
    for record in "$work/"*.log; do
      [[ ! -f $record ]] || cp "$record" "$KEEP_LOGS/" || cleanup_failed=1
    done
  fi
  if [[ $cleanup_failed != 0 ]]; then
    echo "ERROR cleanup incomplete; owned resources and files retained at $work" >&3
    exit 1
  fi
  rm -rf "$work" || exit 1
  echo "CLEANUP complete (exit $status)" >&3
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$work/lower" "$work/runtime" "$work/data" "$work/input" "$work/home" "$work/clients"
mount -t squashfs -o loop,ro "$build/pbs.squashfs" "$work/lower"
truncate -s 12G "$work/data.img"
loop=$(losetup --find --show "$work/data.img")
sectors=$(blockdev --getsz "$loop")
dmsetup create "$mapper" --table "0 $sectors linear $loop 0"
mapped=1
mkfs.ext4 -q -m 0 -L NESTEGG_DATA "/dev/mapper/$mapper"
mount -o nodev,nosuid,noexec "/dev/mapper/$mapper" "$work/data"
mkdir "$work/data/nestegg"
chown 34:34 "$work/data/nestegg"
client_bin=$build/client/usr/bin/proxmox-backup-client
root=$work/runtime/root
server() { nsenter -t "$leader" -m -n chroot "$root" "$@"; }
start() {
  local mode=${1:-normal}
  local label=${2:-$mode}
  unshare --mount --pid --net --uts --fork --kill-child "$here/namespace.sh" "$work" "$work/data" "$here" "$mode" >"$work/server-$label.log" 2>&1 &
  launcher=$!
  for ((i=0;i<120;i++)); do
    leader=$(awk '{print $1}' "/proc/$launcher/task/$launcher/children" 2>/dev/null || true)
    if [[ -n $leader ]] && server test -f /run/spike-ready 2>/dev/null; then
      server cat /run/token-secret >"$work/token"
      server cat /run/fingerprint >"$work/fingerprint"
      chmod 0600 "$work/token" "$work/fingerprint"
      echo "READY $label"
      return 0
    fi
    kill -0 "$launcher" 2>/dev/null || { cat "$work/server-$label.log"; return 1; }
    sleep .5
  done
  cat "$work/server-$label.log"; return 1
}
client() {
  # Join ONLY the net namespace: input files are in the host mount namespace.
  local child rc=0
  setsid timeout --foreground 180 nsenter -t "$leader" -n env -u ALL_PROXY -u HTTPS_PROXY -u HTTP_PROXY \
    HOME="$work/home" PBS_REPOSITORY='nestegg@pbs!stick@127.0.0.1:stick' \
    PBS_PASSWORD_FD=9 PBS_FINGERPRINT="$(cat "$work/fingerprint")" \
    PBS_ENCRYPTION_PASSWORD_FD=8 "$client_bin" "$@" 9<"$work/token" 8<"$work/pass" &
  child=$!
  printf '%s\n' "$child" >"$work/clients/$child"
  wait "$child" || rc=$?
  rm -f "$work/clients/$child"
  return "$rc"
}
backup() { client backup "files.pxar:$work/input" --backup-id "${backup_id:-desktop}" --keyfd 7 "$@" 7<"$work/key.json"; }
verify() {
  server proxmox-backup-manager verify-job create spike --store stick --ignore-verified false
  server proxmox-backup-manager verify-job run spike
  server proxmox-backup-manager verify-job remove spike
}
printf 'disposable fixture\n' >"$work/input/report.txt"
openssl rand -base64 32 >"$work/pass"
# Python's pty creates a controlling terminal without requiring a user terminal.
python3 - "$client_bin" "$work/key.json" "$work/pass" <<'PY'
import os, pty, select, sys, time
binary, key, password = sys.argv[1:]
secret = open(password, 'rb').read().strip()
pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, 'key', 'create', key, '--kdf', 'scrypt'])
buf = b''
answers = 0
deadline = time.monotonic() + 30
while time.monotonic() < deadline:
    if select.select([fd], [], [], .2)[0]:
        try:
            data = os.read(fd, 4096)
        except OSError:
            break
        if not data:
            break
        buf += data
        # Both initial and confirmation prompts end with ': '. No secret echoed.
        if buf.rstrip().endswith(b':'):
            os.write(fd, secret + b'\n')
            answers += 1
            buf = b''
    got, status = os.waitpid(pid, os.WNOHANG)
    if got:
        assert os.waitstatus_to_exitcode(status) == 0, 'key create failed'
        break
else:
    os.kill(pid, 9)
    os.waitpid(pid, 0)
    raise RuntimeError('key create timed out')
assert os.path.exists(key) and answers == 2, 'key create incomplete'
PY
python3 - "$work/key.json" <<'PY'
import json, sys
assert 'Scrypt' in str(json.load(open(sys.argv[1]))['kdf'])
print('PASS key created via pty with scrypt')
PY
start normal first
nsenter -t "$leader" -n ip -br link >"$work/netns.log"
nsenter -t "$leader" -n ss -lntp >>"$work/netns.log"
[[ $(nsenter -t "$leader" -n ip -o link | wc -l) == 1 ]]
if ss -lntp 'sport = :8007' | tail -n +2 | grep -q .; then echo 'Host port unexpectedly in use'; exit 1; fi
echo 'PASS only loopback; no host port 8007'
backup >"$work/backup-first.log" 2>&1
first=$(client snapshot list --output-format json | python3 -c 'import sys,json,datetime; s=json.load(sys.stdin)[0]; print("host/desktop/"+datetime.datetime.fromtimestamp(s["backup-time"],datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))')
client restore "$first" files.pxar "$work/restore-first" --keyfd 7 7<"$work/key.json" >"$work/restore-first.log" 2>&1
diff -r "$work/input" "$work/restore-first"
cp "$work/fingerprint" "$work/fingerprint-first"
cp "$work/token" "$work/token-first"
stat -c "%u %g %a" "$work/data/nestegg/.chunks" >"$work/chunk-mode.log"
stop
sleep 2
start normal second
if cmp -s "$work/fingerprint-first" "$work/fingerprint"; then exit 1; fi
if cmp -s "$work/token-first" "$work/token"; then exit 1; fi
backup >"$work/backup-second.log" 2>&1
[[ $(cat "$work/data/nestegg/host/desktop/owner") == 'nestegg@pbs!stick' ]]
echo 'PASS encrypted backup/restore; fresh certificate and secret; same owner after restart'
client prune host/desktop --keep-last 1 >"$work/prune.log" 2>&1
verify >"$work/verify.log" 2>&1
server proxmox-backup-manager garbage-collection start stick >"$work/gc.log" 2>&1
echo 'PASS prune, server verify, GC (cutoff 1 minute)'
client snapshot list --output-format json >"$work/snapshots-pruned.json"
# Baseline: real random data, not sparse or compressible. One host disk only.
dd if=/dev/urandom of="$work/input/random.bin" bs=16M count=192 status=none
start_seconds=$SECONDS
backup_id=speed backup >"$work/speed.log" 2>&1
printf 'BASELINE 3 GiB random: %s seconds\n' "$((SECONDS-start_seconds))"
verify >"$work/verify-speed.log" 2>&1
# A device-mapper error target causes real EIO, unlike lazy loop detach.
dd if=/dev/urandom of="$work/input/pull.bin" bs=16M count=64 status=none
backup_id=interrupted backup --rate 8388608 >"$work/pull.log" 2>&1 &
bg=$!
for ((i=0;i<60;i++)); do
  grep -q 'Upload directory' "$work/pull.log" && break
  sleep .2
done
sleep 3
kill -0 "$bg"
dmsetup suspend --noflush --nolockfs "$mapper"
dmsetup load "$mapper" --table "0 $sectors error"
dmsetup resume "$mapper"
pull_rc=0
wait "$bg" || pull_rc=$?
[[ $pull_rc -ne 0 && $pull_rc -ne 124 ]]
grep -Eq 'EIO: I/O error|Input/output error|EROFS: Read-only file system' "$work/pull.log"
echo 'PASS pulled-drive backup fails with EIO or consequent read-only filesystem'
bg=
stop
umount "$work/data"
dmsetup suspend --noflush --nolockfs "$mapper"
dmsetup load "$mapper" --table "0 $sectors linear $loop 0"
dmsetup resume "$mapper"
fsck_rc=0
e2fsck -fy "/dev/mapper/$mapper" >"$work/fsck.log" 2>&1 || fsck_rc=$?
[[ $fsck_rc -le 1 ]]
mount -o nodev,nosuid,noexec "/dev/mapper/$mapper" "$work/data"
start normal after-pull
verify >"$work/verify-after-pull.log" 2>&1
client snapshot list --output-format json >"$work/snapshots-after-pull.json"
python3 - "$work/snapshots-after-pull.json" <<'PYASSERT'
import json, sys
snapshots = json.load(open(sys.argv[1]))
assert sorted(s["backup-id"] for s in snapshots) == ["desktop", "speed"], snapshots
PYASSERT
latest=$(client snapshot list host/desktop --output-format json | python3 -c 'import sys,json,datetime; s=json.load(sys.stdin)[0]; print("host/desktop/"+datetime.datetime.fromtimestamp(s["backup-time"],datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))')
client restore "$latest" files.pxar "$work/restore-after-pull" --keyfd 7 7<"$work/key.json" >"$work/restore-after-pull.log" 2>&1
cmp "$work/input/report.txt" "$work/restore-after-pull/report.txt"
echo 'PASS next start, server verify and decrypted restore after EIO and offline e2fsck'
stop
if start disabled ipv6-disabled; then
  echo 'PASS netns IPv6 disabled: PBS still starts'
else
  echo 'FAIL netns IPv6 disabled: PBS does not start (see log)'
fi
stop
# User namespaces need mapped backup uid/gid, not just uid 0.
# The child shell expands $1, not this shell.
# shellcheck disable=SC2016
if unshare --user --map-root-user --mount sh -c 'mount -t tmpfs tmpfs "$1" && touch "$1/file" && chown 34:34 "$1/file"' sh "$work/runtime" >"$work/userns.log" 2>&1; then
  echo 'PASS single-map user namespace ownership'
else
  echo 'FAIL single-map user namespace cannot own files as backup uid/gid'
fi
# Detach before importing a copy into the separate real PBS.
umount "$work/data"
dmsetup remove --retry "$mapper"; mapped=0
losetup -d "$loop"; loop=
"$here/reuse.sh" "$work/data.img" >"$work/reuse.log" 2>&1
echo 'PASS removable datastore reused by e2e PBS'
echo 'Completed core namespace spike'
