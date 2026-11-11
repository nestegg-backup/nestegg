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
    body = {'target': 'stick', 'stick_uuid': uuid, 'snapshot_time': timestamp, 'archive': 'system.pxar',
            'paths': ['/etc/shadow'], 'target_dir': '/home/alice/refused-system-files'}
    status, value = request('POST', '/restores', body)
    assert status == 403, (status, value)
    assert not Path(body['target_dir']).exists()
    body['paths'] = ['/home/alice/notes.txt']
    body['target_dir'] = '/home/alice/restored-stick-files'
    status, value = request('POST', '/restores', body)
    assert status == 202, (status, value)
    assert value['target'] == 'stick' and value['stick_uuid'] == uuid, value
    job = value['id']
    deadline = time.monotonic() + 600
    while value['state'] == 'running' and time.monotonic() < deadline:
        time.sleep(.5)
        status, value = request('GET', '/restores/' + job)
        assert status == 200, (status, value)
        assert value['target'] == 'stick' and value['stick_uuid'] == uuid, value
    assert value['state'] == 'succeeded', value
    status, live = request('GET', '/status')
    assert status == 200 and live['targets']['stick']['sticks'][uuid]['in_use'] is None, live
    status, missing = request('GET', '/snapshots/2000-01-01T00:00:00Z/browse?' + query)
    assert status == 503 and missing['reason'] == 'snapshot_not_on_stick', (status, missing)
    files = list(Path(body['target_dir']).rglob('notes.txt'))
    assert len(files) == 1 and files[0].read_text() == 'hello from alice\n', files
    print('PASS desktop stick API: list, scoped browse, refused system files, own-file restore')


if __name__ == '__main__':
    main()
