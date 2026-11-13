#!/usr/bin/env bash
# End-to-end test: real Proxmox Backup Server + real proxmox-backup-client in
# disposable containers. Nothing outside these containers is touched.
#
# Usage: tests/e2e/run.sh [path/to/nestegg-binary] [client-base-image]
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="${1:-$repo/target/debug/nestegg}"
base="${2:-ubuntu:24.04}"
net="nestegg-e2e-$$"
pbs="nestegg-e2e-pbs-$$"
client="nestegg-e2e-client-$$"
wol="nestegg-e2e-wol-$$"
client_image="nestegg-e2e-client:$(echo "$base" | tr ':/' '--')"

cleanup() {
  [[ -n "${KEEP:-}" ]] && { echo "(KEEP set: containers $pbs $client left running)"; return; }
  docker rm -f "$pbs" "$client" "$wol" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
}
trap cleanup EXIT
[[ -x "$bin" ]] || { echo "nestegg binary not found: $bin (run cargo build first)"; exit 2; }
bin="$(realpath "$bin")"
# shellcheck source=lib.sh
source "$here/lib.sh"

echo "== building images (client base: $base)"
build_pbs_image
DOCKER_BUILDKIT=0 docker build -q --build-arg BASE="$base" -t "$client_image" -f "$here/client.Dockerfile" "$here" >/dev/null

start_pbs

echo "== starting client ($base)"
docker run -d --name "$client" --network "$net" --tmpfs /mnt/small:size=2m,mode=1777 \
  -v "$bin:/usr/local/bin/nestegg:ro" "$client_image" sleep infinity >/dev/null
on_client bash -c '
  set -e
  useradd -m alice
  useradd -m bob
  mkdir -p /home/bob/alice && echo "bob only" > /home/bob/alice/secret.txt
  chown -R bob:bob /home/bob && chmod 700 /home/bob
  mkdir -p /home/alice/Documents /home/alice/.cache/big /home/alice/project/node_modules/x
  echo "quarterly report" > /home/alice/Documents/report.txt
  echo "user data in a nested directory" > /home/alice/Documents/.pxarexclude-cli
  echo "brackets" > "/home/alice/Documents/[ab].txt"
  echo "plain a" > /home/alice/Documents/a.txt
  head -c 3000000 /dev/urandom > /home/alice/Documents/photo.bin
  ln -s Documents/report.txt /home/alice/link-to-report
  echo "cache" > /home/alice/.cache/big/blob
  echo "dep" > /home/alice/project/node_modules/x/index.js
  chmod 640 /home/alice/Documents/report.txt
  cp /bin/true /home/alice/Documents/tool-with-capability
  chown -R alice:alice /home/alice
  # Many sockets: tar reports each as skipped; stderr exceeds a pipe buffer.
  mkdir -p /home/alice/sockets
  perl -MIO::Socket::UNIX - <<"PERL"
for (1..3000) {
  my $s = IO::Socket::UNIX->new(Type => SOCK_STREAM(), Local => "/home/alice/sockets/socket-with-a-long-name-$_", Listen => 1) or die $!;
  close $s;
}
PERL
  chown -R alice: /home/alice/sockets
  mkdir -p /home/alice/Archive/frozen && echo old > /home/alice/Archive/frozen/note.txt
  chmod 0555 /home/alice/Archive/frozen
  truncate -s 64M /home/alice/Documents/sparse.img && chown alice: /home/alice/Documents/sparse.img
  # after chown: chown clears file capabilities
  setcap cap_net_raw+ep /home/alice/Documents/tool-with-capability
'

echo "== setup with the nestegg CLI"
# --no-system: the container's overlay root cannot be backed up as
# system.pxar, so system mode would make every backup "incomplete" (checked
# separately below).
on_client nestegg init --server pbs.e2e.test --datastore store1 --namespace desktops \
  --auth-id 'backup@pbs!desktop' --fingerprint "$fp" --backup-id e2e-desktop --no-system >/dev/null
# A source below the caller's home: its archive root is Documents, not /home.
on_client bash -c 'printf "\n[[backup.sources]]\narchive = \"docs\"\npath = \"/home/alice/Documents\"\n" >> /etc/nestegg/config.toml'
docker exec -i "$client" nestegg credentials set-password --allow-plain <<<"$token_secret" >/dev/null
on_client nestegg key create --allow-plain >/dev/null
check "backup refused while key not exported" \
  fails_with 1 'encryption key has not been exported yet' on_client nestegg backup
export_out="$(on_client nestegg key export /root/nestegg-key.json 2>&1)"
check "export reads the file back" grep -q 'read back' <<<"$export_out"
check "export on the internal disk warns" grep -q 'lost together with this computer' <<<"$export_out"
check "export alone does not unlock backups" \
  fails_with 1 'encryption key has not been exported yet' on_client nestegg backup
check "confirm without proof is refused" \
  fails_with 1 'proof needed' on_client nestegg key confirm-exported
# A real second key: a different file must never confirm the stored key.
on_client proxmox-backup-client key create --kdf none /root/other-key.json >/dev/null
check "confirm with the file of another key is refused" \
  fails_with 1 'does not contain the key' \
  on_client nestegg key confirm-exported --file /root/other-key.json --accept-local-copy
check "confirm with a file that is no key is refused" \
  fails_with 1 'no key found' on_client nestegg key confirm-exported --file /etc/hostname --accept-local-copy
check "right file on the internal disk needs --accept-local-copy" \
  fails_with 1 'accept-local-copy' on_client nestegg key confirm-exported --file /root/nestegg-key.json
check "paper key to a terminal is refused" \
  fails_with 1 'not a copy of the key' docker exec -t "$client" nestegg key paperkey
paper_out="$(on_client nestegg key paperkey --output /root/paper.txt)"
on_client nestegg key paperkey --format html --output /root/paper.html >/dev/null
check "paper key contains a QR code" \
  bash -c "docker exec $client nestegg key paperkey | grep -q 'BEGIN PROXMOX BACKUP KEY\|-----BEGIN'"
paper_code="$(on_client sed -n 's/^Check code: *//p' /root/paper.txt)"
check "paper key header has a check code" bash -c "[[ '$paper_code' =~ ^[0-9A-F]{4}-[0-9A-F]{4}$ ]]"
check "paper key header names the server and datastore" bash -c "docker exec $client head -20 /root/paper.txt | grep -q 'pbs.e2e.test:8007' && docker exec $client head -20 /root/paper.txt | grep -q store1"
check "paper key keeps the client's key block" bash -c "docker exec $client grep -q -- '-----BEGIN PROXMOX BACKUP KEY-----' /root/paper.txt && docker exec $client grep -q -- '-----END PROXMOX BACKUP KEY-----' /root/paper.html"
check "the check code is not shown when the file is written" bash -c "! grep -q '$paper_code' <<<'$paper_out'"
check "paper key is not enough by itself" \
  fails_with 1 'encryption key has not been exported yet' on_client nestegg backup
check "wrong paper code is refused, statement or not" \
  fails_with 1 'does not belong to the key' on_client nestegg key confirm-exported --paper-code 0000-0000 --printed
check "malformed paper code is refused" \
  fails_with 1 'not a check code' on_client nestegg key confirm-exported --paper-code nonsense --printed
# The shortcut: write the paper key to a file, read the code off the file.
# No sheet of paper exists. nestegg remembers the file it wrote.
check "shortcut: the code read from the file on this computer is refused" \
  fails_with 1 'paper key file /root/paper\.' on_client nestegg key confirm-exported --paper-code "$paper_code" --printed
check "shortcut: the message says what to do" \
  fails_with 1 'delete the file or move it to a USB stick' on_client nestegg key confirm-exported --paper-code "$paper_code" --printed
check "shortcut: the message names the way out for a file that cannot be deleted" \
  fails_with 1 'confirm-exported --file /root/paper\.(txt|html) --accept-local-copy' \
  on_client nestegg key confirm-exported --paper-code "$paper_code" --printed
# Where standard output goes is found out before the key is written.
check "paper key to /dev/null is written and nothing is remembered" \
  on_client bash -c 'nestegg key paperkey >/dev/null'
# Recorded as exported only after the whole output was written: a reader that
# is gone (a printer that died, `| true`) is an error, a reader that takes it
# all is not.
check "paper key into a pipe whose reader is gone fails and records nothing" \
  fails_with 1 'nothing was recorded' on_client bash -c 'set -o pipefail; nestegg key paperkey | true'
check "paper key into a pipe that is read completely works" \
  on_client bash -c 'set -o pipefail; nestegg key paperkey | cat >/dev/null'
check "paper key redirected to a file with a name that is not UTF-8 is refused" \
  fails_with 1 'not valid UTF-8' on_client bash -c 'nestegg key paperkey > "$(printf "/root/odd-\377.txt")"'
check "... and the key was not written into that file" \
  on_client bash -c 'test -f "$(printf "/root/odd-\377.txt")" && ! test -s "$(printf "/root/odd-\377.txt")"'
on_client bash -c 'rm -f "$(printf "/root/odd-\377.txt")"'
# After printing, the files are deleted or moved away.
on_client bash -c 'mkdir -p /root/printed && mv /root/paper.txt /root/paper.html /root/printed/'
check "without the statement the right code is not enough" \
  fails_with 1 'State that with --printed' on_client nestegg key confirm-exported --paper-code "$paper_code"
check "nothing was recorded by the failed attempts" \
  fails_with 1 'encryption key has not been exported yet' on_client nestegg backup
check "paper code of the printed key with the statement confirms" on_client nestegg key confirm-exported --paper-code "$paper_code" --printed
check "config contains no secret" exits_with 1 on_client grep -qF "$token_secret" /etc/nestegg/config.toml
on_client nestegg check || true
# This container has no systemd, so the timer check always fails; with a
# stand-in `systemctl` that says "enabled" the exit code is the real verdict.
check "nestegg check passes when everything is set up (exit code 0)" \
  docker exec -e NESTEGG_SYSTEMCTL=/bin/true "$client" nestegg check

echo "== first backup"
check "backup succeeds" on_client nestegg backup
check "status is ok" bash -c "docker exec $client nestegg status --json | grep -q '\"health\": \"ok\"'"
snaps="$(on_client nestegg snapshots --json)"
check "one snapshot listed" bash -c "[[ \$(grep -c '\"snapshot\"' <<<'$snaps') == 1 ]]"
check "snapshot is encrypted" grep -q '"encrypted": true' <<<"$snaps"
check "snapshot has nestegg notes" grep -q '"comment": "nestegg' <<<"$snaps"
check "uploaded bytes recorded" bash -c "docker exec $client nestegg status --json | grep -Eq '\"uploaded_bytes\": [1-9]'"
check "archive sizes recorded" bash -c "docker exec $client nestegg status --json | grep -Eq '\"size_bytes\": [1-9]'"
check "secret not in run logs" exits_with 1 on_client grep -rqF "$token_secret" /var/lib/nestegg
snapshot="$(sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' <<<"$snaps" | head -1)"
admin_list="$(docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$pbs" \
  proxmox-backup-client snapshot list --ns desktops --repository admin@pbs@127.0.0.1:store1 --output-format json)"
check "PBS stores archives encrypted" grep -q '"crypt-mode":"encrypt"' <<<"$admin_list"
check "inventory archive uploaded" grep -q 'inventory.pxar' <<<"$admin_list"

echo "== restore"
on_client nestegg restore "$snapshot" home.pxar /root/restore-home >/dev/null 2>&1
check "CLI whole restore omits the synthetic exclude record" absent_in /root/restore-home .pxarexclude-cli
check "CLI keeps a nested file with the same name" on_client cmp /home/alice/Documents/.pxarexclude-cli /root/restore-home/alice/Documents/.pxarexclude-cli
check "restored file content matches" on_client cmp /home/alice/Documents/photo.bin /root/restore-home/alice/Documents/photo.bin
check "symlink restored" bash -c "[[ \$(docker exec $client readlink /root/restore-home/alice/link-to-report) == Documents/report.txt ]]"
check "owner and mode restored" bash -c "[[ \$(docker exec $client stat -c '%U %a' /root/restore-home/alice/Documents/report.txt) == 'alice 640' ]]"
check "excluded cache not in backup" absent_in /root/restore-home/alice .cache/big/blob
check "excluded node_modules not in backup" absent_in /root/restore-home/alice/project node_modules
check "restore refuses existing target" \
  fails_with 1 'restore-home already exists; restores only go into a new directory' \
  on_client nestegg restore "$snapshot" home.pxar /root/restore-home
on_client nestegg restore "$snapshot" home.pxar /root/restore-docs --pattern 'alice/Documents/report.txt' >/dev/null 2>&1 || true
check "pattern restore limits files" bash -c "docker exec $client test -e /root/restore-docs/alice/Documents/report.txt && ! docker exec $client test -e /root/restore-docs/alice/Documents/photo.bin"
check "restore with plain client and exported key" docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$client" \
  bash -c "proxmox-backup-client restore '$snapshot' etc.pxar /root/restore-etc --ns desktops --keyfile /root/nestegg-key.json --repository admin@pbs@pbs.e2e.test:store1 >/dev/null 2>&1 && test -e /root/restore-etc/hostname"

echo "== key verify"
check "verify: the exported file opens the newest backup" bash -c "docker exec $client nestegg key verify /root/nestegg-key.json | grep -q 'opens the newest backup: yes'"
check "verify: a saved paper key (HTML) opens it too" on_client nestegg key verify /root/printed/paper.html
check "verify: a saved paper key (text) opens it too" on_client nestegg key verify /root/printed/paper.txt
check "verify: another key fails" fails_with 1 'matches the stored key:  NO' on_client nestegg key verify /root/other-key.json
check "verify: a file that is no key fails" fails_with 1 'matches the stored key:  NO' on_client nestegg key verify /etc/hostname

echo "== review findings: export, restore as user, key handling"
alice_uid="$(on_client id -u alice)"
alice_gid="$(on_client id -g alice)"
as_sudo_alice() { docker exec -e SUDO_UID="$alice_uid" -e SUDO_GID="$alice_gid" "$client" "$@"; }
on_client bash -c 'echo keep > /root/victim && ln -s /root/victim /home/alice/evil-key.json'
check "export refuses a planted symlink" \
  fails_with 1 'File exists' as_sudo_alice nestegg key export /home/alice/evil-key.json
check "symlink target untouched" bash -c "[[ \$(docker exec $client cat /root/victim) == keep ]]"
as_sudo_alice nestegg key export /home/alice/key-copy.json >/dev/null
check "confirm with the exported file works" on_client nestegg key confirm-exported --file /home/alice/key-copy.json --accept-local-copy
check "export as sudo user: owned by user, mode 600" bash -c "[[ \$(docker exec $client stat -c '%U %a' /home/alice/key-copy.json) == 'alice 600' ]]"
check "export cannot write where the user may not" \
  fails_with 1 'Permission denied' as_sudo_alice nestegg key export /etc/nestegg-key.json
check "restore into user dir succeeds (exit code)" bash -c "docker exec -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg restore $snapshot home.pxar /home/alice/restore-home > /tmp/nestegg-user-restore.log 2>&1"
check "user restore: full content matches" on_client diff -r /home/alice/Documents /home/alice/restore-home/alice/Documents
check "user restore: files owned by the user" no_output on_client find /home/alice/restore-home ! -user alice
check "user restore with 3000 sockets did not hang (warnings shown)" bash -c "[[ \$(grep -c 'socket ignored' /tmp/nestegg-user-restore.log) -ge 3000 ]]"
check "user restore: read-only directory restored" on_client test -f /home/alice/restore-home/alice/Archive/frozen/note.txt
long_name="$(printf 'r%.0s' $(seq 1 240))"
check "user restore into a 240-character target name" bash -c "docker exec -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg restore $snapshot etc.pxar /home/alice/$long_name >/dev/null 2>&1 && docker exec $client test -f /home/alice/$long_name/hostname"
check "failing user restore (target full) reports the cause" \
  fails_with 1 '[Nn]o space left on device' \
  as_sudo_alice nestegg restore "$snapshot" home.pxar /mnt/small/target --pattern 'alice/Documents/photo.bin'
check "failing user restore: no partial copy left" no_output on_client ls -A /mnt/small
check "failing user restore: staging cleaned up" no_output on_client ls -A /var/lib/nestegg/restore-staging
check "restore into missing parent fails before any work" \
  fails_with 1 '/home/alice/missing does not exist; create it first' \
  as_sudo_alice nestegg restore "$snapshot" etc.pxar /home/alice/missing/target
check "user restore: sparse file stays sparse" bash -c "[[ \$(docker exec $client du -k /home/alice/restore-home/alice/Documents/sparse.img | cut -f1) -lt 1024 ]]"
check "user restore: no partial directory left" no_output on_client find /home/alice -maxdepth 1 -name '.*nestegg-partial*'
check "user restore: staging cleaned up" no_output on_client ls -A /var/lib/nestegg/restore-staging
check "relative export path as sudo user" bash -c "docker exec -w /home/alice -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg key export rel-key.json >/dev/null && docker exec $client test -f /home/alice/rel-key.json"
check "relative --keyfile as sudo user" bash -c "docker exec -w /home/alice -e SUDO_UID=$alice_uid -e SUDO_GID=$alice_gid $client nestegg restore $snapshot etc.pxar /root/restore-relkey --keyfile rel-key.json >/dev/null 2>&1"
check "invalid SUDO_UID is rejected" \
  fails_with 1 "SUDO_UID 'abc' is not a valid user id" docker exec -e SUDO_UID=abc "$client" nestegg key export /root/x.json
check "user restore: capability not applied" bash -c "docker exec $client test -f /home/alice/restore-home/alice/Documents/tool-with-capability \
  && [[ -z \$(docker exec $client getcap /home/alice/restore-home/alice/Documents/tool-with-capability) ]]"
check "root restore keeps the capability" bash -c "docker exec $client getcap /root/restore-home/alice/Documents/tool-with-capability | grep -q cap_net_raw"
check "relative restore target is resolved" bash -c "docker exec -w /root $client nestegg restore $snapshot etc.pxar relative-restore >/dev/null 2>&1 && docker exec $client test -e /root/relative-restore/hostname"
check "restore target with .. is refused" \
  fails_with 1 "the restore target must not contain '\.\.'" \
  on_client nestegg restore "$snapshot" etc.pxar /root/missing/../../tmp/x
check "restore into /tmp without sudo user is refused" \
  fails_with 1 '/tmp/restore-etc is inside a directory that other users can modify' \
  on_client nestegg restore "$snapshot" etc.pxar /tmp/restore-etc

on_client sed -i 's/^encryption = true/encryption = false/' /etc/nestegg/config.toml
check "unencrypted backup succeeds" on_client nestegg run --trigger manual
plain_snapshot="$(on_client nestegg snapshots --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
check "unencrypted snapshot restores while a key exists" on_client nestegg restore "$plain_snapshot" etc.pxar /root/restore-plain
check "older encrypted snapshot still restores" on_client nestegg restore "$snapshot" etc.pxar /root/restore-enc
on_client sed -i 's/^encryption = false/encryption = true/' /etc/nestegg/config.toml

on_client nestegg key create --force --allow-plain >/dev/null
check "new key blocks backups until exported" \
  fails_with 1 'encryption key has not been exported yet' on_client nestegg backup
check "status shows key not exported" bash -c "docker exec $client nestegg status | grep -q 'KEY NOT EXPORTED'"
check "the old key's file does not confirm the new key" \
  fails_with 1 'does not contain the key' \
  on_client nestegg key confirm-exported --file /root/nestegg-key.json --accept-local-copy
check "the old paper key does not confirm the new key" \
  fails_with 1 'does not belong to the key' on_client nestegg key confirm-exported --paper-code "$paper_code" --printed
on_client nestegg key export /root/nestegg-key-2.json >/dev/null
# With a terminal the user is asked; `script` gives the command one.
check "interactive: anything but yes refuses" bash -c "! printf 'no\n' | docker exec -i $client script -qec 'nestegg key confirm-exported --file /root/nestegg-key-2.json' /dev/null >/dev/null 2>&1"
check "still not confirmed after answering no" \
  fails_with 1 'encryption key has not been exported yet' on_client nestegg backup
check "interactive: yes confirms a copy on the internal disk" bash -c "printf 'yes\n' | docker exec -i $client script -qec 'nestegg key confirm-exported --file /root/nestegg-key-2.json' /dev/null >/dev/null 2>&1"
check "verify: the newest snapshot is unencrypted, so it proves nothing" \
  fails_with 0 'opens the newest backup: not checked - .*not encrypted' on_client nestegg key verify /root/nestegg-key-2.json
check "verify: the old key is no longer the stored key" \
  fails_with 1 'matches the stored key:  NO' on_client nestegg key verify /root/nestegg-key.json
check "backup runs again after confirming the new key" on_client nestegg backup
check "verify: the new key opens the new snapshot" bash -c "docker exec $client nestegg key verify /root/nestegg-key-2.json | grep -q 'opens the newest backup: yes'"
check "old snapshot restores with old exported key" on_client nestegg restore "$snapshot" etc.pxar /root/restore-oldkey --keyfile /root/nestegg-key.json

echo "== key verify: key replaced after the last backup"
on_client nestegg key create --force --allow-plain >/dev/null
on_client nestegg key export /root/nestegg-key-3.json >/dev/null
check "verify: the copy matches the new key but not the newest snapshot" \
  fails_with 1 'opens the newest backup: NO' on_client nestegg key verify /root/nestegg-key-3.json
# The paper code asks for the statement at a terminal, too.
on_client nestegg key paperkey --output /root/paper-3.txt >/dev/null
paper_code_3="$(on_client sed -n 's/^Check code: *//p' /root/paper-3.txt)"
on_client mv /root/paper-3.txt /root/printed/paper-3.txt
check "interactive: anything but yes refuses the paper code" bash -c "! printf 'no\n' | docker exec -i $client script -qec 'nestegg key confirm-exported --paper-code $paper_code_3' /dev/null >/dev/null 2>&1"
check "still not confirmed after answering no to the paper statement" \
  fails_with 1 'encryption key has not been exported yet' on_client nestegg backup
check "interactive: yes confirms the paper code" bash -c "printf 'yes\n' | docker exec -i $client script -qec 'nestegg key confirm-exported --paper-code $paper_code_3' /dev/null >/dev/null 2>&1"
check "the interactive paper confirmation is recorded" bash -c "docker exec $client nestegg key show | grep -q 'exported: *yes'"
on_client nestegg key confirm-exported --file /root/nestegg-key-3.json --accept-local-copy >/dev/null

echo "== second backup and timer behaviour"
check "manual second backup succeeds" on_client nestegg run --trigger manual
check "timer run is skipped when not due" bash -c "docker exec $client nestegg run --trigger timer 2>&1 | grep -q 'not due'"
check "four snapshots listed" bash -c "[[ \$(docker exec $client nestegg snapshots --json | grep -c '\"snapshot\"') == 4 ]]"

echo "== complete and verified backups (decision 0015)"
as_admin() {
  docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$pbs" \
    proxmox-backup-client "$@" --ns desktops --repository admin@pbs@127.0.0.1:store1
}
last_run_json() { on_client nestegg history --json --limit 1; }
check "every snapshot has the metadata blob" \
  bash -c "[[ \$(docker exec -e PBS_PASSWORD='$admin_pw' -e PBS_FINGERPRINT='$fp' $pbs proxmox-backup-client snapshot list --ns desktops --repository admin@pbs@127.0.0.1:store1 --output-format json | grep -o 'nestegg-meta.conf.blob' | wc -l) == 4 ]]"
check "a normal run is verified" bash -c "docker exec $client nestegg history --json --limit 1 | grep -q '\"verified\": true'"
check "read-back is in the run log" bash -c "docker exec $client sh -c 'tail -n 30 \$(ls -t /var/lib/nestegg/runs/*.log | head -1)' | grep -q 'read-back: inventory.pxar restored and identical'"

on_client cp /etc/nestegg/config.toml /root/config.toml.saved
on_client bash -c 'printf "\n[[backup.sources]]\narchive = \"data\"\npath = \"/srv/missing-data\"\noptional = true\n" >> /etc/nestegg/config.toml'
check "run with a missing optional source succeeds" on_client nestegg run --trigger manual
status_json="$(on_client nestegg status --json)"
check "missing optional source: health incomplete" grep -q '"health": "incomplete"' <<<"$status_json"
check "missing optional source: reason recorded" \
  grep -q '"optional source /srv/missing-data does not exist, skipped"' <<<"$status_json"
check "missing optional source: still verified" bash -c "docker exec $client nestegg history --json --limit 1 | grep -q '\"verified\": true'"
check "status text shows the reason" bash -c "docker exec $client nestegg status | grep -q '^Incomplete: *optional source /srv/missing-data'"
check "history marks the run as incomplete" \
  bash -c "docker exec $client nestegg history | head -1 | grep -q '^part .* - incomplete: optional source /srv/missing-data'"
part_snapshot="$(last_run_json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
check "snapshot notes report the incomplete backup" \
  bash -c "docker exec -e PBS_PASSWORD='$admin_pw' -e PBS_FINGERPRINT='$fp' $pbs proxmox-backup-client snapshot notes show '$part_snapshot' --ns desktops --repository admin@pbs@127.0.0.1:store1 | grep -q '\"incomplete\":\[\"optional source /srv/missing-data'"
on_client cp /root/config.toml.saved /etc/nestegg/config.toml

on_client sed -i 's/^\[backup\]$/[backup]\nsystem = true/' /etc/nestegg/config.toml
check "system mode without a recreatable root still backs up" on_client nestegg run --trigger manual
check "skipped full system backup makes it incomplete" \
  bash -c "docker exec $client nestegg status --json | grep -q '\"health\": \"incomplete\"' && docker exec $client nestegg history --json --limit 1 | grep -q '\"full system backup skipped: the root file system'"
on_client cp /root/config.toml.saved /etc/nestegg/config.toml

# Read-back failures. A test double around the real client changes what
# the server seems to return; the comparison code in nestegg is the real one.
on_client bash -c 'cat > /usr/local/bin/pbc-test-double <<"SH"
#!/bin/bash
real=/usr/bin/proxmox-backup-client
case "$DOUBLE:$1:$2:$3" in
  stale:restore:*:nestegg-meta.conf)
    # The server hands out the blob of an older snapshot.
    shift 2; exec "$real" restore "$STALE_FROM" "$@" ;;
  missing:snapshot:files:*)
    # The listing lacks home.pxar.
    "$real" "$@" | sed "s/home\.pxar\.didx/gone.pxar.didx/g"; exit "${PIPESTATUS[0]}" ;;
  down:restore:*)
    echo "Error: error trying to connect: tcp connect error: Connection refused (os error 111)" >&2
    exit 255 ;;
esac
exec "$real" "$@"
SH
chmod 755 /usr/local/bin/pbc-test-double'
stale_from="$(on_client nestegg snapshots --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
run_with_double() {
  docker exec -e NESTEGG_PBS_CLIENT=/usr/local/bin/pbc-test-double -e DOUBLE="$1" \
    -e STALE_FROM="$stale_from" "$client" nestegg run --trigger manual
}
check "stale metadata from the server fails the run" \
  fails_with 1 'nestegg-meta.conf has different content' run_with_double stale
check "stale metadata: kind verification_failed" bash -c "docker exec $client nestegg history --json --limit 1 | grep -q '\"kind\": \"verification_failed\"'"
check "stale metadata: run not verified" bash -c "docker exec $client nestegg history --json --limit 1 | grep -q '\"verified\": false'"
check "stale metadata: health failing" bash -c "docker exec $client nestegg status --json | grep -q '\"health\": \"failing\"'"
check "missing archive on the server fails the run" \
  fails_with 1 'home\.pxar is missing' run_with_double missing
check "server gone during the check fails the run" \
  fails_with 1 'could not be read back from the server' run_with_double down
check "server gone during the check: verification_failed, not pbs_unreachable" \
  bash -c "docker exec $client nestegg history --json --limit 1 | grep -q '\"kind\": \"verification_failed\"' && docker exec $client nestegg history --json --limit 1 | grep -q '\"state\": \"failed\"'"
check "a normal run afterwards is ok and verified again" on_client nestegg run --trigger manual
check "health ok again" bash -c "docker exec $client nestegg status --json | grep -q '\"health\": \"ok\"'"

echo "== daemon API"
on_client bash -c 'nestegg daemon >/var/log/nestegg-daemon.log 2>&1 &'
sleep 1
check "daemon status endpoint" bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock http://localhost/api/v1/status | grep -q '\"health\":\"ok\"'"
check "daemon snapshots endpoint" bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock http://localhost/api/v1/snapshots | grep -q e2e-desktop"
check "socket mode is 0660" bash -c "[[ \$(docker exec $client stat -c '%a' /run/nestegg/api.sock) == 660 ]]"

echo "== API as unprivileged user: session, browse, restore, machines, cancel"
on_client usermod -aG nestegg alice
on_client usermod -aG nestegg bob
API="docker exec -u alice $client curl -s --unix-socket /run/nestegg/api.sock"
# responds CODE PATTERN USER CURL-ARGS...: the daemon answers USER with HTTP
# status CODE and a body matching the extended regular expression PATTERN.
responds() {
  local want="$1" pattern="$2" user="$3" out code
  shift 3
  out="$(docker exec -u "$user" "$client" curl -s -w '\n%{http_code}' --unix-socket /run/nestegg/api.sock "$@")" \
    || { echo "    curl failed"; return 1; }
  code="${out##*$'\n'}"
  out="${out%$'\n'*}"
  [[ "$code" == "$want" ]] || { echo "    expected HTTP $want, got $code:"; show "$out"; return 1; }
  grep -Eq -- "$pattern" <<<"$out" || { echo "    body does not match /$pattern/:"; show "$out"; return 1; }
}
latest="$(on_client nestegg snapshots --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
ltime="${latest##*/}"
check "session identifies the caller" bash -c "$API http://localhost/api/v1/session | grep -q '\"user\":\"alice\"'"
check "browse root shows only the own home" bash -c "[[ \$($API 'http://localhost/api/v1/snapshots/$ltime/browse?archive=home.pxar&path=/' | grep -o '\"name\":\"[^\"]*\"' | sort -u | tr -d '\n') == '\"name\":\"alice\"' ]]"
check "browse inside the own home" bash -c "$API 'http://localhost/api/v1/snapshots/$ltime/browse?archive=home.pxar&path=/alice/Documents' | grep -q report.txt"
check "browse of another archive is refused" \
  responds 403 'does not contain your home directory' alice "http://localhost/api/v1/snapshots/$ltime/browse?archive=etc.pxar&path=/"
restore_resp="$($API -X POST -H 'Content-Type: application/json' -d "{\"snapshot_time\":\"$ltime\",\"archive\":\"home.pxar\",\"paths\":[\"/alice/Documents\"],\"target_dir\":\"/home/alice/api-restore\"}" http://localhost/api/v1/restores)"
restore_id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$restore_resp")"
check "restore accepted" test -n "$restore_id"
for _ in $(seq 1 60); do
  rstate="$($API "http://localhost/api/v1/restores/$restore_id" | sed -n 's/.*"state":"\([^"]*\)".*/\1/p')"
  [[ "$rstate" != running ]] && break
  sleep 1
done
check "API restore succeeded" test "$rstate" = succeeded
check "API restore: content matches" on_client cmp /home/alice/Documents/report.txt /home/alice/api-restore/alice/Documents/report.txt
check "API restore: owned by the caller" no_output on_client find /home/alice/api-restore ! -user alice
check "API restore: only the selected folder" absent_in /home/alice/api-restore/alice .bashrc
check "API restore outside the own home is refused" \
  responds 403 '"reason":"restore_outside_home"' alice -X POST -H 'Content-Type: application/json' \
  -d "{\"snapshot_time\":\"$ltime\",\"archive\":\"home.pxar\",\"paths\":[\"/root\"],\"target_dir\":\"/home/alice/x1\"}" \
  http://localhost/api/v1/restores
check "API restore into an existing target is 409" \
  responds 409 '"code":"target_exists"' alice -X POST -H 'Content-Type: application/json' \
  -d "{\"snapshot_time\":\"$ltime\",\"archive\":\"home.pxar\",\"paths\":[],\"target_dir\":\"/home/alice/api-restore\"}" \
  http://localhost/api/v1/restores
full_resp="$($API -X POST -H 'Content-Type: application/json' -d "{\"snapshot_time\":\"$ltime\",\"archive\":\"home.pxar\",\"paths\":[],\"target_dir\":\"/home/alice/api-full\"}" http://localhost/api/v1/restores)"
full_id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$full_resp")"
for _ in $(seq 1 120); do
  fstate="$($API "http://localhost/api/v1/restores/$full_id" | sed -n 's/.*"state":"\([^"]*\)".*/\1/p')"
  [[ "$fstate" != running ]] && break
  sleep 1
done
check "API restore of the whole own home succeeded" test "$fstate" = succeeded
check "another user's directory named like the caller is NOT restored" bash -c "[[ \$(docker exec $client ls -A /home/alice/api-full) == alice ]]"
glob_resp="$($API -X POST -H 'Content-Type: application/json' -d "{\"snapshot_time\":\"$ltime\",\"archive\":\"home.pxar\",\"paths\":[\"/alice/Documents/[ab].txt\"],\"target_dir\":\"/home/alice/api-glob\"}" http://localhost/api/v1/restores)"
glob_id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$glob_resp")"
for _ in $(seq 1 60); do
  gstate="$($API "http://localhost/api/v1/restores/$glob_id" | sed -n 's/.*"state":"\([^"]*\)".*/\1/p')"
  [[ "$gstate" != running ]] && break
  sleep 1
done
check "glob characters in a name are taken literally" bash -c "docker exec $client test -f '/home/alice/api-glob/alice/Documents/[ab].txt' && ! docker exec $client test -e /home/alice/api-glob/alice/Documents/a.txt"
check "restore job is visible to its owner" responds 200 '"state":"succeeded"' alice "http://localhost/api/v1/restores/$glob_id"
check "restore job of another user is not visible" responds 404 '"message":"unknown restore"' bob "http://localhost/api/v1/restores/$glob_id"
check "machines lists this PC with its report" bash -c "$API http://localhost/api/v1/machines | grep -q '\"hostname\":'"

echo "== an archive sourced below the caller's home"
# Today's configuration is deliberately different: only snapshot metadata counts.
on_client sed -i 's#path = "/home/alice/Documents"#path = "/etc"#' /etc/nestegg/config.toml
check "docs archive root maps directly to Documents" \
  bash -c "$API 'http://localhost/api/v1/snapshots/$ltime/browse?archive=docs.pxar&path=/' | grep -q '\"name\":\"report.txt\"'"
check "docs archive traversal is refused" \
  responds 400 "paths must not contain" alice "http://localhost/api/v1/snapshots/$ltime/browse?archive=docs.pxar&path=/../etc"
for selection in '[]' '["/report.txt"]'; do
  if [[ $selection == '[]' ]]; then target=/home/alice/api-docs-full; else target=/home/alice/api-docs-file; fi
  response="$($API -X POST -H 'Content-Type: application/json' \
    -d "{\"snapshot_time\":\"$ltime\",\"archive\":\"docs.pxar\",\"paths\":$selection,\"target_dir\":\"$target\"}" http://localhost/api/v1/restores)"
  id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$response")"
  check "docs restore accepted ($selection)" test -n "$id"
  for _ in $(seq 1 60); do
    state="$($API "http://localhost/api/v1/restores/$id" | sed -n 's/.*"state":"\([^"]*\)".*/\1/p')"
    [[ $state != running ]] && break
    sleep 1
  done
  check "docs restore succeeded ($selection)" test "$state" = succeeded
  check "docs restore maps the root ($selection)" on_client cmp /home/alice/Documents/report.txt "$target/report.txt"
  check "docs restore runs as the caller ($selection)" no_output on_client find "$target" ! -user alice
  check "docs restore omits the synthetic record ($selection)" absent_in "$target" .pxarexclude-cli
done
check "selected docs restore contains only that file" \
  bash -c "[[ \$(docker exec $client ls -A /home/alice/api-docs-file) == report.txt ]]"

on_client bash -c 'sed -i "/^archive = \"docs\"$/{n;s#path = \"/etc\"#path = \"/home/alice/Documents\"#;}" /etc/nestegg/config.toml'

echo "== non-root permissions come from the backup metadata, not the notes (decision 0016)"
as_backup_token() {
  docker exec -e PBS_PASSWORD="$token_secret" -e PBS_FINGERPRINT="$fp" "$client" \
    proxmox-backup-client "$@" --ns desktops --repository 'backup@pbs!desktop@pbs.e2e.test:store1'
}
# A snapshot the daemon has not looked at yet; the backup token may rewrite
# its notes. The forged report claims etc.pxar was made from alice's home.
forged_snapshot="$(on_client nestegg snapshots --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | sed -n 2p)"
forged_time="${forged_snapshot##*/}"
as_backup_token snapshot notes update "$forged_snapshot" \
  "$(printf 'forged\n\n{"archive_sources":{"etc.pxar":"/home/alice","home.pxar":"/"}}')"
check "forged notes are on the server" \
  bash -c "docker exec -e PBS_PASSWORD='$token_secret' -e PBS_FINGERPRINT='$fp' $client proxmox-backup-client snapshot notes show '$forged_snapshot' --ns desktops --repository 'backup@pbs!desktop@pbs.e2e.test:store1' | grep -q '\"etc.pxar\":\"/home/alice\"'"
check "forged notes do not open etc.pxar to alice" \
  responds 403 'does not contain your home directory' alice "http://localhost/api/v1/snapshots/$forged_time/browse?archive=etc.pxar&path=/"
check "forged notes do not change alice's view of home.pxar" \
  bash -c "[[ \$($API 'http://localhost/api/v1/snapshots/$forged_time/browse?archive=home.pxar&path=/' | grep -o '\"name\":\"[^\"]*\"' | sort -u | tr -d '\n') == '\"name\":\"alice\"' ]]"
check "forged notes do not allow restoring etc.pxar" \
  responds 403 'does not contain your home directory' alice -X POST -H 'Content-Type: application/json' \
  -d "{\"snapshot_time\":\"$forged_time\",\"archive\":\"etc.pxar\",\"paths\":[],\"target_dir\":\"/home/alice/forged\"}" \
  http://localhost/api/v1/restores

# A snapshot without metadata blob, as older versions made them (made with
# the current key: nestegg-key-3.json since the last `key create` above).
as_backup_token backup home.pxar:/home docs.pxar:/home/alice/Documents --backup-type host --backup-id e2e-desktop \
  --keyfile /root/nestegg-key-3.json >/dev/null 2>&1
raw_snapshot="$(on_client nestegg snapshots --json | sed -n 's/.*"snapshot": "\([^"]*\)".*/\1/p' | head -1)"
raw_time="${raw_snapshot##*/}"
check "snapshot without metadata blob created" \
  bash -c "[[ '$raw_snapshot' != '$latest' ]] && ! docker exec -e PBS_PASSWORD='$admin_pw' -e PBS_FINGERPRINT='$fp' $pbs proxmox-backup-client snapshot files '$raw_snapshot' --ns desktops --repository admin@pbs@127.0.0.1:store1 | grep -q nestegg-meta"
check "no metadata and no local record: refused for a user" \
  responds 403 'ask an administrator .*sudo nestegg restore' alice "http://localhost/api/v1/snapshots/$raw_time/browse?archive=home.pxar&path=/"
check "no metadata and no local record: root may still browse" \
  bash -c "docker exec $client curl -sf --unix-socket /run/nestegg/api.sock 'http://localhost/api/v1/snapshots/$raw_time/browse?archive=home.pxar&path=/' | grep -q '\"name\":\"bob\"'"
# The local, root-owned run record an older version wrote for it.
on_client bash -c "f=\$(ls -t /var/lib/nestegg/runs/*.json | head -1) && sed -e 's#\"snapshot\": \"[^\"]*\"#\"snapshot\": \"$raw_snapshot\"#' -e 's#\"id\": \"[^\"]*\"#\"id\": \"20000101T000000Z-cli-0000\"#' \"\$f\" > /var/lib/nestegg/runs/20000101T000000Z-cli-0000.json"
check "no metadata: the local run record is used" \
  bash -c "[[ \$($API 'http://localhost/api/v1/snapshots/$raw_time/browse?archive=home.pxar&path=/' | grep -o '\"name\":\"[^\"]*\"' | sort -u | tr -d '\n') == '\"name\":\"alice\"' ]]"
check "legacy non-canonical source below home stays closed" \
  responds 403 'handle-bound source metadata' alice "http://localhost/api/v1/snapshots/$raw_time/browse?archive=docs.pxar&path=/"
on_client rm -f /var/lib/nestegg/runs/20000101T000000Z-cli-0000.json

check "pbs info: reachable" bash -c "$API http://localhost/api/v1/pbs | grep -q '\"reachable\":true'"
check "connection check via API" bash -c "$API -X POST http://localhost/api/v1/pbs/check | grep -q 'PBS login and datastore access'"

# Cancel a running backup: throttle the upload and add new data.
on_client sed -i 's/^\[backup\]$/[backup]\nrate_limit = "100KiB"/' /etc/nestegg/config.toml
on_client bash -c 'head -c 20000000 /dev/urandom > /home/alice/big-new-file.bin'
run_resp="$($API -X POST http://localhost/api/v1/runs)"
run_id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$run_resp")"
for _ in $(seq 1 60); do
  $API http://localhost/api/v1/status | grep -q '"state":"uploading"' && break
  sleep 1
done
check "cancel request accepted" bash -c "$API -o /dev/null -w '%{http_code}' -X POST http://localhost/api/v1/runs/$run_id/cancel | grep -q 202"
for _ in $(seq 1 60); do
  $API "http://localhost/api/v1/runs/$run_id" | grep -q '"state":"cancelled"' && break
  sleep 1
done
check "cancelled run recorded as cancelled" bash -c "$API http://localhost/api/v1/runs/$run_id | grep -q '\"state\":\"cancelled\"'"
check "run record names who cancelled" bash -c "$API http://localhost/api/v1/runs/$run_id | grep -q 'cancel requested by alice (uid $alice_uid)'"
check "daemon log names who cancelled" on_client grep -q "backup run $run_id cancelled by alice (uid $alice_uid)" /var/log/nestegg-daemon.log
check "no backup client left running" exits_with 1 on_client pgrep -f 'proxmox-backup-client backup'
# A run that exists but is over cannot be cancelled (409); an id that does not exist is 404.
check "cancelling a finished run: 409 cannot_cancel" \
  responds 409 '"code":"cannot_cancel".*already finished' alice -X POST "http://localhost/api/v1/runs/$run_id/cancel"
check "cancelling an unknown run: 404 not_found" \
  responds 404 '"code":"not_found"' alice -X POST http://localhost/api/v1/runs/no-such-run/cancel
on_client sed -i '/^rate_limit = "100KiB"$/d' /etc/nestegg/config.toml
on_client rm -f /home/alice/big-new-file.bin

echo "== token cannot delete backups"
as_token() {
  docker exec -e PBS_PASSWORD="$token_secret" -e PBS_FINGERPRINT="$fp" "$client" \
    proxmox-backup-client "$@" --ns desktops --repository 'backup@pbs!desktop@pbs.e2e.test:store1'
}
pbs_has_snapshot() {
  docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$pbs" \
    proxmox-backup-client snapshot files "$1" --ns desktops --repository admin@pbs@127.0.0.1:store1 >/dev/null 2>&1
}
# token_may_not forget|prune SNAPSHOT: the PBS permission check refuses to let
# the token forget SNAPSHOT or prune its group down to the newest snapshot
# (which would remove SNAPSHOT, the oldest one). The PBS checks privileges
# before it looks the snapshot up, so SNAPSHOT is also checked to exist
# before and after: a wrong name or a refusal for another reason (login,
# connection, usage) cannot pass.
no_delete_priv='permission check failed - missing Datastore\.Modify\|Datastore\.Prune on /datastore/store1/desktops'
token_may_not() {
  local action="$1" snap="$2"
  pbs_has_snapshot "$snap" || { echo "    $snap is not on the PBS"; return 1; }
  case "$action" in
    forget) fails_with 255 "$no_delete_priv" as_token snapshot forget "$snap" ;;
    prune)  fails_with 255 "$no_delete_priv" as_token prune "${snap%/*}" --keep-last 1 ;;
    *)      echo "    unknown action $action"; false ;;
  esac || return 1
  pbs_has_snapshot "$snap" || { echo "    $snap was removed"; return 1; }
}
check "token is not allowed to forget snapshots" token_may_not forget "$snapshot"
check "token is not allowed to prune" token_may_not prune "$snapshot"

echo "== failed server-side verification is recorded"
snap_dir="/datastore/ns/desktops/${snapshot}"
on_pbs bash -c "printf 'XXXX' | dd of='$snap_dir/nestegg-meta.conf.blob' bs=1 seek=40 conv=notrunc 2>/dev/null"
on_pbs proxmox-backup-manager verify store1 >/dev/null 2>&1 || true
check "run after a failed server verification succeeds" on_client nestegg run --trigger manual
check "state lists the snapshot the server marked as failed" \
  bash -c "docker exec $client nestegg status --json | tr -d ' \n' | grep -q '\"server_verify_failed\":\[\"$snapshot\"\]'"
check "status text shows the failed server check" bash -c "docker exec $client nestegg status | grep -q '^Server check: FAILED for 1 snapshot(s): $snapshot'"

echo "== PBS unreachable"
docker stop "$pbs" >/dev/null
# The connection check with the PBS stopped and no Wake-on-LAN: the server did not answer (failed),
# so the login could not be tried (not checked); neither is reported as fine.
set +e
check_out="$(on_client nestegg check 2>&1)"
check_code=$?
set -e
check "nestegg check: exit code 1 when the PBS does not answer" test "$check_code" -eq 1
# The same without the timer failure of this container: still 1, for the server.
check "nestegg check: exit code 1 for the server alone" \
  exits_with 1 docker exec -e NESTEGG_SYSTEMCTL=/bin/true "$client" nestegg check
check "nestegg check: PBS reachable is a FAIL, the login is skipped" \
  bash -c "grep -q '^\[FAIL\] PBS reachable' <<<\"\$1\" && grep -q '^\[skip\] PBS login and datastore access' <<<\"\$1\"" _ "$check_out"
check "POST /pbs/check (PBS down, no Wake-on-LAN): reachable failed, login not checked" \
  responds 200 '"name":"PBS reachable","ok":false,"status":"failed".*"name":"PBS login and datastore access","ok":false,"status":"not_checked"' alice -X POST http://localhost/api/v1/pbs/check
set +e
on_client nestegg run --trigger manual >/dev/null 2>&1
code=$?
set -e
check "exit code 75 when PBS is down" test "$code" -eq 75
check "state is pbs_unreachable" bash -c "docker exec $client nestegg status --json | grep -q '\"state\": \"pbs_unreachable\"'"
check "health stays ok after a recent success" bash -c "docker exec $client nestegg status --json | grep -q '\"health\": \"ok\"'"

echo "== Wake-on-LAN wakes the sleeping PBS"
# The PBS is still stopped. A second container on the same network stands in
# for the network card of the sleeping server and prints every datagram that
# arrives on UDP port 9. This script plays the mainboard: once the magic
# packet for the configured MAC address has arrived, it switches the PBS on.
subnet="$(docker network inspect -f '{{(index .IPAM.Config 0).Subnet}}' "$net")"
bcast="$(python3 -c 'import ipaddress, sys; print(ipaddress.ip_network(sys.argv[1]).broadcast_address)' "$subnet")"
docker run -d --name "$wol" --network "$net" "$client_image" perl -MIO::Socket::INET -e '
  $| = 1;
  my $s = IO::Socket::INET->new(Proto => "udp", LocalPort => 9, ReuseAddr => 1, Broadcast => 1) or die $!;
  while (defined $s->recv(my $buf, 200)) { print unpack("H*", $buf), "\n"; }' >/dev/null
docker exec -i "$client" tee -a /etc/nestegg/config.toml >/dev/null <<WAKE

[wake]
mac = "00:00:5e:00:53:01"
broadcast = "$bcast"
port = 9
timeout_seconds = 120
WAKE
# The PBS is stopped and Wake-on-LAN is configured. Without a wake request nothing was checked:
# "not checked" (API status not_checked), never "fine" and not a failure either. (The exit code 2 for
# "nothing failed, but not everything was checked" is covered by unit tests: this container has no
# systemd timer, so `nestegg check` fails here for that reason anyway.)
check_out="$(on_client nestegg check 2>&1 || true)"
check "nestegg check: PBS reachable and the login are skip, neither ok nor FAIL" \
  bash -c "grep -q '^\[skip\] PBS reachable' <<<\"\$1\" && grep -q '^\[skip\] PBS login and datastore access' <<<\"\$1\" && ! grep -q '^\[FAIL\] PBS' <<<\"\$1\"" _ "$check_out"
check "nestegg check: exit code 2 when nothing failed but the sleeping server was not checked" \
  exits_with 2 docker exec -e NESTEGG_SYSTEMCTL=/bin/true "$client" nestegg check
check "nestegg check: says how to wake the server and check everything" \
  bash -c "grep -q 'sudo nestegg check --wake' <<<\"\$1\"" _ "$check_out"
check "POST /pbs/check (PBS asleep, Wake-on-LAN): not_checked, ok false for old clients" \
  responds 200 '"name":"PBS reachable","ok":false,"status":"not_checked".*"name":"PBS login and datastore access","ok":false,"status":"not_checked"' alice -X POST http://localhost/api/v1/pbs/check
# Asked to wake it and nobody answers: a failed check. A packet for another MAC address goes out, so
# the stand-in mainboard below does not switch the PBS on, and the wait is short.
on_client sed -i 's/^mac = "00:00:5e:00:53:01"$/mac = "00:00:5e:00:53:02"/; s/^timeout_seconds = 120$/timeout_seconds = 6/' /etc/nestegg/config.toml
check "POST /pbs/check?wake=true (the PBS does not come up): reachable failed" \
  responds 200 'no answer, not even after the Wake-on-LAN attempt","name":"PBS reachable","ok":false,"status":"failed"' alice -X POST 'http://localhost/api/v1/pbs/check?wake=true'
on_client sed -i 's/^mac = "00:00:5e:00:53:02"$/mac = "00:00:5e:00:53:01"/; s/^timeout_seconds = 6$/timeout_seconds = 120/' /etc/nestegg/config.toml
magic="ffffffffffff$(printf '00005e005301%.0s' $(seq 1 16))"
(
  for _ in $(seq 1 200); do
    if docker logs "$wol" 2>/dev/null | grep -qx "$magic"; then
      docker start "$pbs" >/dev/null
      exit 0
    fi
    sleep 0.5
  done
  exit 1
) &
waker=$!
set +e
wake_out="$(on_client nestegg run --trigger manual 2>&1)"
code=$?
wait "$waker"
woke=$?
set -e
check "the magic packet for the configured MAC arrived on the network" test "$woke" -eq 0
check "backup succeeds after the PBS was woken" test "$code" -eq 0
check "the run says the PBS answered after Wake-on-LAN" \
  bash -c "grep -q 'PBS reachable after Wake-on-LAN' <<<\"\$1\"" _ "$wake_out"
check "the last run is recorded as succeeded" \
  bash -c "docker exec $client nestegg status --json | tr -d ' \n' | grep -q '\"last_run\":{[^}]*\"state\":\"succeeded\"'"

echo "== forged metadata blob while encryption is on stays closed (decision 0016)"
# This block runs last: it uploads snapshots with timestamps after the group's
# newest one, so no real backup follows that the PBS would then refuse.
# The backup token uploads an UNENCRYPTED snapshot into this machine's own
# group, with a forged nestegg-meta.conf that claims etc.pxar was made from
# alice's home. The blob names the correct snapshot and backup time, so only
# the missing signature (the client is handed the key and refuses an unsigned
# manifest) can keep it out.
write_forged_blob() {
  local file="$1" snap="$2" epoch="$3" sources="$4"
  on_client bash -c "cat > '$file' <<JSON
{
  \"format\": 1,
  \"nestegg_version\": \"forged\",
  \"backup_id\": \"e2e-desktop\",
  \"hostname\": \"desktop\",
  \"backup_time\": $epoch,
  \"snapshot\": \"$snap\",
  \"archive_sources\": $sources,
  \"sources\": [\"/home\"],
  \"system_mode\": false,
  \"incomplete\": []
}
JSON"
}
# Newer than the group's last snapshot: the PBS refuses a backup whose time
# is older than the newest one in the group.
forged_epoch=$(( $(date -u +%s) + 10 ))
forged_snap="host/e2e-desktop/$(on_client date -u -d @$forged_epoch +%Y-%m-%dT%H:%M:%SZ)"
forged_stime="${forged_snap##*/}"
write_forged_blob /tmp/forged-meta.conf "$forged_snap" "$forged_epoch" \
  '{"etc.pxar": "/home/alice", "home.pxar": "/"}'
check "backup token uploads a forged unencrypted snapshot" \
  bash -c "docker exec -e PBS_PASSWORD='$token_secret' -e PBS_FINGERPRINT='$fp' $client proxmox-backup-client backup nestegg-meta.conf:/tmp/forged-meta.conf etc.pxar:/etc home.pxar:/home --backup-type host --backup-id e2e-desktop --backup-time '$forged_epoch' --crypt-mode none --ns desktops --repository 'backup@pbs!desktop@pbs.e2e.test:store1'"
check "forged unencrypted snapshot with a matching blob is on the server" \
  bash -c "docker exec -e PBS_PASSWORD='$admin_pw' -e PBS_FINGERPRINT='$fp' $pbs proxmox-backup-client snapshot files '$forged_snap' --ns desktops --repository admin@pbs@127.0.0.1:store1 | grep -q nestegg-meta.conf.blob"
check "forged unencrypted blob does not open etc.pxar to alice" \
  responds 403 'ask an administrator' alice "http://localhost/api/v1/snapshots/$forged_stime/browse?archive=etc.pxar&path=/"
check "forged unencrypted blob does not let alice restore etc.pxar" \
  responds 403 'ask an administrator' alice -X POST -H 'Content-Type: application/json' \
  -d "{\"snapshot_time\":\"$forged_stime\",\"archive\":\"etc.pxar\",\"paths\":[],\"target_dir\":\"/home/alice/forged-blob\"}" \
  http://localhost/api/v1/restores

# Root restores the unprotected archive through the daemon while encryption is
# configured: it works, the job warns, and the daemon log names snapshot,
# archive and requester.
unprot_resp="$(docker exec "$client" curl -s --unix-socket /run/nestegg/api.sock -X POST -H 'Content-Type: application/json' \
  -d "{\"snapshot_time\":\"$forged_stime\",\"archive\":\"etc.pxar\",\"paths\":[],\"target_dir\":\"/root/unprotected-restore\"}" \
  http://localhost/api/v1/restores)"
unprot_id="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$unprot_resp")"
check "root restore of an unprotected archive accepted" test -n "$unprot_id"
for _ in $(seq 1 60); do
  unprot_job="$(docker exec "$client" curl -s --unix-socket /run/nestegg/api.sock "http://localhost/api/v1/restores/$unprot_id")"
  grep -q '"state":"running"' <<<"$unprot_job" || break
  sleep 1
done
check "unprotected restore succeeded" grep -q '"state":"succeeded"' <<<"$unprot_job"
check "unprotected restore: the job warns" grep -q 'not protected by the encryption key' <<<"$unprot_job"
check "unprotected restore: the daemon log names snapshot, archive and requester" \
  on_client grep -q "restore of etc.pxar from $forged_snap for root: the archive is not protected" /var/log/nestegg-daemon.log
check "CLI: an archive that is not in the snapshot has its own message" \
  fails_with 1 "data.pxar is not in $forged_snap" on_client nestegg restore "$forged_snap" data.pxar /root/not-there

# The same, but signed with a DIFFERENT key than the configured one. The
# client is handed the configured key, cannot verify the signature, and the
# snapshot stays closed (an error, never a widened scope).
other_epoch=$(( $(date -u +%s) + 20 ))
other_snap="host/e2e-desktop/$(on_client date -u -d @$other_epoch +%Y-%m-%dT%H:%M:%SZ)"
other_stime="${other_snap##*/}"
write_forged_blob /tmp/other-meta.conf "$other_snap" "$other_epoch" '{"etc.pxar": "/home/alice"}'
check "backup token uploads a snapshot signed with another key" \
  bash -c "docker exec -e PBS_PASSWORD='$token_secret' -e PBS_FINGERPRINT='$fp' $client proxmox-backup-client backup nestegg-meta.conf:/tmp/other-meta.conf etc.pxar:/etc --backup-type host --backup-id e2e-desktop --backup-time '$other_epoch' --keyfile /root/other-key.json --crypt-mode encrypt --ns desktops --repository 'backup@pbs!desktop@pbs.e2e.test:store1'"
check "snapshot signed with another key: blob stored encrypted" \
  bash -c "docker exec -e PBS_PASSWORD='$admin_pw' -e PBS_FINGERPRINT='$fp' $pbs proxmox-backup-client snapshot files '$other_snap' --ns desktops --repository admin@pbs@127.0.0.1:store1 --output-format json | grep -q '\"crypt-mode\":\"encrypt\"'"
check "a snapshot signed with another key does not open etc.pxar to alice" \
  responds 500 'cannot read the backup metadata' alice "http://localhost/api/v1/snapshots/$other_stime/browse?archive=etc.pxar&path=/"

result
