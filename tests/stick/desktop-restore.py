#!/usr/bin/env python3
"""VM-only API acceptance, executed as an ordinary group member."""
import http.client
import json
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
    body = {'target': 'stick', 'snapshot_time': timestamp, 'archive': 'system.pxar',
            'paths': ['/etc/shadow'], 'target_dir': '/home/alice/refused-system-files'}
    status, value = request('POST', '/restores', body)
    assert status == 403, (status, value)
    assert not Path(body['target_dir']).exists()
    body['paths'] = ['/home/alice/notes.txt']
    body['target_dir'] = '/home/alice/restored-stick-files'
    status, value = request('POST', '/restores', body)
    assert status == 202, (status, value)
    job = value['id']
    deadline = time.monotonic() + 600
    while value['state'] == 'running' and time.monotonic() < deadline:
        time.sleep(.5)
        status, value = request('GET', '/restores/' + job)
        assert status == 200, (status, value)
    assert value['state'] == 'succeeded', value
    files = list(Path(body['target_dir']).rglob('notes.txt'))
    assert len(files) == 1 and files[0].read_text() == 'hello from alice\n', files
    print('PASS desktop stick API: list, scoped browse, refused system files, own-file restore')


if __name__ == '__main__':
    main()
