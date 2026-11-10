#!/usr/bin/env bash
# Package upgrade test: install an OLD nestegg .deb into a container running
# systemd, set it up against a disposable PBS and back up through the timer
# unit, then install the NEW .deb over it. Checks that services, timer,
# configuration, credentials and run history survive the upgrade, that the
# new version backs up and restores, what `apt remove` and `apt purge` keep
# (see packaging/debian/postrm) and that reinstalling enables the units that
# were enabled before removal (prerm, postinst).
#
# The NEW package must have a higher version than the OLD one, otherwise apt
# does not upgrade. Build the old one with a lower version, for example:
#   cargo deb -p nestegg --no-build -o old/ --deb-version "0.1.0~base"
#
# Needs the same Docker privileges as tests/e2e/systemd.sh (host cgroup
# namespace). Only run this on a disposable test machine.
#
# Usage: tests/e2e/upgrade.sh <old nestegg_*.deb> <new nestegg_*.deb>
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
old_deb="${1:?old .deb}"
new_deb="${2:?new .deb}"
net="nestegg-e2e-$$"
pbs="nestegg-e2e-pbs-$$"
client="nestegg-e2e-up-$$"

cleanup() {
  [[ -n "${KEEP:-}" ]] && { echo "(KEEP set: containers $pbs $client left running)"; return; }
  docker rm -f "$pbs" "$client" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
}
trap cleanup EXIT

for f in "$old_deb" "$new_deb"; do
  [[ -f "$f" ]] || { echo "package not found: $f"; exit 2; }
done
old_deb="$(realpath "$old_deb")"
new_deb="$(realpath "$new_deb")"
old_version="$(dpkg-deb -f "$old_deb" Version)"
new_version="$(dpkg-deb -f "$new_deb" Version)"
dpkg --compare-versions "$new_version" gt "$old_version" || {
  echo "new version $new_version is not higher than old version $old_version (see --deb-version)"
  exit 2
}
# shellcheck source=lib.sh
source "$here/lib.sh"
apt_in_client() { on_client bash -c 'DEBIAN_FRONTEND=noninteractive apt-get "$@" >>/tmp/upgrade-apt.log 2>&1' apt "$@"; }
installed_version() { on_client dpkg-query -W -f "\${Version}" nestegg 2>/dev/null; }
# Checksums of configuration and credentials (must not change).
config_sums() { on_client bash -c 'cd /etc/nestegg && find . -type f | sort | xargs sha256sum'; }
api_get() { on_client curl -sf --unix-socket /run/nestegg/api.sock "http://localhost$1"; }
succeeded_runs() { on_client nestegg history --json | grep -c '"state": "succeeded"' || true; }
daemon_pid() { on_client systemctl show -p MainPID --value nestegg.service; }
timer_next() { on_client systemctl show -p NextElapseUSecRealtime --value nestegg-backup.timer; }

echo "== upgrade $old_version -> $new_version"
echo "== building images"
build_pbs_image
build_systemd_images

start_pbs

echo "== starting systemd client and installing the old package"
start_systemd_client -v "$old_deb:/tmp/nestegg-old.deb:ro" -v "$new_deb:/tmp/nestegg-new.deb:ro"
on_client bash -c 'useradd -m alice && echo "hello from before the upgrade" > /home/alice/notes.txt && chown -R alice: /home/alice'
check "old package installs" apt_in_client install -y -q /tmp/nestegg-old.deb
check "old version installed" test "$(installed_version)" == "$old_version"

echo "== setup with the old version"
# --no-system: the container's overlay root cannot be backed up as
# system.pxar; system mode would make every backup "incomplete".
on_client nestegg init --server pbs.e2e.test --datastore store1 --namespace desktops \
  --auth-id 'backup@pbs!desktop' --fingerprint "$fp" --backup-id up-desktop --no-system >/dev/null
docker exec -i "$client" nestegg credentials set-password <<<"$token_secret" >/dev/null
on_client nestegg key create >/dev/null
on_client nestegg key export /root/key.json >/dev/null
# The old package may still confirm without proof (no --file option); call
# each package with the syntax it understands.
if on_client nestegg key confirm-exported --help | grep -q -- '--file'; then
  on_client nestegg key confirm-exported --file /root/key.json --accept-local-copy >/dev/null
else
  on_client nestegg key confirm-exported >/dev/null
fi
check "services enable" on_client systemctl enable --now nestegg.service nestegg-backup.timer
# The old package (merge base) may predate the health check; then the upgrade
# has to switch its timer on, otherwise it was enabled by the old package.
if on_client systemctl cat nestegg-notify.timer >/dev/null 2>&1; then
  echo "  (the old package already has the health check timer)"
else
  ok "old package has no health check timer"
fi
check "timer unit run succeeds (old)" on_client systemctl start nestegg-backup.service
check "unit result success (old)" test "$(on_client systemctl show -p Result --value nestegg-backup.service)" == success
old_run="$(on_client nestegg history --json | sed -n 's/.*"id": "\([^"]*\)".*/\1/p' | head -1)"
old_snapshot="$(on_client nestegg history --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
check "old run recorded" test -n "$old_run"
check "old run has a snapshot" test -n "$old_snapshot"
sums_before="$(config_sums)"
run_record_before="$(on_client sha256sum "/var/lib/nestegg/runs/$old_run.json")"
pid_before="$(daemon_pid)"

echo "== lock files a development version left 0644"
# Development versions created their lock files group-readable (0644), which
# let a group member hold a shared lock and block the daemon or a backup
# (decision 0022). There was never a release with such files, so the upgrade
# only tightens them to 0600 in place; it never replaces or removes them. The
# run above made run.lock and .current.lock; make all four exist as 0644.
on_client usermod -aG nestegg alice
lock_files="/run/nestegg/run.lock /var/lib/nestegg/.current.lock /var/lib/nestegg/.notify.lock /var/lib/nestegg/.notify-settings.lock"
for lf in $lock_files; do
  on_client sh -c "mkdir -p \"\$(dirname '$lf')\"; [ -e '$lf' ] || : > '$lf'; chgrp nestegg '$lf' 2>/dev/null || true; chmod 0644 '$lf'"
done
lock_inodes() { for lf in $lock_files; do on_client stat -c '%i' "$lf"; done; }
inodes_before="$(lock_inodes)"
check "a member can open the old lock files before the upgrade" docker exec -u alice "$client" sh -c "for f in $lock_files; do exec 9<\"\$f\" || exit 1; done"

echo "== upgrade to the new package"
check "new package installs over the old one" apt_in_client install -y -q /tmp/nestegg-new.deb
# Straight after the installation, before this script runs any nestegg command:
# the two lock files of the notification check were tightened by the package's
# postinst (the new binary also tightens a lock file when it opens one; the
# daemon restarted by the upgrade does not open these two by itself).
for lf in /var/lib/nestegg/.notify.lock /var/lib/nestegg/.notify-settings.lock; do
  check "$(basename "$lf") is 0600 right after the installation" bash -c "[[ \$(docker exec $client stat -c '%a' '$lf' 2>/dev/null) == 600 ]]"
done
check "new version installed" test "$(installed_version)" == "$new_version"
check "daemon still enabled" on_client systemctl is-enabled --quiet nestegg.service
check "timer still enabled" on_client systemctl is-enabled --quiet nestegg-backup.timer
check "daemon running" on_client systemctl is-active --quiet nestegg.service
check "timer active" on_client systemctl is-active --quiet nestegg-backup.timer
check "health check timer enabled after the upgrade" on_client systemctl is-enabled --quiet nestegg-notify.timer
check "health check timer running after the upgrade" on_client systemctl is-active --quiet nestegg-notify.timer
check "health check unit installed" on_client systemctl cat nestegg-notify.service
check "health check does nothing to a healthy setup" bash -c "docker exec $client nestegg notify | grep -q 'everything is fine'"
next="$(timer_next)"
check "timer still scheduled ($next)" bash -c "[[ -n '$next' && '$next' != infinity ]]"
pid_after="$(daemon_pid)"
check "daemon restarted by the upgrade" bash -c "[[ '$pid_after' != '$pid_before' && '$pid_after' != 0 ]]"
check "daemon runs the new binary" bash -c "[[ \$(docker exec $client readlink /proc/$pid_after/exe) == /usr/bin/nestegg ]]"
check "configuration and credentials unchanged" test "$(config_sums)" == "$sums_before"
# The old package created config.toml world-readable (0644 root:root); the
# upgrade's postinst corrects it to group-readable only (decision 0022).
check "config.toml is 0640 root:nestegg after the upgrade" bash -c "[[ \$(docker exec $client stat -c '%a %U:%G' /etc/nestegg/config.toml) == '640 root:nestegg' ]]"
check "password still stored with systemd-creds" on_client test -f /etc/nestegg/credentials/password.cred
check "earlier run record unchanged" test "$(on_client sha256sum "/var/lib/nestegg/runs/$old_run.json")" == "$run_record_before"
check "earlier run in nestegg history" bash -c "docker exec $client nestegg history --json | grep -q '\"id\": \"$old_run\"'"
check "earlier run is the last success in nestegg status" bash -c "docker exec $client nestegg status --json | grep -q '\"id\": \"$old_run\"'"
check "earlier run listed by the API" bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock http://localhost/api/v1/runs | grep -q '\"$old_run\"'"
check "check passes after the upgrade" on_client nestegg check
check "a key confirmed before the upgrade stays confirmed" bash -c "docker exec $client nestegg key show | grep -Eq 'exported: +yes'"
check "the exported key file still verifies after the upgrade" bash -c "docker exec $client nestegg key verify /root/key.json | grep -q 'opens the newest backup: yes'"

echo "== after the upgrade all lock files are 0600, the same files, closed to members"
for lf in $lock_files; do
  b="$(basename "$lf")"
  check "$b is 0600 after the upgrade" bash -c "[[ \$(docker exec $client stat -c '%a' '$lf' 2>/dev/null) == 600 ]]"
  check "a member can no longer open $b" bash -c "! docker exec -u alice $client sh -c \"exec 9<'$lf'\" 2>/dev/null"
done
check "the lock files were tightened in place, not replaced" test "$(lock_inodes)" == "$inodes_before"
check "the daemon answers after the upgrade" api_get /api/v1/status

echo "== timer backup and restore with the new version"
# The timer only backs up when the last success is older than the interval:
# pretend the earlier backup finished long ago.
on_client sed -Ei 's/("finished_at": ?")[^"]*"/\12000-01-01T00:00:00Z"/g' /var/lib/nestegg/state.json
check "timer unit run succeeds (new)" on_client systemctl start nestegg-backup.service
check "unit result success (new)" test "$(on_client systemctl show -p Result --value nestegg-backup.service)" == success
check "new timer run recorded" test "$(succeeded_runs)" -ge 2
check "new run is the last success" bash -c "s=\$(docker exec $client nestegg status --json) && grep -q '\"last_success\"' <<<\"\$s\" && ! grep -q '\"id\": \"$old_run\"' <<<\"\$s\""
check "status ok via the API" bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock http://localhost/api/v1/status | grep -q '\"health\":\"ok\"'"
on_client nestegg restore "$old_snapshot" home.pxar /root/restore-old >/dev/null 2>&1 || true
check "restore of the pre-upgrade snapshot" on_client cmp /home/alice/notes.txt /root/restore-old/alice/notes.txt
new_snapshot="$(on_client nestegg history --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
check "post-upgrade snapshot is a new one" test -n "$new_snapshot" -a "$new_snapshot" != "$old_snapshot"
on_client nestegg restore "$new_snapshot" home.pxar /root/restore-new >/dev/null 2>&1 || true
check "restore of the post-upgrade snapshot" on_client cmp /home/alice/notes.txt /root/restore-new/alice/notes.txt

echo "== a user browses snapshots of both versions (decision 0016)"
on_client usermod -aG nestegg alice
user_browse_names() {
  docker exec -u alice "$client" curl -s --unix-socket /run/nestegg/api.sock \
    "http://localhost/api/v1/snapshots/${1##*/}/browse?archive=home.pxar&path=/" \
    | grep -o '"name":"[^"]*"' | sort -u | tr -d '\n'
}
check "pre-upgrade snapshot has no metadata blob" bash -c "! docker exec -e PBS_PASSWORD='$admin_pw' -e PBS_FINGERPRINT='$fp' $pbs proxmox-backup-client snapshot files '$old_snapshot' --ns desktops --repository admin@pbs@127.0.0.1:store1 | grep -q nestegg-meta"
check "post-upgrade snapshot has the metadata blob" bash -c "docker exec -e PBS_PASSWORD='$admin_pw' -e PBS_FINGERPRINT='$fp' $pbs proxmox-backup-client snapshot files '$new_snapshot' --ns desktops --repository admin@pbs@127.0.0.1:store1 | grep -q nestegg-meta.conf.blob"
check "user browses the pre-upgrade snapshot (local run record)" test "$(user_browse_names "$old_snapshot")" == '"name":"alice"'
check "user browses the post-upgrade snapshot (metadata blob)" test "$(user_browse_names "$new_snapshot")" == '"name":"alice"'

echo "== apt remove keeps configuration, credentials and state"
sums_before="$(config_sums)"
check "package removes cleanly" apt_in_client remove -y -q nestegg
check "package state is config-files" test "$(on_client dpkg-query -W -f "\${db:Status-Status}" nestegg)" == config-files
check "binary removed" bash -c "! docker exec $client test -e /usr/bin/nestegg"
check "units removed" bash -c "! docker exec $client test -e /usr/lib/systemd/system/nestegg.service"
check "daemon stopped" bash -c "! docker exec $client systemctl is-active --quiet nestegg.service"
check "timer stopped" bash -c "! docker exec $client systemctl is-active --quiet nestegg-backup.timer"
check "health check timer stopped" bash -c "! docker exec $client systemctl is-active --quiet nestegg-notify.timer"
check "configuration and credentials kept after remove" test "$(config_sums)" == "$sums_before"
check "run history kept after remove" on_client test -f "/var/lib/nestegg/runs/$old_run.json"

echo "== apt purge keeps them too (postrm: deleting the key would orphan backups)"
check "package purges cleanly" apt_in_client purge -y -q nestegg
check "package fully gone" bash -c "! docker exec $client dpkg -s nestegg >/dev/null 2>&1"
check "configuration and credentials kept after purge" test "$(config_sums)" == "$sums_before"
check "state kept after purge" on_client test -f /var/lib/nestegg/state.json
check "run history kept after purge" on_client test -f "/var/lib/nestegg/runs/$old_run.json"

echo "== reinstall after purge picks up the kept setup"
check "new package reinstalls" apt_in_client install -y -q /tmp/nestegg-new.deb
# prerm records the enabled units before disabling them, postinst enables
# them again.
check "daemon enabled again after reinstall" on_client systemctl is-enabled --quiet nestegg.service
check "timer enabled again after reinstall" on_client systemctl is-enabled --quiet nestegg-backup.timer
check "daemon running again after reinstall" on_client systemctl is-active --quiet nestegg.service
check "timer running again after reinstall" on_client systemctl is-active --quiet nestegg-backup.timer
check "health check timer enabled again after reinstall" on_client systemctl is-enabled --quiet nestegg-notify.timer
check "health check timer running again after reinstall" on_client systemctl is-active --quiet nestegg-notify.timer
check "enable marker consumed" absent_in /var/lib/nestegg enabled-units
check "check passes with the kept setup" on_client nestegg check
check "history still lists the earlier run" bash -c "docker exec $client nestegg history --json | grep -q '\"id\": \"$old_run\"'"

echo "== a unit disabled before removal stays disabled"
check "timer disabled by the user" on_client systemctl disable --now nestegg-backup.timer
check "package removes again" apt_in_client remove -y -q nestegg
check "marker lists the daemon and the health check, not the backup timer" test "$(on_client cat /var/lib/nestegg/enabled-units)" == $'nestegg.service\nnestegg-notify.timer'
check "package reinstalls again" apt_in_client install -y -q /tmp/nestegg-new.deb
check "timer still disabled" test "$(on_client systemctl is-enabled nestegg-backup.timer || true)" == disabled
check "daemon enabled again" test "$(on_client systemctl is-enabled nestegg.service || true)" == enabled
check "health check timer enabled again" test "$(on_client systemctl is-enabled nestegg-notify.timer || true)" == enabled

echo "== a health check timer switched off by the user stays off on an upgrade"
check "health check timer disabled by the user" on_client systemctl disable --now nestegg-notify.timer
check "same package reinstalls over itself (health check timer)" apt_in_client install -y -q --reinstall /tmp/nestegg-new.deb
check "health check timer still disabled" test "$(on_client systemctl is-enabled nestegg-notify.timer || true)" == disabled

echo "== a stale marker is ignored on an upgrade"
# A marker can be left behind, for example when an older package version
# without this logic was installed after the removal. The next upgrade must
# neither re-enable units from it nor skip the daemon restart (preinst).
on_client sh -c 'printf "nestegg.service\nnestegg-backup.timer\n" > /var/lib/nestegg/enabled-units'
pid_before="$(daemon_pid)"
check "same package reinstalls over itself" apt_in_client install -y -q --reinstall /tmp/nestegg-new.deb
check "timer not re-enabled by a stale marker" test "$(on_client systemctl is-enabled nestegg-backup.timer || true)" == disabled
check "stale marker removed" absent_in /var/lib/nestegg enabled-units
check "daemon restarted on upgrade" test "$(daemon_pid)" != "$pid_before" -a "$(daemon_pid)" != 0

[[ $fail -eq 0 ]] || { echo "-- apt log:"; on_client tail -40 /tmp/upgrade-apt.log | sed 's/^/    /'; }
result
