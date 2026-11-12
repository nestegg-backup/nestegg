#!/usr/bin/env python3
"""VM-only API acceptance, executed as an ordinary group member."""
import http.client
import json
import os
from pathlib import Path
import socket
import time
from urllib.parse import urlencode


class UnixHTTP(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect('/run/desktop-api/api.sock')


def request(method, path, body=None):
    connection = UnixHTTP('localhost', timeout=240)
    connection.request(method, '/api/v1' + path,
                       None if body is None else json.dumps(body),
                       {} if body is None else {'Content-Type': 'application/json'})
    response = connection.getresponse()
    value = json.loads(response.read())
    status = response.status
    connection.close()
    return status, value


def main():
    status, value = request('GET', '/snapshots?target=stick')
    assert status == 200 and value['snapshots'], (status, value)
    snapshot = value['snapshots'][0]
    timestamp = snapshot['snapshot'].split('/', 2)[2]
    query = urlencode({'target': 'stick', 'archive': 'system.pxar', 'path': '/etc'})
    status, value = request('GET', '/snapshots/' + timestamp + '/browse?' + query)
    assert status == 403, (status, value)
    query = urlencode({'target': 'stick', 'archive': 'system.pxar', 'path': '/home/alice'})
    status, value = request('GET', '/snapshots/' + timestamp + '/browse?' + query)
    assert status == 200 and value['entries'], (status, value)
    query = urlencode({'target': 'stick', 'archive': 'system.pxar', 'path': '/home/bob'})
    status, value = request('GET', '/snapshots/' + timestamp + '/browse?' + query)
    assert status == 403, (status, value)
    body = {'target': 'stick', 'snapshot_time': timestamp, 'archive': 'system.pxar',
            'paths': ['/etc/shadow'], 'target_dir': '/home/alice/refused-system-files'}
    status, value = request('POST', '/restores', body)
    assert status == 403, (status, value)
    assert not Path(body['target_dir']).exists()
    body['paths'] = ['/home/bob/private.txt']
    body['target_dir'] = '/home/alice/refused-other-user'
    status, value = request('POST', '/restores', body)
    assert status == 403, (status, value)
    assert not Path(body['target_dir']).exists()
    # Only the restored PATHS are confined to the caller's home. The target
    # may be anywhere the caller can write: the worker creates it with the
    # caller's identity (decision 0007; tests/e2e/systemd.sh uses /tmp).
    body['paths'] = ['/home/alice/notes.txt']
    body['target_dir'] = '/home/bob/refused-target'
    status, value = request('POST', '/restores', body)
    if status == 202:
        value = finish(value)
        assert value['state'] == 'failed', value
    else:
        assert status in (403, 409), (status, value)
    # Absence inside bob's 0700 home is checked as root by the caller.
    for target in ['/tmp/stick-restore-outside-home', '/home/alice/restored-stick-files']:
        body['target_dir'] = target
        status, value = request('POST', '/restores', body)
        assert status == 202, (status, value)
        value = finish(value)
        assert value['state'] == 'succeeded', value
        files = list(Path(target).rglob('notes.txt'))
        assert len(files) == 1 and files[0].read_text() == 'hello from alice\n', files
        for item in [Path(target), *Path(target).rglob('*')]:
            assert item.lstat().st_uid == os.getuid(), item
    print('PASS desktop stick API: list, scoped browse, refused system/other-user files, '
          'other-user target fails, own-file restore into home and /tmp as the caller')


def finish(value):
    job = value['id']
    deadline = time.monotonic() + 600
    while value['state'] == 'running' and time.monotonic() < deadline:
        time.sleep(.5)
        status, value = request('GET', '/restores/' + job)
        assert status == 200, (status, value)
    return value

if __name__ == '__main__':
    main()
