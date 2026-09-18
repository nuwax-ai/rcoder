"""Real HTTP first-open fan-in after a durable EnsureBuilder is observable."""
from concurrent.futures import ThreadPoolExecutor
import json
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


def terminal_operation(base, app, user, operation_id):
    # 终态对账走 HTTP（T4：SQLite 引擎不读 Turso 活库——受理窗口观察与
    # 终态校验同一约束，跨引擎 mmap 并发读已被 SIGBUS 实测排除）。
    with urllib.request.urlopen(base + '/api/v1/userapp/' + app + '/operations/'
                                + urllib.parse.quote(operation_id) + '?user_id=' + urllib.parse.quote(user),
                                timeout=30) as response:
        body = json.load(response)
    if body.get('code') != '0000' or not body.get('data'):
        raise RuntimeError('terminal operation query failed: ' + str(body.get('code')))
    return body['data']


def exercise(base, app, user):
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
        # operations/current 返回全部在途槽位（application/dev/prod）——筛出
        # 目标 EnsureBuilder 在途操作
        for operation in body['data']:
            if operation.get('kind') == 'EnsureBuilder' and operation.get('state') in ('Pending', 'Running'):
                return operation
        return None

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
    # 终态对账走 HTTP（存储层内容对账由调用方的离线快照完成）。
    time.sleep(1)
    completed = terminal_operation(base, app, user, accepted['operation_id'])
    if (completed['operation_id'] != accepted['operation_id']
            or completed['kind'] != 'EnsureBuilder'
            or completed['state'] != 'Succeeded'):
        raise RuntimeError('first-open requests did not converge on the original builder operation')
    return {'operation_id': accepted['operation_id'], 'lifecycle_id': accepted['lifecycle_id'],
            'entries': results, 'evidence_level': 'single_replica_real_http'}
