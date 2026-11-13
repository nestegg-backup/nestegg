#!/usr/bin/env python3
"""VM-only API acceptance, executed as an ordinary group member."""
import http.client
import json
import os
import threading
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
    assert value['target'] == 'stick' and value['stick_uuid'], value
    uuid = value['stick_uuid']
    status, target_status = request('GET', '/status')
    stick = target_status['targets']['stick']
    assert uuid in stick['plugged_in_uuids'], stick
    assert stick['sticks'][uuid]['in_use'] is None, stick
    snapshot = value['snapshots'][0]
    timestamp = snapshot['snapshot'].split('/', 2)[2]
    query = urlencode({'target': 'stick', 'stick_uuid': uuid, 'archive': 'system.pxar', 'path': '/etc'})
    status, value = request('GET', '/snapshots/' + timestamp + '/browse?' + query)
    assert status == 403, (status, value)
    query = urlencode({'target': 'stick', 'stick_uuid': uuid, 'archive': 'system.pxar', 'path': '/home/alice'})
    gate = os.environ.get('NESTEGG_TEST_BROWSE_GATE')
    if gate:
        gate = Path(gate)
        (gate / 'pause').touch()
        result = []
        worker = threading.Thread(target=lambda: result.append(request('GET', '/snapshots/' + timestamp + '/browse?' + query)))
        worker.start()
        try:
            deadline = time.monotonic() + 120
            while not (gate / 'busy').exists() and time.monotonic() < deadline:
                time.sleep(.1)
            assert (gate / 'busy').exists(), 'browse worker did not reach catalog'
            status, live = request('GET', '/status')
            assert status == 200, live
            assert live['targets']['stick']['in_use'] == 'restore', live
            assert live['targets']['stick']['sticks'][uuid]['in_use'] == 'restore', live
            status, busy = request('GET', '/snapshots?target=stick&stick_uuid=' + uuid)
            assert status == 503 and busy['reason'] == 'busy', (status, busy)
            print('PASS live browse usage and competing request busy reason')
        finally:
            (gate / 'pause').unlink(missing_ok=True)
            worker.join(240)
        assert not worker.is_alive() and result, 'browse did not finish'
        status, value = result[0]
    else:
        status, value = request('GET', '/snapshots/' + timestamp + '/browse?' + query)
    assert status == 200 and value['entries'], (status, value)
    assert value['target'] == 'stick' and value['stick_uuid'] == uuid, value
    other = urlencode({'target': 'stick', 'stick_uuid': uuid, 'archive': 'system.pxar', 'path': '/home/bob'})
    status, refused = request('GET', '/snapshots/' + timestamp + '/browse?' + other)
    assert status == 403, (status, refused)
    body = {'target': 'stick', 'stick_uuid': uuid, 'snapshot_time': timestamp, 'archive': 'system.pxar',
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
        value = finish(value, uuid)
        assert value['state'] == 'failed', value
    else:
        assert status in (403, 409), (status, value)
    # Absence inside bob's 0700 home is checked as root by the caller.
    for target in ['/tmp/stick-restore-outside-home', '/home/alice/restored-stick-files']:
        body['target_dir'] = target
        status, value = request('POST', '/restores', body)
        assert status == 202, (status, value)
        assert value['target'] == 'stick' and value['stick_uuid'] == uuid, value
        value = finish(value, uuid)
        assert value['state'] == 'succeeded', value
        files = list(Path(target).rglob('notes.txt'))
        assert len(files) == 1 and files[0].read_text() == 'hello from alice\n', files
        for item in [Path(target), *Path(target).rglob('*')]:
            assert item.lstat().st_uid == os.getuid(), item
    # The signed source is below the home: /report.txt is the archive path,
    # and the restore target must not acquire home/alice/Documents prefixes.
    docs = urlencode({'target': 'stick', 'stick_uuid': uuid, 'archive': 'docs.pxar', 'path': '/'})
    status, value = request('GET', '/snapshots/' + timestamp + '/browse?' + docs)
    assert status == 200 and any(e['name'] == 'report.txt' for e in value['entries']), (status, value)
    traversal = urlencode({'target': 'stick', 'stick_uuid': uuid, 'archive': 'docs.pxar', 'path': '/../etc'})
    status, value = request('GET', '/snapshots/' + timestamp + '/browse?' + traversal)
    assert status == 400, (status, value)
    for paths, target in [([], '/home/alice/stick-docs-full'), (['/report.txt'], '/home/alice/stick-docs-file')]:
        status, value = request('POST', '/restores', {
            'target': 'stick', 'stick_uuid': uuid, 'snapshot_time': timestamp,
            'archive': 'docs.pxar', 'paths': paths, 'target_dir': target})
        assert status == 202, (status, value)
        value = finish(value, uuid)
        assert value['state'] == 'succeeded', value
        assert (Path(target) / 'report.txt').read_text() == 'document from alice\n'
        assert not (Path(target) / '.pxarexclude-cli').exists()
        for item in [Path(target), *Path(target).rglob('*')]:
            assert item.lstat().st_uid == os.getuid(), item
    print('PASS home-subdirectory stick archive: browse and whole/selected restore, root mapping, identity, no client exclude record')
    status, live = request('GET', '/status')
    assert status == 200 and live['targets']['stick']['sticks'][uuid]['in_use'] is None, live
    status, missing = request('GET', '/snapshots/2000-01-01T00:00:00Z/browse?' + query)
    assert status == 503 and missing['reason'] == 'snapshot_not_on_stick', (status, missing)
    print('PASS desktop stick API: list, scoped browse, refused system/other-user files, '
          'other-user target fails, own-file restore into home and /tmp as the caller')


def finish(value, uuid):
    job = value['id']
    deadline = time.monotonic() + 600
    while value['state'] == 'running' and time.monotonic() < deadline:
        time.sleep(.5)
        status, value = request('GET', '/restores/' + job)
        assert status == 200, (status, value)
        assert value['target'] == 'stick' and value['stick_uuid'] == uuid, value
    return value


if __name__ == '__main__':
    main()
