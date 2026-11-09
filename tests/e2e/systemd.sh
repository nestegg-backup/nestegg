#!/usr/bin/env bash
# Package and service test: install the .deb into a container running
# systemd, store secrets with systemd-creds, run backups through the timer
# unit and the daemon, use the CLI and web UI as an unprivileged user and
# change settings through the API, and get told about failed, recovered and
# unwanted states through the health check (hook command and desktop
# notification, decision 0017).
#
# The systemd container needs the host cgroup namespace (--cgroupns=host with
# a writable /sys/fs/cgroup) and CAP_SYS_ADMIN without seccomp/AppArmor
# confinement. Only run this on a disposable test machine.
#
# Usage: tests/e2e/systemd.sh [path/to/nestegg_*.deb]
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
deb="${1:-$(ls "$repo"/target/nestegg_*_amd64.deb 2>/dev/null | tail -1)}"
net="nestegg-e2e-$$"
pbs="nestegg-e2e-pbs-$$"
client="nestegg-e2e-sd-$$"

cleanup() {
  [[ -n "${KEEP:-}" ]] && { echo "(KEEP set: containers $pbs $client left running)"; return; }
  docker rm -f "$pbs" "$client" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
}
trap cleanup EXIT

[[ -f "$deb" ]] || { echo "package not found (run: cargo deb -p nestegg)"; exit 2; }
deb="$(realpath "$deb")"
# shellcheck source=lib.sh
source "$here/lib.sh"
as_alice() { docker exec -u alice "$client" "$@"; }

echo "== building images"
build_pbs_image
build_systemd_images

start_pbs

echo "== starting systemd client and installing $(basename "$deb")"
start_systemd_client -v "$deb:/tmp/nestegg.deb:ro"
check "systemd enforces mount sandboxing" bash -c "docker exec $client systemd-run --quiet --wait --pipe \
  -p ProtectHome=read-only touch /root/.sandbox-probe 2>&1 | grep -q 'Read-only file system'"
# shared/ is setgid: restores through the daemon unit must handle the bit.
on_client bash -c 'useradd -m alice && echo "hello" > /home/alice/notes.txt && mkdir /home/alice/shared && chown -R alice: /home/alice && chmod 2775 /home/alice/shared'
check "package installs" on_client bash -c 'DEBIAN_FRONTEND=noninteractive apt-get install -y -q /tmp/nestegg.deb >/tmp/apt.log 2>&1'
check "group nestegg created" on_client getent group nestegg
check "runtime dir from tmpfiles" bash -c "[[ \$(docker exec $client stat -c '%a %G' /run/nestegg) == '750 nestegg' ]]"
check "units installed" on_client systemctl cat nestegg.service nestegg-backup.service nestegg-backup.timer
check "health check units installed" on_client systemctl cat nestegg-notify.service nestegg-notify.timer
check "health check timer enabled by the package" on_client systemctl is-enabled --quiet nestegg-notify.timer
check "health check timer running" on_client systemctl is-active --quiet nestegg-notify.timer
check "backup unit starts the check after a success" bash -c "docker exec $client systemctl show -p OnSuccess --value nestegg-backup.service | grep -qx nestegg-notify.service"
check "backup unit starts the check after a failure" bash -c "docker exec $client systemctl show -p OnFailure --value nestegg-backup.service | grep -qx nestegg-notify.service"
check "health check does nothing before the setup" bash -c "docker exec $client nestegg notify | grep -q 'not configured'"
check "package recommends libnotify-bin" bash -c "dpkg-deb -f '$deb' Recommends | grep -q libnotify-bin"

echo "== setup"
# --no-system: the container's overlay root cannot be backed up as
# system.pxar; system mode would make every backup "incomplete".
on_client nestegg init --server pbs.e2e.test --datastore store1 --namespace desktops \
  --auth-id 'backup@pbs!desktop' --fingerprint "$fp" --backup-id sd-desktop --no-system >/dev/null
# The health check reports to a hook command (root-only configuration).
docker exec -i "$client" tee /usr/local/bin/nestegg-test-hook >/dev/null <<'HOOK'
#!/bin/sh
# One line per event: event|host|last success|message from stdin
printf '%s|%s|%s|%s\n' "$NESTEGG_EVENT" "$NESTEGG_HOSTNAME" "$NESTEGG_LAST_SUCCESS" "$(cat | tr '\n' ' ')" >> /root/hook-events
HOOK
on_client chmod 755 /usr/local/bin/nestegg-test-hook
docker exec -i "$client" tee -a /etc/nestegg/config.toml >/dev/null <<'NOTIFY'

[notify]
command = ["/usr/local/bin/nestegg-test-hook"]
NOTIFY
# Kernel modules must reach timer snapshots: a sandbox option such as
# ProtectKernelModules=yes hides /usr/lib/modules behind an empty tmpfs.
# The container's overlay root makes nestegg skip system.pxar, so the
# directory gets its own archive here; on a real disk it is in system.pxar.
on_client bash -c 'mkdir -p /usr/lib/modules/0.0.0-nestegg-e2e && echo sentinel > /usr/lib/modules/0.0.0-nestegg-e2e/sentinel'
on_client sed -i '0,/^\[\[backup\.sources\]\]$/s//[[backup.sources]]\narchive = "e2e-modules"\npath = "\/usr\/lib\/modules"\n\n&/' /etc/nestegg/config.toml
docker exec -i "$client" nestegg credentials set-password <<<"$token_secret" >/dev/null
check "password stored with systemd-creds" on_client test -f /etc/nestegg/credentials/password.cred
check "no plain password file" bash -c "! docker exec $client test -e /etc/nestegg/credentials/password"
check "credential file is not plain text" bash -c "! docker exec $client grep -q '$token_secret' /etc/nestegg/credentials/password.cred"
on_client nestegg key create >/dev/null
check "key stored with systemd-creds" on_client test -f /etc/nestegg/credentials/encryption-key.cred
on_client nestegg key export /root/key.json >/dev/null
on_client nestegg key confirm-exported --file /root/key.json --accept-local-copy >/dev/null
on_client usermod -aG nestegg alice
check "services enable" on_client systemctl enable --now nestegg.service nestegg-backup.timer
sleep 1
check "daemon active" on_client systemctl is-active --quiet nestegg.service
check "timer active" on_client systemctl is-active --quiet nestegg-backup.timer
check "check passes" on_client nestegg check

echo "== backup through the systemd unit"
check "timer unit run succeeds" on_client systemctl start nestegg-backup.service
check "unit result success" bash -c "[[ \$(docker exec $client systemctl show -p Result --value nestegg-backup.service) == success ]]"
check "status ok for unprivileged user" bash -c "docker exec -u alice $client nestegg status | grep -q 'Health:       ok'"
check "history readable by group member" bash -c "docker exec -u alice $client nestegg history | grep -q '^ok'"
ROOT_API="docker exec $client curl -s --unix-socket /run/nestegg/api.sock"
timer_snap="$(on_client nestegg snapshots --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
timer_snap="${timer_snap##*/}"
check "timer snapshot contains /usr/lib/modules" bash -c "
  $ROOT_API 'http://localhost/api/v1/snapshots/$timer_snap/browse?archive=e2e-modules.pxar&path=/0.0.0-nestegg-e2e' | grep -q '\"name\":\"sentinel\"' ||
  $ROOT_API 'http://localhost/api/v1/snapshots/$timer_snap/browse?archive=system.pxar&path=/usr/lib/modules/0.0.0-nestegg-e2e' | grep -q '\"name\":\"sentinel\"'"

echo "== backup requested by the unprivileged user (daemon + systemd-run)"
check "user can start a backup" bash -c "docker exec -u alice $client nestegg backup | grep -q 'Backup started'"
for _ in $(seq 1 60); do
  runs="$(as_alice nestegg history --json | grep -c '"state": "succeeded"' || true)"
  [[ "$runs" -ge 2 ]] && break
  sleep 1
done
check "manual run finished" test "${runs:-0}" -ge 2
check "user can list snapshots via daemon" bash -c "[[ \$(docker exec -u alice $client nestegg snapshots --json | grep -c '\"snapshot\"') -ge 2 ]]"
check "non-member is refused" bash -c "docker exec $client useradd -m bob && ! docker exec -u bob $client nestegg snapshots >/dev/null 2>&1"

echo "== restore through the daemon unit (its sandbox applies to restores)"
# Start an API restore of alice's home; prints the final state.
# $1 runs the request: as_alice (root staging, then a copy as alice) or
# on_client (root, direct restore into a root-owned directory).
api_restore() {
  local as="$1" target="$2" resp id state=""
  resp="$("$as" curl -s --unix-socket /run/nestegg/api.sock -X POST -H 'Content-Type: application/json' \
    -d "{\"snapshot_time\":\"$timer_snap\",\"archive\":\"home.pxar\",\"paths\":[\"/alice\"],\"target_dir\":\"$target\"}" \
    http://localhost/api/v1/restores)"
  id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$resp")"
  [[ -n "$id" ]] || { echo "rejected: $resp" >&2; return; }
  for _ in $(seq 1 60); do
    state="$("$as" curl -s --unix-socket /run/nestegg/api.sock "http://localhost/api/v1/restores/$id" | sed -n 's/.*"state":"\([^"]*\)".*/\1/p')"
    [[ "$state" != running ]] && break
    sleep 1
  done
  echo "$state"
}
check "API restore into the home directory" test "$(api_restore as_alice /home/alice/api-restore)" = succeeded
check "home restore: content arrived" on_client cmp /home/alice/notes.txt /home/alice/api-restore/alice/notes.txt
# The copy as alice drops the setgid bit (tar without -p); the root restore
# below checks that the bit itself survives the daemon's sandbox.
check "home restore: directory from a setgid source restored" on_client test -d /home/alice/api-restore/alice/shared
check "home restore: owned by the caller" bash -c "[[ -z \$(docker exec $client find /home/alice/api-restore ! -user alice) ]]"
check "API restore into /tmp" test "$(api_restore as_alice /tmp/alice-api-restore)" = succeeded
check "tmp restore: visible outside the daemon" on_client test -f /tmp/alice-api-restore/alice/notes.txt
check "API restore as root into /root" test "$(api_restore on_client /root/api-restore)" = succeeded
check "root restore: setgid bit kept" on_client test -g /root/api-restore/alice/shared

echo "== web UI as unprivileged user"
docker exec -d -u alice "$client" bash -c 'nestegg ui --no-browser --port 18080 > /tmp/ui.out 2>&1'
sleep 2
token="$(on_client sed -n 's/.*token=\([0-9a-f]*\).*/\1/p' /tmp/ui.out | head -1)"
check "ui prints a token" test -n "$token"
check "ui URL carries the token in the fragment" on_client grep -q '/#token=' /tmp/ui.out
check "ui rejects API requests without token" bash -c "[[ \$(docker exec $client curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18080/api/v1/status) == 401 ]]"
check "ui proxies the API with bearer token" bash -c "docker exec $client curl -sf -H 'Authorization: Bearer $token' http://127.0.0.1:18080/api/v1/status | grep -q '\"health\":\"ok\"'"
check "ui sets no cookies" bash -c "! docker exec $client curl -s -D - -o /dev/null http://127.0.0.1:18080/ | grep -qi 'set-cookie'"
check "ui only listens on loopback" bash -c "docker exec $client grep -qi ' 0100007F:46A0 ' /proc/net/tcp && ! docker exec $client grep -qi ' 00000000:46A0 ' /proc/net/tcp"

echo "== settings changed through the API (decision 0013)"
contains() { grep -q -- "$2" <<<"$1" || { echo "    no '$2' in:"; show "$1"; return 1; }; }
# PATCH /config as $1 with body $2; prints the response body, then the status code.
patch_config() {
  "$1" curl -s --unix-socket /run/nestegg/api.sock -X PATCH -H 'Content-Type: application/json' \
    -d "$2" -w '\n%{http_code}' http://localhost/api/v1/config
}
cfg=/etc/nestegg/config.toml
out="$(patch_config as_alice '{"interval_hours":12,"overdue_after_hours":30,"excludes":["**/.cache","*.iso"],"automatic_backups":false}')"
check "group member changes settings" test "$(tail -1 <<<"$out")" = 200
check "response shows the new values" contains "$out" '"interval_hours":12'
check "config.toml has the new interval" on_client grep -qx 'interval_hours = 12' "$cfg"
check "config.toml has the new excludes" on_client grep -q '"\*\.iso"' "$cfg"
check "config.toml keeps the header comment" bash -c "docker exec $client head -1 $cfg | grep -q '^# nestegg configuration'"
check "config.toml keeps the hand-added source" on_client grep -q 'archive = "e2e-modules"' "$cfg"
check "config.toml keeps the [notify] section" on_client grep -q 'nestegg-test-hook' "$cfg"
check "config.toml keeps owner and mode" bash -c "[[ \$(docker exec $client stat -c '%a %U:%G' $cfg) == '644 root:root' ]]"
check "no temporary files left in /etc/nestegg" no_output on_client find /etc/nestegg -maxdepth 1 -name '.*.tmp'
check "timer disabled" exits_with 1 on_client systemctl is-enabled --quiet nestegg-backup.timer
check "timer stopped" exits_with 3 on_client systemctl is-active --quiet nestegg-backup.timer
check "status shows the new interval" bash -c "docker exec -u alice $client nestegg status --json | grep -q '\"interval_hours\": 12'"
out="$(patch_config as_alice '{"automatic_backups":true}')"
check "group member enables automatic backups" test "$(tail -1 <<<"$out")" = 200
check "timer enabled again" on_client systemctl is-enabled --quiet nestegg-backup.timer
check "timer running again" on_client systemctl is-active --quiet nestegg-backup.timer
before="$(on_client sha256sum "$cfg")"
out="$(patch_config as_alice '{"server":"pbs.example.com"}')"
check "server cannot be changed" test "$(tail -1 <<<"$out")" = 400
check "refusal names the reason" contains "$out" 'cannot be changed here'
out="$(patch_config as_alice '{"excludes":["**"]}')"
check "exclude-everything pattern refused" contains "$out" 'would exclude everything'
out="$(patch_config as_alice '{"wake_on_lan":true}')"
check "Wake-on-LAN without MAC refused" contains "$out" '\[wake\]'
out="$(patch_config as_alice '{"command":["/bin/sh","-c","id"]}')"
check "the notify command cannot be set through the API" test "$(tail -1 <<<"$out")" = 400
out="$(patch_config as_alice '{"notify":{"desktop":false}}')"
check "the notify section cannot be set through the API" test "$(tail -1 <<<"$out")" = 400
check "refused changes leave config.toml alone" test "$(on_client sha256sum "$cfg")" = "$before"
check "change logged with the caller" bash -c "docker exec $client journalctl -u nestegg.service --no-pager | grep -q 'settings changed by alice (uid [0-9]*): interval_hours: 24 -> 12'"
check "refusal logged with the caller" bash -c "docker exec $client journalctl -u nestegg.service --no-pager | grep -q 'settings change by alice (uid [0-9]*) refused'"
check "web UI forwards a settings change" bash -c "[[ \$(docker exec $client curl -s -o /dev/null -w '%{http_code}' -X PATCH \
  -H 'Authorization: Bearer $token' -H 'Content-Type: application/json' -d '{\"overdue_after_hours\":40}' \
  http://127.0.0.1:18080/api/v1/config) == 200 ]] && docker exec $client grep -qx 'overdue_after_hours = 40' $cfg"
check "web UI rejects a cross-origin settings change" bash -c "[[ \$(docker exec $client curl -s -o /dev/null -w '%{http_code}' -X PATCH \
  -H 'Authorization: Bearer $token' -H 'Origin: http://attacker.example.com' -H 'Content-Type: application/json' \
  -d '{\"automatic_backups\":false}' http://127.0.0.1:18080/api/v1/config) == 403 ]]"
check "timer still enabled after the rejected request" on_client systemctl is-enabled --quiet nestegg-backup.timer

echo "== notifications: hook command and desktop (decision 0017)"
alice_uid="$(on_client id -u alice)"
alice_gid="$(on_client id -g alice)"
nestegg_gid="$(on_client getent group nestegg | cut -d: -f3)"
hook_events() { on_client cat /root/hook-events 2>/dev/null || true; }
count_events() { hook_events | grep -c "^$1|" || true; }
# wait_for_event EVENT N: at least N lines of EVENT in the hook's file.
wait_for_event() {
  for _ in $(seq 1 40); do
    [[ "$(count_events "$1")" -ge "$2" ]] && return 0
    sleep 1
  done
  return 1
}
notify_invocation() { on_client systemctl show -p InvocationID --value nestegg-notify.service; }
# wait_new_notify OLD: the check unit ran again since invocation OLD.
wait_new_notify() {
  local now
  for _ in $(seq 1 40); do
    now="$(notify_invocation)"
    [[ "$now" != "$1" && "$(on_client systemctl is-active nestegg-notify.service || true)" == inactive ]] && return 0
    sleep 1
  done
  return 1
}
desktop_log() { on_client cat /tmp/notify-send.log 2>/dev/null || true; }
desktop_count() { desktop_log | grep -c '^uid=' || true; }
# Timer runs only start a backup when the last success is old.
age_state() { on_client sed -Ei 's/("finished_at": ?")[^"]*"/\12000-01-01T00:00:00Z"/g' /var/lib/nestegg/state.json; }

check "healthy backups so far produced no message" test -z "$(hook_events)"
check "health check says all is well" bash -c "docker exec $client nestegg notify | grep -q 'everything is fine'"

# A stand-in for alice's desktop session: the bus socket below /run/user/<uid>
# and a notify-send that records who runs it and with which environment.
# (No desktop in the container, so no real notification service.)
on_client install -d -m 755 /run/user
on_client install -d -m 700 -o alice -g alice "/run/user/$alice_uid"
check "fake session bus socket" on_client perl -MSocket -e 'socket(S, PF_UNIX, SOCK_STREAM, 0) or die; bind(S, sockaddr_un($ARGV[0])) or die' "/run/user/$alice_uid/bus"
docker exec -i "$client" tee /usr/local/bin/notify-send >/dev/null <<'STUB'
#!/bin/sh
{ echo "uid=$(id -u) gid=$(id -g) groups=$(id -G)"; echo "bus=$DBUS_SESSION_BUS_ADDRESS"; echo "xdg=$XDG_RUNTIME_DIR"; for a in "$@"; do echo "arg=$a"; done; echo "--"; } >> /tmp/notify-send.log
STUB
on_client chmod 755 /usr/local/bin/notify-send
on_client install -m 666 /dev/null /tmp/notify-send.log

out="$(on_client nestegg notify --test)"
check "test message reaches the desktop of the logged-in user" contains "$out" 'desktop: sent to alice'
check "test message reaches the hook" contains "$out" 'command: ran successfully'
check "hook got the test event" test "$(count_events test)" = 1
check "notify-send runs as alice with her groups" bash -c "docker exec $client grep -Eq '^uid=$alice_uid gid=$alice_gid groups=.*\\b$nestegg_gid\\b' /tmp/notify-send.log"
check "notify-send does not keep root's group" bash -c "! docker exec $client grep -Eq '^uid=.* groups=(.* )?0( |\$)' /tmp/notify-send.log"
check "notify-send gets alice's session bus" on_client grep -qx "bus=unix:path=/run/user/$alice_uid/bus" /tmp/notify-send.log
check "notify-send gets alice's runtime directory" on_client grep -qx "xdg=/run/user/$alice_uid" /tmp/notify-send.log
check "notify-send is told the app name" on_client grep -qx 'arg=--app-name=nestegg' /tmp/notify-send.log
check "users without a session bus get nothing" test "$(desktop_count)" = 1
out="$(on_client nestegg notify --test --dry-run)"
check "dry run names the user and sends nothing" contains "$out" 'would notify alice'
check "dry run changed nothing" bash -c "[[ $(desktop_count) == 1 && $(count_events test) == 1 ]]"
on_client sh -c 'echo LANG=de_DE.UTF-8 > /etc/default/locale'
out="$(on_client nestegg notify --test --dry-run)"
check "texts follow the system language" contains "$out" 'Testnachricht'
# Ubuntu links /etc/default/locale and /etc/locale.conf; remove both.
on_client rm -f /etc/default/locale /etc/locale.conf

echo "-- a failed backup is reported at once (OnFailure=), not repeated, and its end is reported"
on_client cp "$cfg" /root/config.good
on_client bash -c "printf '\n[[backup.sources]]\narchive = \"e2e-missing\"\npath = \"/nonexistent-dir\"\n' >> $cfg"
age_state
before="$(notify_invocation)"
check "backup unit fails" exits_with 1 on_client systemctl start nestegg-backup.service
check "the check ran by itself" wait_new_notify "$before"
check "hook got one failing event" test "$(count_events failing)" = 1
check "event carries host, last success and the reason" bash -c "docker exec $client grep -Eq '^failing[|][^|]+[|]2000-01-01T00:00:00Z[|].*/nonexistent-dir does not exist.*nestegg ui' /root/hook-events"
check "desktop got the failure as a critical message" bash -c "[[ \$(docker exec $client grep -c -x 'arg=--urgency=critical' /tmp/notify-send.log) == 1 ]] && docker exec $client grep -qx 'arg=Backup failed' /tmp/notify-send.log"
before="$(notify_invocation)"
check "second failure of the same kind: backup unit fails again" exits_with 1 on_client systemctl start nestegg-backup.service
check "the check ran again" wait_new_notify "$before"
check "no second failing event within 24 hours" test "$(count_events failing)" = 1
check "no second desktop message" test "$(desktop_count)" = 2
on_client systemctl start nestegg-notify.service
check "the hourly check stays quiet too" bash -c "[[ $(count_events failing) == 1 && $(desktop_count) == 2 ]]"
check "the check logs why" bash -c "docker exec $client journalctl -u nestegg-notify.service --no-pager | grep -q 'known problems, nothing new'"

on_client bash -c "cat /root/config.good > $cfg"
before="$(notify_invocation)"
check "the next backup works" on_client systemctl start nestegg-backup.service
check "the check ran after the success" wait_new_notify "$before"
check "hook got the recovered event" test "$(count_events recovered)" = 1
check "desktop got the good news" bash -c "docker exec $client grep -qx 'arg=Backups work again' /tmp/notify-send.log"
check "good news is not critical" test "$(on_client grep -c -x 'arg=--urgency=critical' /tmp/notify-send.log)" = 1
on_client systemctl start nestegg-notify.service
check "nothing more is sent" bash -c "[[ $(count_events recovered) == 1 && $(count_events failing) == 1 && $(desktop_count) == 3 ]]"
check "status ok again" bash -c "docker exec -u alice $client nestegg status | grep -q 'Health:       ok'"

echo "== removal keeps credentials"
check "package removes cleanly" on_client bash -c 'DEBIAN_FRONTEND=noninteractive apt-get remove -y -q nestegg >/tmp/apt-rm.log 2>&1'
check "health check timer stopped by the removal" bash -c "! docker exec $client systemctl is-active --quiet nestegg-notify.timer"
check "removal remembers the health check timer" on_client grep -qx nestegg-notify.timer /var/lib/nestegg/enabled-units
check "key survives removal" on_client test -f /etc/nestegg/credentials/encryption-key.cred

result
