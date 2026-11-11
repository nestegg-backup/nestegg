#!/usr/bin/env bash
# Runs ONLY in the disposable live VM created by desktop-vm.sh.
set -Eeuo pipefail
trap 'status=$?; if (( status != 0 )); then echo "FAILED guest line $LINENO"; cat /run/key.log /run/key-copy.log /run/hotplug-service.log /run/udev-properties.log /run/udev-1.log 2>/dev/null || true; fi' EXIT
[[ -f /run/nestegg-disposable-vm ]] || { echo 'Disposable VM marker missing' >&2; exit 2; }
cp /mnt/fixture/nestegg /usr/bin/nestegg
cp /mnt/fixture/nestegg /usr/local/bin/nestegg
cp /mnt/fixture/nestegg-stick-pbs@.service /etc/systemd/system/
cp /mnt/fixture/nestegg-backup.service /etc/systemd/system/
cp /mnt/fixture/70-nestegg-stick.rules /etc/udev/rules.d/
mkdir -p /etc/nestegg/credentials /var/lib/nestegg/stick /srv/stick-test /run/nestegg
chmod 0700 /etc/nestegg/credentials
# The live root is an untrusted overlay for cache provenance. Supply only the
# disposable fixture cache on explicit root-owned RAM storage, as in desktop.sh.
mount -t tmpfs -o mode=0755 tmpfs /var/lib/nestegg/stick
cp /mnt/fixture/pbs.squashfs /var/lib/nestegg/stick/pbs.squashfs
sha256sum /var/lib/nestegg/stick/pbs.squashfs | cut -d' ' -f1 >/var/lib/nestegg/stick/pbs.squashfs.sha256
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
if nestegg stick registered --uuid 01234567-89ab-cdef-0123-456789abcdef >/dev/null; then exit 1; fi
python3 - <<'PY'
import json
s=json.load(open('/var/lib/nestegg/state.json'))
assert s['stick']['last_success']['verified']
assert s.get('last_success') is None
PY
# Exercise the actual hourly unit with its shipped privilege sandbox. Age
# only the disposable target's success so the timer performs a real backup.
python3 - <<'PYTIMER'
import json
p='/var/lib/nestegg/state.json'
s=json.load(open(p)); s['stick']['last_success']['finished_at']='2000-01-01T00:00:00Z'
with open(p,'w') as f: json.dump(s,f)
PYTIMER
systemctl start nestegg-backup.service
[[ $(systemctl show -p NoNewPrivileges --value nestegg-backup.service) == yes ]]
python3 - <<'PYTIMER'
import json,os
s=json.load(open('/var/lib/nestegg/state.json'))
r=s['stick']['last_run']
assert r['trigger']=='timer' and r['state']=='succeeded' and r['verified'],r
assert not os.path.exists('/var/lib/nestegg/current.json')
import subprocess
view=json.loads(subprocess.check_output(['nestegg','status','--json']))['targets']['stick']
assert view['plugged_in'] is True,view
assert view['total_bytes'] > view['available_bytes'] >= 0 and view['space_checked_at'],view
PYTIMER
printf 'VM-%s\n' TIMER-SANDBOX-PASS
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
connection.request('GET','/api/v1/config')
response=connection.getresponse()
assert response.status==200,response.read().decode()
import json
config=json.loads(response.read())
assert len(config['stick_data_uuids'])==1,config
assert config['limits']['stick_overdue_after_hours']['min']==1,config
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
if systemctl is-active --quiet "$unit"; then exit 1; fi
# Unlike loop detach (which is lazy while mounted), removing this owned
# virtual SCSI device makes the kernel revoke a disk in the middle of upload.
disk=$(lsblk -dnpo PATH,SERIAL | awk '$2 == "nestegg-test" {print $1}')
[[ -b $disk && $(lsblk -dnro SERIAL "$disk") == nestegg-test ]]
printf 'label: gpt\n,,L\n' | sfdisk "$disk" >/run/scsi-partition.log 2>&1
partprobe "$disk"; udevadm settle
part=${disk}1
mkfs.ext4 -q -m 0 "$part"
uuid=$(blkid -p -s UUID -o value "$part")
mount "$part" /run/prepare; mkdir /run/prepare/nestegg; umount /run/prepare
sed -i -E "s/data_uuids = .*/data_uuids = [\"$uuid\"]/; s/system = true/system = false/" /etc/nestegg/config.toml
sed -i '/system = false/a rate_limit = "64KiB"' /etc/nestegg/config.toml
nestegg stick key-copy --uuid "$uuid" --password-fd 7 7</run/password >/run/scsi-key-copy.log 2>&1
head -c 67108864 /dev/urandom >/srv/stick-test/random
python3 - <<'PYUNPLUG'
import json
p='/var/lib/nestegg/state.json'; s=json.load(open(p))
s['stick']['last_success']=None
with open(p,'w') as f: json.dump(s,f)
PYUNPLUG
# A deliberately stubborn fixture process remains in the device-bound cgroup
# after SIGTERM, proving systemd's final SIGKILL cleanup as well as graceful
# run-state recording. It cannot escape this disposable VM or its unit.
cat >>/usr/local/bin/stick-events <<'STUBBORN'
if [ "$NESTEGG_EVENT" = stick_start ] && [ -f /run/start-stubborn ]; then
    nohup /bin/bash -c 'trap "" TERM INT; echo $$ >/run/stubborn.pid; while :; do sleep 1; done' >/run/stubborn.log 2>&1 </dev/null &
fi
STUBBORN
touch /run/start-stubborn
unit=$(systemd-escape --template=nestegg-stick-pbs@.service "$uuid")
mkdir -p "/etc/systemd/system/$unit.d"
printf '[Service]\nTimeoutStopSec=10s\n' >"/etc/systemd/system/$unit.d/test.conf"
systemctl daemon-reload
systemctl start --no-block "$unit"
for _ in {1..240}; do
    if [[ -s /run/stubborn.pid && -f /var/lib/nestegg/current.json ]] && python3 - <<'PYUNPLUG'
import json,pathlib,sys
r=json.load(open('/var/lib/nestegg/current.json'))
log=pathlib.Path('/var/lib/nestegg/runs')/(r['id']+'.log')
sys.exit(0 if r['state']=='uploading' and log.exists() and 'Starting backup:' in log.read_text() else 1)
PYUNPLUG
    then break; fi
    sleep .5
done
[[ -s /run/stubborn.pid ]]
stubborn=$(cat /run/stubborn.pid)
kill -0 "$stubborn"
cp /var/lib/nestegg/current.json /run/unplug-current.json
# This is the synthetic disk identified by its QEMU serial above, never a
# system disk or a real device on the host.
echo 1 >"/sys/class/block/${disk##*/}/device/delete"
udevadm settle
for _ in {1..120}; do
    state=$(systemctl show -p ActiveState --value "$unit")
    [[ $state == inactive || $state == failed ]] && break
    sleep .5
done
journalctl -u "$unit" --no-pager >/run/unplug-unit.log
[[ $(systemctl show -p Result --value "$unit") == timeout ]]
if kill -0 "$stubborn" 2>/dev/null; then exit 1; fi
python3 - <<'PYUNPLUG'
import json,os
s=json.load(open('/var/lib/nestegg/state.json'))
r=s['stick']['last_run']; requested=json.load(open('/run/unplug-current.json'))
assert r['id']==requested['id'],(r,requested)
assert r['state']=='stick_not_plugged_in' and r['error']['kind']=='stick_not_plugged_in',r
assert s['stick']['consecutive_failures']==0,s['stick']
assert not os.path.exists('/var/lib/nestegg/current.json')
assert not os.path.exists('/run/nestegg/run.pid')
PYUNPLUG
if pgrep -f '^/usr/lib/.*/proxmox-backup-(proxy|api)' >/dev/null; then exit 1; fi
if find /run/nestegg/stick/pbs -maxdepth 1 -type d -name 'pbs-*' | grep -q .; then exit 1; fi
printf 'VM-%s\n' UNPLUG-TERM-KILL-PASS
printf 'VM-%s\n' HOTPLUG-PASS
