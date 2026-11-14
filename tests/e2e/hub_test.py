"""Real PBS/hub API checks; all secrets travel over stdin, files or a PTY."""
import hashlib
import datetime
import json
import os
import pathlib
import pty
import re
import secrets
import select
import subprocess
import sys
import time

prefix = 'nestegg-hub-e2e-' + secrets.token_hex(5)
net, pbs, hub = (prefix + suffix for suffix in ('-net', '-pbs', '-hub'))
here = pathlib.Path(__file__).resolve().parent
binary = sys.argv[1]
passed = 0
serve = None


def run(args, data=None, check=True):
    result = subprocess.run(args, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and result.returncode:
        # Output may contain a submitted password echoed by an upstream tool.
        raise RuntimeError('test command failed: ' + args[0] + ' (exit ' + str(result.returncode) + ')')
    return result


def dx(container, *args, data=None, check=True):
    return run(['docker', 'exec', '-i', container, *args], data, check)


def write(container, path, data):
    dx(container, 'sh', '-c', 'umask 077; cat > "$1"', 'write', path,
       data=data if isinstance(data, bytes) else data.encode())


def ok(label, condition=True):
    global passed
    if not condition:
        raise AssertionError(label)
    passed += 1
    print('  ok  ' + label, flush=True)


def init_with_terminal(args):
    pid, terminal = pty.fork()
    if pid == 0:
        os.execvp('docker', ['docker', 'exec', '-it', hub, *args])
    deadline = time.monotonic() + 120
    pending = b''
    prompts = 0
    try:
        while time.monotonic() < deadline:
            if select.select([terminal], [], [], .2)[0]:
                try:
                    part = os.read(terminal, 8192)
                except OSError:
                    break
                if not part:
                    break
                pending = (pending + part)[-32768:]
                if pending.lower().rstrip().endswith(b':') and b'password' in pending.lower():
                    prompts += 1
                    if prompts > 2:
                        raise RuntimeError('unexpected master-key prompt')
                    os.write(terminal, b'e2e-disposable-master-passphrase\n')
                    pending = b''
            got, status = os.waitpid(pid, os.WNOHANG)
            if got:
                if os.waitstatus_to_exitcode(status) != 0:
                    # Show only safe diagnostic lines, never PTY prompt contents.
                    for line in pending.decode(errors='replace').splitlines():
                        if line.startswith('error:'):
                            print(line, file=sys.stderr)
                    raise RuntimeError('hub init failed')
                return
        _, status = os.waitpid(pid, 0)
        if os.waitstatus_to_exitcode(status) != 0:
            for line in pending.decode(errors='replace').splitlines():
                if line.startswith('error:'):
                    print(line, file=sys.stderr)
            raise RuntimeError('hub init failed')
        ok('master password passed directly through terminal', prompts == 2)
    finally:
        os.close(terminal)


pin = None


def request(path, method='GET', body=None, session=True, header=True, bearer_file=None, wanted=200):
    args = ['curl', '-fsS' if wanted < 400 else '-sS', '-k', '--pinnedpubkey', pin,
            '-w', '\n%{http_code}', '-X', method]
    if session:
        args += ['-b', '/run/hub-cookie', '-c', '/run/hub-cookie']
    if header:
        args += ['-H', 'X-Nestegg-Hub: 1']
    if bearer_file:
        args += ['-H', '@' + bearer_file]
    if body is not None:
        args += ['-H', 'Content-Type: application/json', '--data-binary', '@-']
    args += ['https://hub.example.com:8443' + path]
    result = dx(hub, 'sh', '-c', 'umask 077; exec "$@"', 'request', *args,
                data=None if body is None else json.dumps(body).encode(), check=False)
    payload, _, status = result.stdout.rpartition(b'\n')
    if status != str(wanted).encode():
        # Error codes/messages of this implementation never include secrets.
        parsed = json.loads(payload) if payload.startswith(b'{') else {}
        raise AssertionError(f'{method} {path}: expected {wanted}, got {status!r}; code {parsed.get("code")}')
    return json.loads(payload) if payload else None


try:
    print('== disposable PBS and hub', flush=True)
    for image in ['nestegg-e2e-pbs', 'nestegg-e2e-client:debian-trixie']:
        if run(['docker', 'image', 'inspect', image], check=False).returncode:
            dockerfile = 'pbs.Dockerfile' if image == 'nestegg-e2e-pbs' else 'client.Dockerfile'
            args = ['docker', 'build', '-q', '-t', image, '-f', str(here / dockerfile)]
            if dockerfile == 'client.Dockerfile':
                args += ['--build-arg', 'BASE=debian:trixie']
            run(args + [str(here)])
    run(['docker', 'network', 'create', '--internal', net])
    run(['docker', 'run', '-d', '--name', pbs, '--hostname', 'pbs', '--network', net,
         '--network-alias', 'pbs.example.com', '--tmpfs', '/run', 'nestegg-e2e-pbs'])
    run(['docker', 'run', '-d', '--name', hub, '--network', net, '--network-alias', 'hub.example.com',
         '--tmpfs', '/run', '-v', binary + ':/usr/local/bin/nestegg:ro',
         'nestegg-e2e-client:debian-trixie', 'sleep', 'infinity'])
    for _ in range(120):
        if dx(pbs, 'curl', '-skf', 'https://127.0.0.1:8007/api2/json/ping', check=False).returncode == 0:
            break
        time.sleep(.5)
    else:
        raise RuntimeError('disposable PBS failed to start')
    admin_pw = secrets.token_hex(32)
    dx(pbs, 'chpasswd', data=('root:' + admin_pw + '\n').encode())
    dx(pbs, 'mkdir', '-p', '/datastore')
    dx(pbs, 'proxmox-backup-manager', 'datastore', 'create', 'store1', '/datastore')
    fp_output = dx(pbs, 'proxmox-backup-manager', 'cert', 'info').stdout.decode()
    fp = re.search(r'Fingerprint \(sha256\): (.*)', fp_output).group(1).strip()
    write(hub, '/run/pbs-admin-password', admin_pw)
    hub_password = secrets.token_hex(24)
    write(hub, '/run/hub-admin-password', hub_password)
    dx(hub, 'useradd', '--system', '--home-dir', '/var/lib/nestegg-hub', 'nestegg-hub')
    # A package with the right metadata is enough for delivery/checksum tests;
    # this lane does not execute the computer-side join implementation.
    dx(hub, 'mkdir', '-p', '/run/deb/DEBIAN')
    write(hub, '/run/deb/DEBIAN/control', 'Package: nestegg\nVersion: 0.1.0\nArchitecture: amd64\nMaintainer: nestegg maintainers\nDescription: disposable hub test package\n')
    dx(hub, 'dpkg-deb', '--build', '/run/deb', '/run/nestegg.deb')
    init_with_terminal(['nestegg', 'hub', 'init', '--name', 'Home', '--url', 'https://hub.example.com:8443',
                        '--server', 'pbs.example.com', '--datastore', 'store1', '--namespace', 'household',
                        '--user', 'backup@pbs', '--fingerprint', fp, '--prepare-pbs', '--pbs-admin', 'root@pam',
                        '--pbs-admin-password-file', '/run/pbs-admin-password',
                        '--admin-password-file', '/run/hub-admin-password', '--allow-plain',
                        '--paperkey', '/run/master.html', '--defer-confirm', '--package', '/run/nestegg.deb'])
    ok('init prepared PBS user, namespace, ACL and verified token creation/deletion')
    status = dx(hub, 'nestegg', 'hub', 'status').stdout.decode()
    pin = re.search(r'^Pin: (.*)$', status, re.M).group(1)
    ok('unproven master blocks CLI join codes', dx(hub, 'nestegg', 'hub', 'join-code', check=False).returncode != 0)
    # Simulate interruption after the paper was written but before its
    # completion record: confirm must read it back and resume safely.
    pending_master = json.loads(dx(hub, 'cat', '/var/lib/nestegg-hub/state.json').stdout)
    pending_master['master']['paper_written'] = False
    write(hub, '/var/lib/nestegg-hub/state.json', json.dumps(pending_master))
    dx(hub, 'cp', '/run/master.html', '/run/master-copy.html')
    dx(hub, 'nestegg', 'hub', 'master-key', 'confirm', '--file', '/run/master-copy.html', '--accept-local-copy')
    ok('interrupted master output resumes before proof', json.loads(dx(hub, 'cat', '/var/lib/nestegg-hub/state.json').stdout)['master']['paper_written'])
    ok('proof removes private master key', dx(hub, 'test', '!', '-e', '/var/lib/nestegg-hub/master/master-private.pem').returncode == 0)
    # Simulate PID 1's LoadCredential* delivery, then actually run as the
    # dedicated account. No host systemd units are installed or started.
    dx(hub, 'mkdir', '-m', '700', '/run/hub-credentials')
    encrypted = dx(hub, 'test', '-f', '/etc/nestegg-hub/credentials/password.cred', check=False).returncode == 0
    if encrypted:
        dx(hub, 'systemd-creds', 'decrypt', '--name=password', '/etc/nestegg-hub/credentials/password.cred', '/run/hub-credentials/password')
    else:
        dx(hub, 'cp', '/etc/nestegg-hub/credentials/password', '/run/hub-credentials/password')
    dx(hub, 'cp', '/etc/nestegg-hub/tls.key', '/run/hub-credentials/hub-tls-key')
    dx(hub, 'chown', '-R', 'nestegg-hub:nestegg-hub', '/run/hub-credentials')
    # Repeat preparation on already matching PBS objects with their current
    # password. A second isolated hub config gets its own master key/proof.
    init_with_terminal(['nestegg', 'hub', '--config', '/etc/nestegg-hub-repeat/hub.toml', 'init',
                        '--state-dir', '/var/lib/nestegg-hub-repeat', '--name', 'Home',
                        '--url', 'https://hub.example.com:8443', '--server', 'pbs.example.com',
                        '--datastore', 'store1', '--namespace', 'household', '--user', 'backup@pbs',
                        '--fingerprint', fp, '--prepare-pbs', '--pbs-admin', 'root@pam',
                        '--pbs-admin-password-file', '/run/pbs-admin-password',
                        '--pbs-password-file', '/run/hub-credentials/password',
                        '--admin-password-file', '/run/hub-admin-password', '--allow-plain',
                        '--paperkey', '/run/master-repeat.html', '--defer-confirm'])
    ok('PBS preparation is idempotent for matching existing objects')
    paper = dx(hub, 'cat', '/run/master-repeat.html').stdout.decode()
    paper_code = re.search(r'Check code:</td><td><b[^>]*>([0-9A-F-]+)', paper).group(1)
    repeat = ['nestegg', 'hub', '--config', '/etc/nestegg-hub-repeat/hub.toml', 'master-key', 'confirm']
    ok('paper code needs removal of remembered local file', dx(hub, *repeat, '--paper-code', paper_code, '--printed', check=False).returncode != 0)
    dx(hub, 'rm', '/run/master-repeat.html')
    dx(hub, *repeat, '--paper-code', paper_code, '--printed')
    ok('printed paper code proves master key after file removal')
    serve = subprocess.Popen(['docker', 'exec', '-u', 'nestegg-hub', '-e', 'CREDENTIALS_DIRECTORY=/run/hub-credentials', hub,
                              'nestegg', 'hub', 'serve'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    for _ in range(60):
        if dx(hub, 'curl', '-skf', '--pinnedpubkey', pin, 'https://hub.example.com:8443/', check=False).returncode == 0:
            break
        time.sleep(.25)
    else:
        raise RuntimeError('hub did not start: ' + serve.stderr.read(1000).decode())
    ok('HTTPS service runs as dedicated unprivileged account')
    ok('no session is unauthorized', request('/api/hub/v1/hub', session=False, wanted=401)['code'] == 'unauthorized')
    ok('login requires custom header', request('/api/hub/v1/session', 'POST', {'password': hub_password}, session=False, header=False, wanted=401)['code'] == 'unauthorized')
    request('/api/hub/v1/session', 'POST', {'password': hub_password}, wanted=204)
    cookie = dx(hub, 'cat', '/run/hub-cookie').stdout.decode()
    ok('session cookie flags', '#HttpOnly_' in cookie and '\tTRUE\t' in cookie)
    headers = dx(hub, 'curl', '-skI', '--pinnedpubkey', pin, 'https://hub.example.com:8443/').stdout.lower()
    ok('security headers on static page', b'content-security-policy:' in headers and b'x-frame-options: deny' in headers)
    dx(hub, 'nestegg', 'hub', 'package', 'add', '/run/nestegg.deb')
    ok('hub state reports proven master and package', request('/api/hub/v1/hub')['master_key']['proven'])
    request('/api/hub/v1/join-codes', 'POST', {}, header=False, wanted=401)
    code = request('/api/hub/v1/join-codes', 'POST', {'note': 'test computer'}, wanted=201)
    listing = request('/api/hub/v1/join-codes')
    ok('codes stored/listed only as hashes', code['code'] not in json.dumps(listing))
    state = dx(hub, 'cat', '/var/lib/nestegg-hub/state.json').stdout
    ok('raw code absent from disk state', code['code'].encode() not in state)
    joined = request('/api/hub/v1/join', 'POST', {'code': code['code'], 'hostname': 'desktop'}, session=False, header=False)
    ok('join returns correct PBS token identity', joined['pbs']['auth_id'] == 'backup@pbs!desktop')
    ok('machine credential has 256 bits', len(joined['machine_credential']) == 64)
    request('/api/hub/v1/join', 'POST', {'code': code['code'], 'hostname': 'second'}, session=False, wanted=403)
    ok('used code uniformly rejected')
    revoked = request('/api/hub/v1/join-codes', 'POST', {}, wanted=201)
    request('/api/hub/v1/join-codes/' + revoked['code_id'], 'DELETE', wanted=204)
    ok('revoked code uniformly rejected', request('/api/hub/v1/join', 'POST', {'code': revoked['code'], 'hostname': 'revoked'}, wanted=403)['code'] == 'invalid_code')
    code2 = request('/api/hub/v1/join-codes', 'POST', {}, wanted=201)
    ok('joined name cannot be reused', request('/api/hub/v1/join', 'POST', {'code': code2['code'], 'hostname': 'desktop'}, wanted=409)['code'] == 'name_taken')
    ok('failed join leaves code usable', any(c['code_id'] == code2['code_id'] for c in request('/api/hub/v1/join-codes')['codes']))
    expired = request('/api/hub/v1/join-codes', 'POST', {}, wanted=201)
    data = json.loads(dx(hub, 'cat', '/var/lib/nestegg-hub/state.json').stdout)
    for c in data['codes']:
        if c['code_id'] == expired['code_id']:
            c['expires_at'] = '2000-01-01T00:00:00Z'
    write(hub, '/var/lib/nestegg-hub/state.json', json.dumps(data))
    ok('expired code uniformly rejected', request('/api/hub/v1/join', 'POST', {'code': expired['code'], 'hostname': 'expired'}, wanted=403)['code'] == 'invalid_code')
    wrong_pin = 'sha256//' + 'A' * 43 + '='
    ok('wrong pin fails with curl pin mismatch', dx(hub, 'curl', '-sk', '--pinnedpubkey', wrong_pin, 'https://hub.example.com:8443/join.sh', check=False).returncode == 90)
    script = dx(hub, 'curl', '-skf', '--pinnedpubkey', pin, 'https://hub.example.com:8443/join.sh').stdout.decode()
    ok('installer pins every hub download', script.count('-k --pinnedpubkey "$pin"') == 2 and '@HUB_' not in script)
    dx(hub, 'curl', '-skf', '--pinnedpubkey', pin, '-o', '/run/received.deb', 'https://hub.example.com:8443/pkg/nestegg.deb')
    sums = dx(hub, 'curl', '-skf', '--pinnedpubkey', pin, 'https://hub.example.com:8443/pkg/SHA256SUMS').stdout.decode()
    received = dx(hub, 'cat', '/run/received.deb').stdout
    ok('served package matches checksum', sums == hashlib.sha256(received).hexdigest() + '  nestegg.deb\n')
    # Real backup, with secret on an inherited descriptor, no environment value.
    write(hub, '/run/token', joined['pbs']['secret'])
    dx(hub, 'mkdir', '/run/source')
    write(hub, '/run/source/example.txt', 'disposable backup data\n')
    dx(hub, 'bash', '-c', 'PBS_PASSWORD_FD=10 PBS_FINGERPRINT="$1" proxmox-backup-client backup test.pxar:/run/source --repository "backup@pbs!desktop@pbs.example.com:store1" --ns household --backup-id desktop --crypt-mode none 10</run/token', 'backup', fp)
    snapshots_raw = dx(hub, 'bash', '-c', 'PBS_PASSWORD_FD=10 PBS_FINGERPRINT="$1" proxmox-backup-client snapshot list --repository "backup@pbs!desktop@pbs.example.com:store1" --ns household --output-format json 10</run/token', 'list', fp).stdout
    snapshot_time = json.loads(snapshots_raw)[0]['backup-time']
    snapshot = 'host/desktop/' + datetime.datetime.fromtimestamp(snapshot_time, datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    notes = 'nestegg test: ok\n\n' + json.dumps({'state': 'ok', 'warnings': []})
    dx(hub, 'bash', '-c', 'PBS_PASSWORD_FD=10 PBS_FINGERPRINT="$1" proxmox-backup-client snapshot notes update "$2" "$3" --repository "backup@pbs!desktop@pbs.example.com:store1" --ns household 10</run/token', 'notes', fp, snapshot, notes)
    machines = request('/api/hub/v1/machines')
    ok('machine overview reads real backup from PBS', machines['pbs_reachable'] and machines['machines'][0]['snapshot_count'] == 1)
    ok('overview reads snapshot notes as display-only report', machines['machines'][0]['report']['state'] == 'ok')
    run(['docker', 'stop', '-t', '2', pbs])
    cached = request('/api/hub/v1/machines')
    ok('offline PBS listing returns last cached snapshot without waking', not cached['pbs_reachable'] and cached['pbs_read_at'] == machines['pbs_read_at'] and cached['machines'][0]['snapshot_count'] == 1)
    run(['docker', 'start', pbs])
    for _ in range(120):
        if dx(pbs, 'curl', '-skf', 'https://127.0.0.1:8007/api2/json/ping', check=False).returncode == 0:
            break
        time.sleep(.25)
    # A group owned by a different token is never adopted by the hub.
    foreign_output = dx(pbs, 'proxmox-backup-manager', 'user', 'generate-token', 'backup@pbs', 'foreign').stdout.decode()
    foreign = {'value': re.search(r'"value"\s*:\s*"([^"]+)"', foreign_output).group(1)}
    dx(pbs, 'proxmox-backup-manager', 'acl', 'update', '/datastore/store1/household', 'DatastoreBackup', '--auth-id', 'backup@pbs!foreign')
    write(hub, '/run/foreign-token', foreign['value'])
    dx(hub, 'bash', '-c', 'PBS_PASSWORD_FD=10 PBS_FINGERPRINT="$1" proxmox-backup-client backup test.pxar:/run/source --repository "backup@pbs!foreign@pbs.example.com:store1" --ns household --backup-id orphan --crypt-mode none 10</run/foreign-token', 'backup', fp)
    code_foreign = request('/api/hub/v1/join-codes', 'POST', {}, wanted=201)
    ok('foreign-owned backup group cannot be adopted', request('/api/hub/v1/join', 'POST', {'code': code_foreign['code'], 'hostname': 'orphan', 'adopt': True}, wanted=409)['code'] == 'name_taken')
    write(hub, '/run/machine-auth', 'Authorization: Bearer ' + joined['machine_credential'] + '\n')
    request('/api/hub/v1/machines/' + joined['machine_id'], 'DELETE', wanted=204)
    ok('removal deletes PBS token', dx(hub, 'bash', '-c', 'PBS_PASSWORD_FD=10 PBS_FINGERPRINT="$1" proxmox-backup-client snapshot list --repository "backup@pbs!desktop@pbs.example.com:store1" --ns household 10</run/token', 'list', fp, check=False).returncode != 0)
    request('/api/hub/v1/machines/self/leave', 'POST', session=False, bearer_file='/run/machine-auth', wanted=401)
    ok('removal revokes machine credential')
    ok('existing backup requires adoption', request('/api/hub/v1/join', 'POST', {'code': code2['code'], 'hostname': 'desktop'}, wanted=409)['code'] == 'name_exists_on_pbs')
    rejoined = request('/api/hub/v1/join', 'POST', {'code': code2['code'], 'hostname': 'desktop', 'adopt': True})
    ok('same token identity adopts its former backup group', rejoined['pbs']['auth_id'] == joined['pbs']['auth_id'])
    write(hub, '/run/machine-auth', 'Authorization: Bearer ' + rejoined['machine_credential'] + '\n')
    request('/api/hub/v1/machines/self/leave', 'POST', session=False, header=False, bearer_file='/run/machine-auth', wanted=204)
    ok('computer can leave with Bearer authentication')
    request('/api/hub/v1/session', 'DELETE', wanted=204)
    ok('logout revokes session', not request('/api/hub/v1/session')['logged_in'])
    request('/api/hub/v1/session', 'POST', {'password': hub_password}, wanted=204)
    dx(hub, 'nestegg', 'hub', 'password', '--password-file', '/run/hub-admin-password')
    ok('CLI password change revokes live sessions', not request('/api/hub/v1/session')['logged_in'])
    print(f'== result: {passed} passed, 0 failed', flush=True)
finally:
    if serve is not None:
        serve.terminate()
    for container in (hub, pbs):
        run(['docker', 'rm', '-f', container], check=False)
    run(['docker', 'network', 'rm', net], check=False)
    if serve is not None:
        serve.wait(timeout=15)
