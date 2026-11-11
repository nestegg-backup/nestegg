#!/usr/bin/env bash
# Runs ONLY in the disposable live VM created by desktop-vm.sh.
set -Eeuo pipefail
trap 'echo "FAILED guest line $LINENO"; cat /run/key.log /run/key-copy.log /run/hotplug-service.log /run/udev-properties.log /run/udev-1.log 2>/dev/null || true' ERR
[[ -f /run/nestegg-disposable-vm ]] || { echo 'Disposable VM marker missing' >&2; exit 2; }
cp /mnt/fixture/nestegg /usr/bin/nestegg
cp /mnt/fixture/nestegg /usr/local/bin/nestegg
cp /mnt/fixture/nestegg-stick-pbs@.service /etc/systemd/system/
cp /mnt/fixture/70-nestegg-stick.rules /etc/udev/rules.d/
mkdir -p /etc/nestegg/credentials /var/lib/nestegg/stick /srv/stick-test /run/nestegg
chmod 0700 /etc/nestegg/credentials
cp /mnt/fixture/pbs.squashfs /var/lib/nestegg/stick/pbs.squashfs
cp /mnt/fixture/client /usr/local/bin/proxmox-backup-client
systemctl daemon-reload
udevadm control --reload-rules
truncate -s 3G /run/stick-test.img
loop=$(losetup --find --show --partscan /run/stick-test.img)
printf 'label: gpt\n,64M,U\n,,L\n' | sfdisk "$loop" >/run/partition.log 2>&1
partprobe "$loop"; udevadm settle
mkfs.vfat -F32 "${loop}p1" >/dev/null
mkfs.ext4 -q -m 0 "${loop}p2"
uuid=$(blkid -p -s UUID -o value "${loop}p2")
mkdir /run/prepare
mount -o nodev,nosuid,noexec "${loop}p2" /run/prepare
mkdir /run/prepare/nestegg
umount /run/prepare
cat >/etc/nestegg/config.toml <<EOF
backup_id = "desktop"
[stick]
data_uuids = ["$uuid"]
[backup]
sources = [{archive="files",path="/srv/stick-test"}]
system = false
inventory = true
[notify]
desktop = false
command = ["/usr/local/bin/stick-events"]
EOF
cat >/usr/local/bin/stick-events <<'EOF'
#!/bin/sh
printf '%s\n' "$NESTEGG_EVENT" >>/run/stick-events
EOF
chmod 0700 /usr/local/bin/stick-events
printf 'synthetic live-VM desktop file\n' >/srv/stick-test/report.txt
export NESTEGG_SYSTEMD_CREDS=/no-systemd-creds
nestegg key create --allow-plain >/run/key.log 2>&1
openssl rand -base64 30 >/run/password
nestegg stick key-copy --uuid "$uuid" --password-fd 7 7</run/password >/run/key-copy.log 2>&1
# Reconnect the owned image after registration. systemd acts on SYSTEMD_WANTS
# when a device becomes active, not when an already-active device is retagged.
losetup -d "$loop"
udevadm settle
loop=$(losetup --find --show --partscan /run/stick-test.img)
udevadm settle
unit=$(systemd-escape --template=nestegg-stick-pbs@.service "$uuid")
udevadm info --query=property --name "${loop}p2" >/run/udev-properties.log
systemctl show "$unit" >/run/hotplug-service.log
journalctl -u "$unit" --no-pager >>/run/hotplug-service.log
for _ in {1..240}; do
    if [[ -f /run/stick-events ]] && grep -q '^stick_safe_to_unplug$' /run/stick-events; then break; fi
    if systemctl is-failed --quiet "$unit"; then journalctl -u "$unit" --no-pager >>/run/hotplug-service.log; exit 1; fi
    sleep .5
done
journalctl -u "$unit" --no-pager >>/run/hotplug-service.log
grep -q '^stick_start$' /run/stick-events
grep -q '^stick_safe_to_unplug$' /run/stick-events
[[ $(systemctl show -p Result --value "$unit") == success ]]
# Verify UDisks ignore on both the data partition and the ESP, independent
# of event ordering; unknown block devices must not pass the predicate.
for number in 1 2; do
    env PATH=/usr/bin /usr/bin/nestegg stick registered --parent "${loop}p$number"
    udevadm info --query=property --name "${loop}p$number" >"/run/udev-$number.log" 2>&1
    grep -q 'UDISKS_IGNORE=1' "/run/udev-$number.log"
done
! nestegg stick registered --uuid 01234567-89ab-cdef-0123-456789abcdef >/dev/null
python3 - <<'PY'
import json
s=json.load(open('/var/lib/nestegg/state.json'))
assert s['stick']['last_success']['verified']
assert s.get('last_success') is None
PY
# The live overlay cannot be recreated by the bare-metal restore. Requesting
# system mode must therefore preserve layout.json and report incomplete,
# rather than silently claim a complete system backup.
sed -i 's/system = false/system = true/' /etc/nestegg/config.toml
nestegg daemon >/run/daemon.log 2>&1 &
daemon_pid=$!
for _ in {1..50}; do [[ -S /run/nestegg/api.sock ]] && break; sleep .1; done
python3 - <<'PY'
import http.client, socket
class LocalApi(http.client.HTTPConnection):
    def connect(self):
        self.sock=socket.socket(socket.AF_UNIX)
        self.sock.connect('/run/nestegg/api.sock')
connection=LocalApi('localhost', timeout=10)
connection.request('POST','/api/v1/targets/stick/runs')
response=connection.getresponse()
assert response.status==202, response.read().decode()
with open('/run/api-run.json','wb') as output: output.write(response.read())
connection.close()
PY
for _ in {1..240}; do
    if python3 - <<'PY'
import json,sys
requested=json.load(open('/run/api-run.json'))
s=json.load(open('/var/lib/nestegg/state.json'))
last=s['stick'].get('last_success') or {}
sys.exit(0 if last.get('id')==requested['id'] and requested['target']=='stick' else 1)
PY
    then break; fi
    sleep .5
done
kill "$daemon_pid"; wait "$daemon_pid" || true
test -s /var/lib/nestegg/inventory/layout.json
python3 - <<'PY'
import json
s=json.load(open('/var/lib/nestegg/state.json'))
assert s['stick']['last_success']['verified']
assert s['stick']['last_success']['incomplete']
PY
# Actual removal of the owned loop must deactivate its bound device unit.
losetup -d "$loop"
udevadm settle
! systemctl is-active --quiet "$unit"
printf 'VM-%s\n' HOTPLUG-PASS
