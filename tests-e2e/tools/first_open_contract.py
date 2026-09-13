"""Real HTTP first-open fan-in after a durable EnsureBuilder is observable."""
from concurrent.futures import ThreadPoolExecutor
import json
import sqlite3
import time
import threading
import urllib.error
import urllib.parse
import urllib.request
import uuid

ENTRIES = ('pod-ensure', 'workspace-v1', 'workspace-v2', 'file-list')


def multipart(fields):
    boundary = 'rcoder-' + uuid.uuid4().hex
    data = ''.join('--' + boundary + '\r\nContent-Disposition: form-data; name="' + name + '"\r\n\r\n' + value + '\r\n'
                   for name, value in fields.items()) + '--' + boundary + '--\r\n'
    return data.encode(), 'multipart/form-data; boundary=' + boundary


def operations(database, app):
    with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True, timeout=1) as connection:
        return [json.loads(row[0]) for row in connection.execute(
            'SELECT record FROM userapp_operations WHERE app_id=?', (app,))]


def exercise(base, app, user, database):
    followers_ready = threading.Barrier(len(ENTRIES), timeout=10)

    def request(entry):
        if entry != "leader":
            followers_ready.wait()
        headers = {'X-App-Id': app, 'X-Service-Type': 'userapp', 'X-User-Id': user}
        if entry == 'leader':
            path = '/api/v1/userapp/workspace'
            data, content = json.dumps({'app_id': app, 'user_id': user}).encode(), 'application/json'
        elif entry == 'pod-ensure':
            path = '/computer/pod/ensure'
            data, content = json.dumps({'app_id': app, 'user_id': user, 'app_stage': 'dev', 'service_type': 'userapp'}).encode(), 'application/json'
        elif entry in ('workspace-v1', 'workspace-v2'):
            path = '/api/computer/create-workspace' + ('-v2' if entry.endswith('v2') else '')
            data, content = multipart({'userId': user, 'cId': 'first-open', 'serviceType': 'userapp', 'appId': app})
        else:
            path = '/api/computer/get-file-list?' + urllib.parse.urlencode({'userId': user, 'cId': 'first-open', 'recursive': 'false'})
            data, content = None, 'application/json'
        headers['Content-Type'] = content
        started = time.monotonic()
        with urllib.request.urlopen(urllib.request.Request(base + path, data=data, headers=headers), timeout=120) as response:
            body = json.load(response)
            success = body.get('code') == '0000' if 'code' in body else body.get('success') is True
            if response.status != 200 or not success:
                raise RuntimeError('first-open ' + entry + ' failed: ' + str(body.get('code')))
            return {'entry': entry, 'http_status': response.status,
                    'elapsed_seconds': time.monotonic() - started, 'success': success}

    def admitted_operation():
        # 受理窗口观察走 HTTP。宿主直读 bind 挂载的 SQLite 与容器内 WAL/shm
        # mmap 跨 OS 不相干——写侧 checkpoint 截断可致容器 SIGBUS（实测 Exit 135）。
        # 数据库直读仅保留在全部请求静止后的终态校验。
        url = base + '/api/v1/userapp/' + app + '/operations/current?user_id=' + urllib.parse.quote(user)
        try:
            with urllib.request.urlopen(url, timeout=5) as response:
                body = json.load(response)
        except urllib.error.HTTPError:
            return None
        if body.get('code') != '0000' or not body.get('data'):
            return None
        operation = body['data']
        if operation.get('kind') != 'EnsureBuilder' or operation.get('state') not in ('Pending', 'Running'):
            return None
        return operation

    # Observe admission before querying a read-only route. A query before any
    # operation exists may legitimately return not-found and must not create it.
    with ThreadPoolExecutor(max_workers=5) as pool:
        leader = pool.submit(request, 'leader')
        deadline = time.monotonic() + 20
        accepted = None
        while time.monotonic() < deadline:
            pending = admitted_operation()
            if pending is not None:
                accepted = pending
                break
            if leader.done():
                leader.result()  # preserve a concrete HTTP error when available
                raise RuntimeError('first-open did not expose the required in-flight operation window')
            time.sleep(0.05)
        if accepted is None:
            raise RuntimeError('first-open durable acceptance was not observed')
        followers = [pool.submit(request, entry) for entry in ENTRIES]
        results = [leader.result()] + [future.result() for future in followers]
    # 静默期（全部请求终态、写侧停止）后再直读数据库做终态对账。
    time.sleep(1)
    completed = [op for op in operations(database, app) if op['kind'] == 'EnsureBuilder']
    if (len(completed) != 1 or completed[0]['operation_id'] != accepted['operation_id']
            or completed[0]['state'] != 'Succeeded'):
        raise RuntimeError('first-open requests did not converge on the original builder operation')
    return {'operation_id': accepted['operation_id'], 'lifecycle_id': accepted['lifecycle_id'],
            'entries': results, 'evidence_level': 'single_replica_real_http'}
