"""Exercise the shipped bootstrap, real systemd, package, hub and PBS together.

All credentials and bootstrap commands stay in captured output/stdin or private
container files. Never print subprocess arguments/output containing secrets.
"""
import html.parser
import json
import os
import pathlib
import pty
import re
import secrets
import select
import signal
import subprocess
import sys
import time
import urllib.parse

HERE = pathlib.Path(__file__).resolve().parent
PACKAGE = pathlib.Path(sys.argv[1])
PREFIX = sys.argv[2]
PBS, HUB, FIRST, SECOND = (PREFIX + '-' + s for s in ('pbs', 'hub', 'desktop', 'portable'))
NET = PREFIX + '-net'
URL = 'https://hub.example.com:8443'
MASTER_PASSWORD = secrets.token_hex(24)
PIN = None
passed = 0
started = time.monotonic()


def run(args, data=None, check=True, timeout=600):
    result = subprocess.run(args, input=data, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=timeout)
    if check and result.returncode:
        # The upstream program may echo credentials. Report only the operation.
        operation = ' '.join(args[4:6]) if args[:3] == ['docker', 'exec', '-i'] else args[0]
        raise RuntimeError(f'{operation} failed (exit {result.returncode}); output withheld')
    return result


def dx(container, *args, **kw):
    return run(['docker', 'exec', '-i', container, *args], **kw)


def write(container, path, data):
    dx(container, 'sh', '-c', 'umask 077; cat > "$1"', 'write', path,
       data=data if isinstance(data, bytes) else data.encode())


def ok(label, condition=True):
    global passed
    if not condition:
        raise AssertionError(label)
    passed += 1
    print('  ok  ' + label, flush=True)


def terminal(container, args, answers):
    """Give the official client's password prompts their own controlling TTY."""
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp('docker', ['docker', 'exec', '-it', container, *args])
    pending, prompts = b'', 0
    deadline = time.monotonic() + 120
    try:
        while time.monotonic() < deadline:
            if select.select([fd], [], [], .1)[0]:
                try:
                    part = os.read(fd, 8192)
                except OSError:
                    break
                if not part:
                    break
                pending = (pending + part)[-65536:]
                if pending.lower().rstrip().endswith(b':') and b'password' in pending.lower():
                    if prompts >= len(answers):
                        raise RuntimeError('unexpected password prompt')
                    os.write(fd, answers[prompts].encode() + b'\n')
                    prompts += 1
                    pending = b''
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                pid = None
                if os.waitstatus_to_exitcode(status):
                    raise RuntimeError('terminal operation failed; output withheld')
                if prompts != len(answers):
                    raise RuntimeError('expected password prompts were not seen')
                return
        if pid:
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                pid = None
                if os.waitstatus_to_exitcode(status) == 0 and prompts == len(answers):
                    return
        raise RuntimeError('terminal operation failed or timed out; output withheld')
    finally:
        os.close(fd)
        if pid:
            os.kill(pid, signal.SIGTERM)
            os.waitpid(pid, 0)


def wait_for(label, probe, seconds=120):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if probe():
            return
        time.sleep(.5)
    raise AssertionError(label + ' timed out')


def request(path, method='GET', body=None, wanted=200, headers=False, browser=False):
    args = ['curl', '-sS', '-k', '--pinnedpubkey', PIN, '--max-time', '60',
            '-w', '\n%{http_code}', '-D', '/run/response-headers', '-X', method,
            '-b', '/run/hub-cookie', '-c', '/run/hub-cookie']
    if method != 'GET':
        args += ['-H', 'X-Nestegg-Hub: 1']
    if browser:
        args += ['-H', 'Origin: ' + URL, '-H', 'Sec-Fetch-Site: same-origin']
    if body is not None:
        args += ['-H', 'Content-Type: application/json', '--data-binary', '@-']
    result = dx(HUB, 'sh', '-c', 'umask 077; exec "$@"', 'request', *args, URL + path,
                data=None if body is None else json.dumps(body).encode())
    payload, _, status = result.stdout.rpartition(b'\n')
    if status != str(wanted).encode():
        parsed = json.loads(payload) if payload.startswith(b'{') else {}
        raise AssertionError(f'{method} {path}: HTTP {status.decode()}, code {parsed.get("code")}')
    if headers:
        return payload, dx(HUB, 'cat', '/run/response-headers').stdout.decode()
    return json.loads(payload) if payload else None


def as_user(container, command):
    # A new login session resolves supplementary groups after usermod.
    # Commands/codes are passed over stdin, not printed by the harness.
    return dx(container, 'su', '-', 'alice', '-s', '/bin/bash', '-c',
              'exec bash -o pipefail', data=(command + '\n').encode(), check=False)


def signature(container):
    return dx(container, 'python3', '-c', '''
import hashlib,json,pathlib,stat,subprocess
files=[]
for base in ['/etc/nestegg','/var/lib/nestegg/state.json']:
 p=pathlib.Path(base)
 for f in sorted(p.rglob('*')) if p.is_dir() else [p]:
  if f.is_file():
   s=f.stat(); files.append((str(f),stat.S_IMODE(s.st_mode),s.st_uid,s.st_gid,hashlib.sha256(f.read_bytes()).hexdigest()))
print(json.dumps([files,subprocess.check_output(['dpkg-query','-W']).decode(),subprocess.check_output(['getent','group','nestegg']).decode(),subprocess.check_output(['systemctl','show','nestegg.service','nestegg-backup.timer','nestegg-notify.timer','-p','UnitFileState','-p','ActiveState']).decode()]))
''').stdout


def reject(container, command, message):
    before = signature(container)
    result = as_user(container, command)
    ok('refusal: ' + message, result.returncode != 0 and
       message.encode() in result.stdout + result.stderr)
    ok('refusal leaves configuration, secrets, package, group and units unchanged',
       signature(container) == before)


def check_mode(container, path, expected):
    ok(path + ' permissions ' + expected,
       dx(container, 'stat', '-c', '%a %U:%G', path).stdout.decode().strip() == expected)


def backup_finished(container, previous_run=None):
    result = dx(container, 'cat', '/var/lib/nestegg/state.json', check=False)
    if result.returncode:
        return False
    state = json.loads(result.stdout)
    last = state.get('last_run')
    if last and last['state'] in ('failed', 'cancelled'):
        error = last.get('error') or {}
        # Error kinds are safe; messages could contain upstream echo.
        raise AssertionError('join-started backup failed: ' + str(error.get('kind')))
    return (last and last['state'] == 'succeeded' and last['id'] != previous_run
            and not state.get('current_run'))


def machines():
    return request('/api/hub/v1/machines')


def check_join(container, name, previous_count=0, previous_run=None):
    ok(name + ': official backup client installed',
       dx(container, 'dpkg-query', '-W', '-f=${Status}', 'proxmox-backup-client-static').stdout == b'install ok installed')
    ok(name + ': nestegg package installed',
       dx(container, 'dpkg-query', '-W', '-f=${Status}', 'nestegg').stdout == b'install ok installed')
    check_mode(container, '/etc/nestegg/config.toml', '640 root:nestegg')
    check_mode(container, '/etc/nestegg/credentials', '700 root:root')
    for credential in ('password', 'machine-credential', 'encryption-key'):
        # Real systemd seals credentials; no plain fallback on the computers.
        check_mode(container, '/etc/nestegg/credentials/' + credential + '.cred', '600 root:root')
        ok('no plaintext ' + credential,
           dx(container, 'test', '!', '-e', '/etc/nestegg/credentials/' + credential).returncode == 0)
    check_mode(container, '/etc/nestegg/master-public.pem', '644 root:root')
    for unit in ('nestegg.service', 'nestegg-backup.timer', 'nestegg-notify.timer'):
        ok(unit + ' enabled', dx(container, 'systemctl', 'is-enabled', '--quiet', unit).returncode == 0)
        ok(unit + ' active', dx(container, 'systemctl', 'is-active', '--quiet', unit).returncode == 0)
    ok('calling user joined nestegg group',
       b'nestegg' in dx(container, 'id', '-nG', 'alice').stdout.split())
    print('== waiting for the backup started by join (no extra start command)', flush=True)
    wait_for('first backup', lambda: backup_finished(container, previous_run), 180)
    ok('join alone starts and finishes a successful first backup')
    status_result = as_user(container, 'nestegg status --json')
    ok('status works in a new ordinary-user login', status_result.returncode == 0)
    status = json.loads(status_result.stdout)
    ok('ordinary-user status has successful backup', status['last_success']['state'] == 'succeeded')
    ok('ordinary-user status has master-key coverage', status['encryption']['master_key'] is not None)
    ok('ordinary-user status is hub managed', status['hub']['name'] == 'Home')
    text = as_user(container, 'nestegg status').stdout.decode()
    ok('human status explains household protection and hub',
       'protected by the proven household master key' in text and 'Hub:          Home' in text)
    # Docker's overlay root is intentionally unsupported for bare-metal restore.
    # The home backup must succeed, with the incomplete system image disclosed.
    ok('container system-image limitation is reported, not hidden',
       any('system' in s.lower() for s in status['last_success']['incomplete']))
    snapshots = json.loads(dx(container, 'nestegg', 'snapshots', '--json').stdout)
    snapshot = snapshots[0]['snapshot']
    ok('snapshot is in the configured namespace', 'household' in
       dx(container, 'cat', '/etc/nestegg/config.toml').stdout.decode())
    listing = machines()
    machine = next(m for m in listing['machines'] if m['backup_id'] == name)
    ok('hub lists real snapshot count and time', listing['pbs_reachable'] and
       machine['snapshot_count'] > previous_count and machine['last_snapshot'] is not None)
    ok('hub reads computer status report', machine['report'] is not None and
       machine['report']['result'] == 'succeeded')
    group = dx(PBS, 'cat', f'/datastore/ns/household/host/{name}/owner').stdout.decode().strip()
    ok('PBS group owned by exact computer token', group == 'backup@pbs!' + name)
    # Snapshot filenames are visible to the PBS without the computer key.
    ok('snapshot contains master recovery envelope',
       dx(PBS, 'test', '-f', '/datastore/ns/household/' + snapshot + '/rsa-encrypted.key.blob').returncode == 0)
    return machine, snapshot


class References(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.assets = set()

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag in ('script', 'link'):
            ref = attrs.get('src') if tag == 'script' else attrs.get('href')
            if ref and not urllib.parse.urlsplit(ref).scheme and not ref.startswith('#'):
                self.assets.add(urllib.parse.urljoin('/', ref))


try:
    print('== disposable PBS, packaged unprivileged hub and fresh systemd clients', flush=True)
    run(['docker', 'network', 'create', NET])
    run(['docker', 'run', '-d', '--name', PBS, '--hostname', 'pbs', '--network', NET,
         '--network-alias', 'pbs.example.com', '--tmpfs', '/run', PREFIX + '-pbs'])
    run(['docker', 'run', '-d', '--name', HUB, '--network', NET,
         '--network-alias', 'hub.example.com', '--tmpfs', '/run',
         '-v', str(PACKAGE) + ':/run/nestegg.deb:ro', PREFIX + '-hub', 'sleep', 'infinity'])
    wait_for('PBS startup', lambda: dx(PBS, 'curl', '-skf',
             'https://127.0.0.1:8007/api2/json/ping', check=False).returncode == 0)
    admin_password, hub_password = secrets.token_hex(32), secrets.token_hex(24)
    dx(PBS, 'chpasswd', data=('root:' + admin_password + '\n').encode())
    dx(PBS, 'mkdir', '-p', '/datastore')
    dx(PBS, 'proxmox-backup-manager', 'datastore', 'create', 'store1', '/datastore')
    fp = re.search(r'Fingerprint \(sha256\): (.*)',
                   dx(PBS, 'proxmox-backup-manager', 'cert', 'info').stdout.decode()).group(1).strip()
    dx(HUB, 'sh', '-c', 'apt-get update >/run/install.log 2>&1 && DEBIAN_FRONTEND=noninteractive apt-get install -y /run/nestegg.deb >>/run/install.log 2>&1')
    write(HUB, '/run/pbs-admin-password', admin_password)
    write(HUB, '/run/hub-admin-password', hub_password)
    terminal(HUB, ['nestegg', 'hub', 'init', '--name', 'Home', '--url', URL,
                  '--server', 'pbs.example.com', '--datastore', 'store1', '--namespace', 'household',
                  '--user', 'backup@pbs', '--fingerprint', fp, '--prepare-pbs', '--pbs-admin', 'root@pam',
                  '--pbs-admin-password-file', '/run/pbs-admin-password',
                  '--admin-password-file', '/run/hub-admin-password', '--allow-plain',
                  '--paperkey', '/run/master.html', '--defer-confirm', '--package', '/run/nestegg.deb'],
             [MASTER_PASSWORD, MASTER_PASSWORD])
    PIN = re.search(r'^Pin: (.*)$', dx(HUB, 'nestegg', 'hub', 'status').stdout.decode(), re.M).group(1)
    # Administrator's saved PEM, before proof deletes the private hub copy.
    master_copy = dx(HUB, 'cat', '/var/lib/nestegg-hub/master/master-private.pem').stdout
    dx(HUB, 'cp', '/run/master.html', '/run/master-copy.html')
    dx(HUB, 'nestegg', 'hub', 'master-key', 'confirm', '--file', '/run/master-copy.html', '--accept-local-copy')
    ok('hub initialized with PBS preparation, package and master proof')
    ok('private master removed after proof', dx(HUB, 'test', '!', '-e',
       '/var/lib/nestegg-hub/master/master-private.pem').returncode == 0)
    # Same credential delivery as hub.sh; the server really runs unprivileged.
    dx(HUB, 'mkdir', '-m', '700', '/run/hub-credentials')
    if dx(HUB, 'test', '-f', '/etc/nestegg-hub/credentials/password.cred', check=False).returncode == 0:
        dx(HUB, 'systemd-creds', 'decrypt', '--name=password',
           '/etc/nestegg-hub/credentials/password.cred', '/run/hub-credentials/password')
    else:
        dx(HUB, 'cp', '/etc/nestegg-hub/credentials/password', '/run/hub-credentials/password')
    dx(HUB, 'cp', '/etc/nestegg-hub/tls.key', '/run/hub-credentials/hub-tls-key')
    dx(HUB, 'chown', '-R', 'nestegg-hub:nestegg-hub', '/run/hub-credentials')
    run(['docker', 'exec', '-d', '-u', 'nestegg-hub', '-e',
         'CREDENTIALS_DIRECTORY=/run/hub-credentials', HUB, 'nestegg', 'hub', 'serve'])
    wait_for('hub startup', lambda: dx(HUB, 'curl', '-skf', '--pinnedpubkey', PIN, URL + '/', check=False).returncode == 0)
    request('/api/hub/v1/session', 'POST', {'password': hub_password}, wanted=204, browser=True)
    ok('browser-style cookie session without custom GET header',
       request('/api/hub/v1/session', browser=True)['logged_in'])
    cookie = dx(HUB, 'cat', '/run/hub-cookie').stdout
    ok('cookie is Secure and HttpOnly', b'#HttpOnly_' in cookie and b'\tTRUE\t' in cookie)
    page, headers = request('/', headers=True)
    ok('root returns the exact built hub interface', page == (HERE / '../../ui/dist/hub.html').resolve().read_bytes())
    page_csp = next(h.split(':', 1)[1] for h in headers.splitlines() if h.lower().startswith('content-security-policy:'))
    refs = References()
    refs.feed(page.decode())
    ok('built page references JavaScript and CSS', any(p.endswith('.js') for p in refs.assets)
       and any(p.endswith('.css') for p in refs.assets))
    # Check transitive bundled imports/fonts too, not only the entry points.
    refs.assets.update('/assets/' + p.name for p in (HERE / '../../ui/dist/assets').resolve().iterdir() if p.is_file())
    for path in sorted(refs.assets):
        asset, asset_headers = request(path, headers=True)
        ok('static asset loads: ' + path, bool(asset))
        csp = page_csp
        ok('asset carries the same CSP as the page', any(
            h.lower().startswith('content-security-policy:') and h.split(':', 1)[1] == csp
            for h in asset_headers.splitlines()))
        directive = ('script-src' if path.endswith('.js') else
                     'font-src' if path.endswith('.woff2') else 'style-src')
        policy = {parts[0]: parts[1:] for item in csp.split(';') if (parts := item.split())}
        ok('CSP permits same-origin ' + directive,
           "'self'" in policy.get(directive, policy.get('default-src', [])))
    for container, hostname in ((FIRST, 'desktop'), (SECOND, 'portable')):
        run(['docker', 'run', '-d', '-t', '--name', container, '--hostname', hostname,
             '--network', NET, '--cgroupns=host', '--cap-add', 'SYS_ADMIN',
             '--security-opt', 'seccomp=unconfined', '--security-opt', 'apparmor=unconfined',
             '-v', '/sys/fs/cgroup:/sys/fs/cgroup:rw', '--tmpfs', '/run', '--tmpfs', '/run/lock', PREFIX + '-client'])
        wait_for('systemd boot', lambda: dx(container, 'systemctl', 'is-system-running', check=False).stdout.strip()
                 in (b'running', b'degraded'))
        ok(hostname + ': starts with neither nestegg nor backup client', dx(container, 'sh', '-c',
           '! command -v nestegg && ! command -v proxmox-backup-client && test ! -e /etc/nestegg/config.toml').returncode == 0)
    code1 = request('/api/hub/v1/join-codes', 'POST', {}, wanted=201)
    print('== executing the exact API command in an ordinary login on computer 1', flush=True)
    joined = as_user(FIRST, code1['command'])
    ok('one-command bootstrap completes', joined.returncode == 0)
    ok('bootstrap package downloads stay sandboxed by apt', b'unsandboxed' not in joined.stdout + joined.stderr)
    first_machine, snapshot = check_join(FIRST, 'desktop')
    code2 = request('/api/hub/v1/join-codes', 'POST', {}, wanted=201)
    wrong_pin = 'sha256//' + 'A' * 43 + '='
    # A fresh second computer: a pin mismatch cannot install even one package.
    before = dx(SECOND, 'sh', '-c', 'dpkg-query -W; find /etc -name "*nestegg*" -o -name "*proxmox*"').stdout
    mismatch = as_user(SECOND, code2['command'].replace(PIN, wrong_pin))
    ok('wrong pin fails before bootstrap (curl exit 90)', mismatch.returncode == 90 and b'public key' in mismatch.stderr.lower())
    ok('wrong pin installs/writes nothing', before == dx(SECOND, 'sh', '-c',
       'dpkg-query -W; find /etc -name "*nestegg*" -o -name "*proxmox*"').stdout)
    print('== one-command bootstrap on computer 2 with --name', flush=True)
    joined2 = as_user(SECOND, code2['command'] + ' --name laptop')
    ok('bootstrap forwards --name', joined2.returncode == 0)
    second_machine, _ = check_join(SECOND, 'laptop')
    reject(FIRST, 'sudo nestegg join ' + URL + ' --pin ' + PIN + ' --code ' + code1['code'] + ' --replace-config',
           'this join code no longer works')
    reject(FIRST, 'sudo nestegg join ' + URL + ' --pin ' + PIN + ' --code ' + code2['code'],
           'this computer has already joined a hub')
    print('== administrator master key recovers a computer key and restores a file', flush=True)
    write(FIRST, '/root/master-private.pem', master_copy)
    dx(FIRST, 'nestegg', 'key', 'export', '/root/original.json')
    original = json.loads(dx(FIRST, 'proxmox-backup-client', 'key', 'show',
                            '/root/original.json', '--output-format', 'json').stdout)['fingerprint']
    terminal(FIRST, ['nestegg', 'key', 'recover', '--master-key', '/root/master-private.pem',
                     '--output', '/root/recovered.json'], [MASTER_PASSWORD])
    recovered = json.loads(dx(FIRST, 'proxmox-backup-client', 'key', 'show',
                             '/root/recovered.json', '--output-format', 'json').stdout)['fingerprint']
    ok('recovered fingerprint equals the computer key', original == recovered)
    check_mode(FIRST, '/root/recovered.json', '600 root:root')
    dx(FIRST, 'nestegg', 'restore', snapshot, 'home.pxar', '/root/restored', '--keyfile', '/root/recovered.json')
    ok('recovered key restores original file', dx(FIRST, 'cmp', '/home/alice/notes.txt', '/root/restored/alice/notes.txt').returncode == 0)
    key_before = dx(SECOND, 'sha256sum', '/etc/nestegg/credentials/encryption-key.cred').stdout
    dx(SECOND, 'nestegg', 'leave')
    left = machines()
    ok('leave removes hub computer', all(m['backup_id'] != 'laptop' for m in left['machines']))
    ok('leave retains backups as unmanaged', any(m['backup_id'] == 'laptop' for m in left['unmanaged']))
    tokens = dx(PBS, 'proxmox-backup-manager', 'user', 'list-tokens', 'backup@pbs', '--output-format', 'json').stdout
    ok('leave deletes PBS token', 'laptop' not in [t['tokenid'] for t in json.loads(tokens)])
    ok('leave deletes local token and machine credential', dx(SECOND, 'sh', '-c',
       'test ! -e /etc/nestegg/credentials/password.cred && test ! -e /etc/nestegg/credentials/machine-credential.cred').returncode == 0)
    request('/api/hub/v1/machines/' + first_machine['machine_id'], 'DELETE', wanted=204)
    tokens = json.loads(dx(PBS, 'proxmox-backup-manager', 'user', 'list-tokens', 'backup@pbs', '--output-format', 'json').stdout)
    ok('administrator removal deletes first PBS token', 'desktop' not in [t['tokenid'] for t in tokens])
    dx(FIRST, 'nestegg', 'run', '--trigger', 'manual', check=False)
    failed = json.loads(dx(FIRST, 'cat', '/var/lib/nestegg/state.json').stdout)['last_run']
    ok('next backup fails with a clear login error', failed['state'] == 'failed' and
       failed['error']['kind'] == 'authentication' and
       re.search(r'login|authentication|credentials|unauthorized', failed['error']['message'], re.I) is not None)
    code3 = request('/api/hub/v1/join-codes', 'POST', {}, wanted=201)
    # Today's explicit safeguards: leave keeps config and old backups, so a
    # repeat enrollment needs replacement and same-computer adoption consent.
    reject(SECOND, 'sudo nestegg join ' + URL + ' --pin ' + PIN + ' --code ' + code3['code'] + ' --name laptop',
           'this computer is already configured')
    rejoined = as_user(SECOND, code3['command'] + ' --name laptop --adopt --replace-config')
    ok('second join under old name completes with explicit adoption/replacement', rejoined.returncode == 0)
    check_join(SECOND, 'laptop', second_machine['snapshot_count'], second_machine['report']['run'])
    ok('rejoin keeps the original sealed computer key', key_before ==
       dx(SECOND, 'sha256sum', '/etc/nestegg/credentials/encryption-key.cred').stdout)
    print(f'== result: {passed} passed, 0 failed; {time.monotonic() - started:.1f} seconds', flush=True)
finally:
    for container in (FIRST, SECOND, HUB, PBS):
        run(['docker', 'rm', '-f', container], check=False)
    run(['docker', 'network', 'rm', NET], check=False)
