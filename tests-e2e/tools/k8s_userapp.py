#!/usr/bin/env python3
"""Explicit test-namespace userApp acceptance against real K8s; no Docker or LLM.

Only this invocation's UUID app can be mutated or cleaned. Reports never contain
Pod environment variables. Existing resources are inventoried before acceptance.
"""
import argparse
import concurrent.futures
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import signal
import socket
import subprocess
import time
import threading
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[1]
# 显式输入模式（remote-k8s 冻结快照）：E2E_SOURCE_ROOT 指向快照根（无 .git），
# E2E_RUN_ROOT 重定向报告根；缺省时保持活动工作目录旧行为（对齐 run.py）。
REPO = Path(os.environ.get('E2E_SOURCE_ROOT') or ROOT.parent)
ORIGIN_HEAD = os.environ.get('E2E_ORIGIN_HEAD')
REQUIRED = {'cluster_ready', 'concurrent_ensure', 'ensure_retry', 'builder_identity',
            'concurrent_error_contract', 'cross_replica_files', 'build_A', 'build_B', 'artifact_A', 'artifact_B',
            'sse_terminal', 'sse_past_terminal', 'cross_replica_cancel', 'cold_deploy',
            'content_A', 'hot_env_rejected', 'hot_failure', 'old_content_healthy',
            'hot_deploy', 'content_B', 'hot_pod_preserved', 'deploy_identity',
            'lock_holder_admitted', 'lock_busy_stop_conflict', 'lock_cross_replica_conflict',
            'lock_busy_restart_conflict', 'lock_busy_delete_conflict', 'lock_holder_still_inflight',
            'lock_holder_completes', 'lock_rejected_no_auto_execution', 'lock_no_side_effects',
            'lock_start_window_admitted', 'lock_start_waits_for_holder',
            'lock_delete_stale_version_conflict', 'lock_delete_version_keeps_resources',
            'lifecycle_active', 'stop_request_operation', 'stop_operation_by_request',
            'lifecycle_stable_stop_wake', 'request_id_replay_no_redeploy', 'request_id_by_request',
            'stop_idempotent', 'stop_zero_pods', 'wake_new_pod', 'wake', 'wake_content_B', 'cleanup',
            'tombstone_deleted', 'tombstone_old_ensure_rejected', 'recreate_new_lifecycle',
            'recreate_deleted', 'baseline_preserved'}


def compact_resource(row):
    meta = row['metadata']
    return {'kind': row.get('kind'), 'name': meta['name'], 'uid': meta['uid'],
            'resourceVersion': meta.get('resourceVersion'), 'labels': meta.get('labels', {}),
            'phase': row.get('status', {}).get('phase'),
            'podIP': row.get('status', {}).get('podIP'),
            'conditions': row.get('status', {}).get('conditions', []),
            'images': [{k: c.get(k) for k in ('name', 'image', 'imageID', 'containerID', 'ready', 'restartCount')}
                       for c in row.get('status', {}).get('containerStatuses', [])]}


class Run:
    def __init__(self, args):
        self.args = args
        self.id = uuid.uuid4().hex
        # app_id 受 USERAPP_APP_ID_MAX_LEN=22 约束（K8s StatefulSet label 63 字节限制）：
        # 'e2e-k8s-'(8) + 14 hex = 22
        self.app = 'e2e-k8s-' + self.id[:14]
        self.user = 'e2e-' + self.id[:12]
        report_base = Path(os.environ['E2E_RUN_ROOT']) if os.environ.get('E2E_RUN_ROOT') else ROOT / 'reports'
        self.root = report_base / self.id
        self.root.mkdir(parents=True)
        self.report_lock = threading.RLock()
        self.assertions = []
        self.requests = []
        self.tunnel = None
        self.baseline = []
        self.created = False
        self.lifecycle_id = None
        self.base = args.url.rstrip('/')
        self.entries = []
        self.trace = uuid.uuid4().hex
        self.summary = {'run_id': self.id, 'app_id': self.app, 'namespace': args.namespace,
                        'ssh': args.ssh, 'planned': sorted(REQUIRED), 'verdict': 'aborted',
                        'head': (ORIGIN_HEAD or self.local('git', 'rev-parse', 'HEAD')).strip(),
                        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
        from run import source_fingerprint
        self.summary['worktree_sha256'] = source_fingerprint()
        self.save('summary.json', self.summary)
        print('Report: ' + str(self.root), flush=True)

    def local(self, *args):
        return subprocess.check_output(args, cwd=REPO, text=True, timeout=90)

    def kube(self, *args):
        context = getattr(self.args, 'context', None)
        command = shlex.join(['kubectl', *(['--context', context] if context else []), '-n', self.args.namespace, *args])
        return self.local('ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', self.args.ssh, command)

    def inventory(self):
        data = json.loads(self.kube('get', 'pods,statefulsets,deployments,pvc,services,configmaps', '-o', 'json'))
        return [compact_resource(r) for r in data['items']]

    def owned(self, row):
        return self.app in row['name']

    def save(self, name, data):
        def redact(value):
            if isinstance(value, dict):
                return {k: '<redacted>' if re.search(r'token|password|secret|api_key', k, re.I) else redact(v) for k, v in value.items()}
            if isinstance(value, list):
                return [redact(v) for v in value]
            return value
        with self.report_lock:
            (self.root / name).write_text(json.dumps(redact(data), indent=2, ensure_ascii=False))

    def check(self, name, ok, detail=None, fatal=True):
        self.assertions.append({'id': name, 'ok': bool(ok), 'detail': detail})
        self.save('assertions.json', self.assertions)
        print(('PASS ' if ok else 'FAIL ') + name, flush=True)
        if not ok and fatal:
            raise AssertionError(name + ': ' + str(detail)[:1500])

    def request(self, path, body=None, base=None, headers=None, raw=False, timeout=90):
        data = body if isinstance(body, bytes) else json.dumps(body).encode() if body is not None else None
        hdr = {'traceparent': '00-' + self.trace + '-' + uuid.uuid4().hex[:16] + '-01'}
        if body is not None and not isinstance(body, bytes):
            hdr['Content-Type'] = 'application/json'
        hdr.update(headers or {})
        req = urllib.request.Request((base or self.base) + path, data=data, headers=hdr)
        started = time.monotonic()
        try:
            response = urllib.request.urlopen(req, timeout=timeout)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            content = response.read()
            status = response.status
        result = content if raw else json.loads(content)
        row = {'path': path, 'status': status, 'elapsed': time.monotonic() - started,
               'base': base or self.base, 'trace_id': self.trace}
        if not raw:
            row['response'] = result
        else:
            row.update(size=len(content), sha256=hashlib.sha256(content).hexdigest())
        self.requests.append(row)
        self.save('requests.json', self.requests)
        return status, result

    def api(self, path, body=None, base=None, timeout=90):
        status, data = self.request(path, body, base, timeout=timeout)
        if status != 200 or data.get('code') != '0000':
            raise AssertionError(f'{path}: HTTP {status}: {data}')
        return data['data']

    def api_retry_conflict(self, path, body=None, timeout=180, attempts=40, interval=10):
        """快失败语义的调用方重试模式：锁被进行中操作（如仍在收敛的后台
        部署协调器——start 等就绪预算可达 30 分钟）持有时返回 ERR_CONFLICT
        ——等待后重试（有界），其余业务错误立即上抛。"""
        last = None
        for _ in range(attempts):
            status, envelope = self.request(path, body, timeout=timeout)
            if status == 200 and envelope.get('code') == '0000':
                return envelope['data']
            if status == 200 and envelope.get('code') == 'ERR_CONFLICT':
                last = envelope
                time.sleep(interval)
                continue
            raise AssertionError(f'{path}: HTTP {status}: {envelope}')
        raise AssertionError(f'{path}: lock still held after {attempts} retries: {last}')

    def poll(self, action, predicate, budget=180):
        deadline = time.monotonic() + budget
        value = None
        while time.monotonic() < deadline:
            value = action()
            if predicate(value):
                return value
            time.sleep(2)
        raise TimeoutError('Polling deadline: ' + str(value)[:1000])

    def prepare(self):
        if self.args.namespace != 'nuwax-k8s-test':
            if not re.fullmatch(r'rcoder-e2e-[a-z0-9][a-z0-9-]{0,35}', self.args.namespace):
                raise ValueError('Only dedicated rcoder-e2e-* namespaces are accepted')
            ns = json.loads(self.kube('get', 'namespace', self.args.namespace, '-o', 'json'))
            owner = getattr(self.args, 'environment_id', None)
            if not owner or ns['metadata'].get('labels', {}).get('rcoder.dev/environment') != owner:
                raise ValueError('Remote E2E namespace ownership mismatch')
        nodes = json.loads(self.kube('get', 'nodes', '-o', 'json'))['items']
        addresses = {a['address'] for n in nodes for a in n['status']['addresses']}
        self.check('entry_targets_test_nodes', all(urllib.parse.urlparse(url).hostname in addresses for url in (self.args.url, self.args.proxy_url)))
        self.baseline = self.inventory()
        self.save('baseline.json', self.baseline)
        self.check('app_namespace_unused', not any(self.owned(r) for r in self.baseline))
        dep = json.loads(self.kube('get', 'deployment', self.args.deployment, '-o', 'json'))
        selector = ','.join(k + '=' + v for k, v in dep['spec']['selector']['matchLabels'].items())
        pods = json.loads(self.kube('get', 'pods', '-l', selector, '-o', 'json'))['items']
        ready = [p for p in pods if any(c['type'] == 'Ready' and c['status'] == 'True' for c in p['status'].get('conditions', []))]
        self.check('cluster_ready', len(ready) >= 2, [compact_resource(p) for p in pods])
        forwards = []
        for pod in ready:
            with socket.socket() as listener:
                listener.bind(('127.0.0.1', 0))
                port = listener.getsockname()[1]
            forwards += ['-L', f'127.0.0.1:{port}:{pod["status"]["podIP"]}:8086']
            self.entries.append(f'http://127.0.0.1:{port}')
        self.save('deployment.json', {'name': dep['metadata']['name'], 'uid': dep['metadata']['uid'], 'labels': dep['metadata'].get('labels'), 'images': [c['image'] for c in dep['spec']['template']['spec']['containers']]})
        self.save('replicas.json', [{'entry': entry, **compact_resource(p)} for entry, p in zip(self.entries, ready)])
        self.tunnel = subprocess.Popen(['ssh', '-N', '-o', 'BatchMode=yes', '-o', 'ExitOnForwardFailure=yes', *forwards, self.args.ssh], stderr=(self.root / 'tunnel.log').open('w'))
        for entry in self.entries:
            deadline = time.monotonic() + 15
            while True:
                try:
                    self.api('/health', base=entry)
                    break
                except urllib.error.URLError:
                    if time.monotonic() >= deadline or self.tunnel.poll() is not None:
                        raise
                    time.sleep(.2)

    def lifecycle(self):
        return self.api(f'/api/v1/userapp/{self.app}/lifecycle?' + urllib.parse.urlencode({'user_id': self.user}))

    def workspace(self):
        self.created = True  # Record intent before sending any creation request.
        self.save('ownership.json', {'run_id': self.id, 'app_id': self.app, 'user_id': self.user, 'creation_intent': True})
        payload = {'app_id': self.app, 'user_id': self.user}
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda e: self.request('/api/v1/userapp/workspace', payload, e, timeout=240), self.entries[:2]))
        # One conflict is valid; neither request may destroy a successful winner.
        self.check('concurrent_ensure', any(s == 200 and b.get('code') == '0000' for s, b in results), results, fatal=False)
        self.check('concurrent_error_contract', all(s == 200 and b.get('code') in ('0000', 'ERR_CONFLICT') for s, b in results), results, fatal=False)
        for entry in self.entries:
            self.api('/api/v1/userapp/workspace', payload, entry, timeout=240)
        self.check('ensure_retry', True)
        inventory = self.inventory()
        self.save('after-ensure.json', [r for r in inventory if self.owned(r)])
        builders = [r for r in inventory if r['kind'] == 'Pod' and r['name'].startswith('rcoder-app-builder-' + self.app)]
        self.check('builder_identity', len(builders) == 1 and not any(r['name'] == 'rcoder-operation-builder-' + self.app for r in inventory), builders)
        self.builder_uid = builders[0]['uid']
        record = self.lifecycle()
        self.save('lifecycle.json', record)
        self.check('lifecycle_active', bool(record['lifecycle_id']) and record['state'] == 'Active'
                   and record['lifecycle_epoch'] == 1 and record['metadata_revision'] >= 1, record)
        self.lifecycle_id = record['lifecycle_id']
        for i, entry in enumerate(self.entries):
            status, data = self.request('/api/v1/userapp/generate-file', {**payload, 'file_name': f'replica-{i}.txt', 'content': self.id}, entry, {'X-App-Id': self.app})
            self.check(f'file_write_{i}', status == 200 and data.get('success') is True, data)
        _, data = self.request('/api/v1/userapp/get-file-list?' + urllib.parse.urlencode(payload), base=self.entries[-1], headers={'X-App-Id': self.app})
        names = {f['name'] for f in data.get('files', [])}
        self.check('cross_replica_files', all(f'replica-{i}.txt' in names for i in range(len(self.entries))), names.__str__())

    def build(self, version):
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, 'w') as archive:
            archive.writestr('workspace.manifest.toml', 'schema_version = 1\n[workspace]\nname = "k8s-acceptance"\n')
            archive.writestr('web/project.manifest.toml', '''schema_version = 1
[project]
service_id = "web"
name = "K8s acceptance"
type = "static"
[build]
command = ["sh", "-c", "mkdir -p dist && cp index.html dist/index.html"]
artifact = "dist"
[proxy]
path = "/"
strip_prefix = false
''')
            archive.writestr('web/index.html', self.id + '-' + version)
        boundary = uuid.uuid4().hex
        body = b''
        for key, value in {'app_id': self.app, 'user_id': self.user, 'enable_git': 'false'}.items():
            body += f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode()
        body += f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="fixture.zip"\r\nContent-Type: application/zip\r\n\r\n'.encode() + buf.getvalue() + f'\r\n--{boundary}--\r\n'.encode()
        status, result = self.request('/api/v1/userapp/init-project-template', body, headers={'Content-Type': 'multipart/form-data; boundary=' + boundary, 'X-App-Id': self.app})
        self.check('import_' + version, status == 200 and result.get('success') is True, result)
        task = self.api('/api/v1/userapp/build', {'app_id': self.app, 'user_id': self.user}, self.entries[0])
        path = '/api/v1/userapp/tasks/' + task['task_id'] + '?' + urllib.parse.urlencode({'app_id': self.app, 'user_id': self.user})
        result = self.poll(lambda: self.api(path, base=self.entries[-1]), lambda r: r['status'] in ('completed', 'failed', 'cancelled'), 240)
        self.check('build_' + version, result['status'] == 'completed', result)
        release, sha = result['release_id'], result['sha256']
        artifact = '/api/v1/userapp/static/' + self.app + '?' + urllib.parse.urlencode({'release_id': release, 'user_id': self.user})
        status, data = self.request(artifact, raw=True)
        self.check('artifact_' + version, status == 200 and hashlib.sha256(data).hexdigest() == sha)
        (self.root / ('artifact-' + version + '.zip')).write_bytes(data)
        if version == 'A':
            cancel = self.api(path.replace('?', '/cancel?'), {}, self.entries[-1])
            self.check('cross_replica_cancel', cancel.get('already_terminal') is True, cancel)
            stream_path = path.replace('?', '/logs/stream?')
            _, events = self.request(stream_path + '&from_seq=0', raw=True, timeout=30)
            (self.root / 'build-sse.txt').write_bytes(events)
            text = events.decode()
            seqs = [int(s) for s in re.findall(r'^id:\s*(\d+)', text, re.M)]
            self.check('sse_terminal', bool(seqs) and seqs == sorted(set(seqs)) and 'completed' in text, text[-1000:])
            _, past = self.request(stream_path + '&from_seq=' + str(max(seqs) + 100), raw=True, timeout=15)
            self.check('sse_past_terminal', not re.search(rb'^data:', past, re.M), past.decode())
        return {'url': self.args.internal_url.rstrip('/') + artifact, 'sha256': sha}, release

    def prod_pod(self):
        rows = self.inventory()
        pods = [r for r in rows if r['kind'] == 'Pod' and r['name'].startswith('rcoder-app-' + self.app + '-')]
        if len(pods) != 1:
            raise AssertionError('Expected one production pod: ' + str(pods))
        return pods[0]

    # ===== 锁快失败真实场景（stop/restart/delete 忙锁 ERR_CONFLICT；start 等待） =====

    def lock_window_holder(self, artifact, admitted_check):
        """在副本0发起一个在途持有者（热部署），以操作受理记录为可观察同步点。

        返回 (holder_thread, rid, view_path)；线程结果存 holder_outcome 列表。
        admitted_check：持有者被受理后需要满足的断言 id（不同窗口分别登记）。
        """
        rid = 'lock-holder-' + uuid.uuid4().hex[:16]
        holder_outcome = []

        def call():
            holder_outcome.append(self.request('/api/v1/userapp/' + self.app + '/start',
                                               {'user_id': self.user, 'request_id': rid,
                                                **artifact, 'deploy_mode': 'hot'},
                                               self.entries[0], timeout=300))

        thread = threading.Thread(target=call, daemon=True)
        thread.start()
        view_path = '/api/v1/userapp/' + self.app + '/operations/by-request?' + urllib.parse.urlencode(
            {'user_id': self.user, 'request_id': rid})

        def admitted():
            status, data = self.request(view_path, base=self.entries[1])
            operation = data.get('data') if isinstance(data, dict) else None
            return operation if status == 200 and operation and operation.get('operation_id') else None

        operation = self.poll(admitted, bool, 90)
        self.check(admitted_check,
                   thread.is_alive() and operation.get('state') != 'Succeeded',
                   {'operation': operation, 'holder_http_pending': thread.is_alive()})
        return thread, rid, view_path, holder_outcome

    def lock_fail_fast(self, artifact_b):
        """忙锁窗口：外部 stop/restart/delete 立即 ERR_CONFLICT（HTTP 200 信封），
        不等待持有者、无副作用、不排队；释放后不自动执行。"""
        stop_query = '/api/v1/userapp/' + self.app + '/stop?' + urllib.parse.urlencode(
            {'user_id': self.user, 'request_id': 'lock-stop-' + self.id[:16]})
        thread, rid, view_path, holder_outcome = self.lock_window_holder(artifact_b, 'lock_holder_admitted')
        timings = {}
        try:
            # 同实例：stop 打到持有者所在副本（进程内锁冲突）
            started = time.monotonic()
            status, envelope = self.request(stop_query, {}, base=self.entries[0], timeout=30)
            timings['stop_same_replica_ms'] = int((time.monotonic() - started) * 1000)
            self.check('lock_busy_stop_conflict',
                       status == 200 and envelope.get('code') == 'ERR_CONFLICT'
                       and envelope.get('success') is False
                       and 'in progress' in envelope.get('message', ''),
                       {'status': status, 'envelope': envelope, 'elapsed_ms': timings['stop_same_replica_ms']})
            # 跨副本：stop 打到另一 rcoder 实例（ConfigMap 租约互斥）
            started = time.monotonic()
            status, cross = self.request(stop_query, {}, base=self.entries[1], timeout=30)
            timings['stop_other_replica_ms'] = int((time.monotonic() - started) * 1000)
            self.check('lock_cross_replica_conflict',
                       status == 200 and cross.get('code') == 'ERR_CONFLICT'
                       and cross.get('success') is False,
                       {'status': status, 'envelope': cross, 'elapsed_ms': timings['stop_other_replica_ms']})
            # restart / delete 忙锁同样快失败
            started = time.monotonic()
            status, restarted = self.request('/api/v1/userapp/' + self.app + '/restart',
                                             {'user_id': self.user, 'request_id': 'lock-restart-' + self.id[:16]},
                                             base=self.entries[1], timeout=30)
            timings['restart_ms'] = int((time.monotonic() - started) * 1000)
            self.check('lock_busy_restart_conflict',
                       status == 200 and restarted.get('code') == 'ERR_CONFLICT'
                       and restarted.get('success') is False,
                       {'status': status, 'envelope': restarted, 'elapsed_ms': timings['restart_ms']})
            started = time.monotonic()
            status, deleted = self.request('/api/v1/userapp/' + self.app + '/prod/delete',
                                           {'user_id': self.user, 'purge': False,
                                            'request_id': 'lock-delete-' + self.id[:16]},
                                           base=self.entries[1], timeout=30)
            timings['delete_ms'] = int((time.monotonic() - started) * 1000)
            self.check('lock_busy_delete_conflict',
                       status == 200 and deleted.get('code') == 'ERR_CONFLICT'
                       and deleted.get('success') is False,
                       {'status': status, 'envelope': deleted, 'elapsed_ms': timings['delete_ms']})
            # 冲突请求已返回而持有者仍在途：证明没有等待持锁业务完成
            _, view = self.request(view_path, base=self.entries[1])
            operation = view.get('data') or {}
            self.check('lock_holder_still_inflight',
                       bool(operation) and operation.get('state') != 'Succeeded'
                       and thread.is_alive(),
                       {'operation': operation, 'holder_http_pending': thread.is_alive()})
        finally:
            thread.join(300)
        status, envelope = holder_outcome[0]
        self.check('lock_holder_completes', status == 200 and envelope.get('code') == '0000',
                   {'status': status, 'envelope': envelope})
        # 释放后不自动执行被拒请求：给假想的排队 stop 一个执行窗口后，
        # 应用仍应 1 副本 Running、无 Stop 操作记录、内容不受影响
        time.sleep(2)
        pod = self.prod_pod()
        status, rejected_view = self.request(
            '/api/v1/userapp/' + self.app + '/operations/by-request?' + urllib.parse.urlencode(
                {'user_id': self.user, 'request_id': 'lock-stop-' + self.id[:16]}), base=self.entries[1])
        self.check('lock_rejected_no_auto_execution',
                   pod['phase'] == 'Running' and status == 200 and rejected_view.get('code') != '0000',
                   {'pod': pod, 'by_request': rejected_view})
        self.check('lock_no_side_effects', self.content(self.id + '-B'))
        runtime = self.api('/api/v1/userapp/' + self.app + '?' + urllib.parse.urlencode({'user_id': self.user}))
        self.pre_stop_version = runtime.get('resource_version')
        self.save('lock-failfast.json', {'timings': timings, 'pre_stop_version': self.pre_stop_version})

    def lock_start_waits(self, artifact_b):
        """等待语义窗口：无 url start 不快失败——等持有者完成后才返回成功。"""
        thread, rid, view_path, holder_outcome = self.lock_window_holder(artifact_b, 'lock_start_window_admitted')
        start_outcome = []

        def call_start():
            start_outcome.append(self.request('/api/v1/userapp/' + self.app + '/start',
                                              {'user_id': self.user},
                                              base=self.entries[1], timeout=300))

        started = time.monotonic()
        start_thread = threading.Thread(target=call_start, daemon=True)
        start_thread.start()
        thread.join(300)
        holder_done = time.monotonic()
        start_thread.join(300)
        start_done = time.monotonic()
        status, envelope = start_outcome[0]
        self.check('lock_start_waits_for_holder',
                   status == 200 and envelope.get('code') == '0000' and start_done >= holder_done,
                   {'status': status, 'envelope': envelope,
                    'start_elapsed_ms': int((start_done - started) * 1000),
                    'holder_done_before_start_return': start_done >= holder_done})

    def delete_version_guard(self):
        """delete 乐观锁：跨 stop/wake 换代后的过期 resource_version 必须被拒，
        计算资源与数据卷不受影响。"""
        stale = getattr(self, 'pre_stop_version', None)
        runtime = self.api('/api/v1/userapp/' + self.app + '?' + urllib.parse.urlencode({'user_id': self.user}))
        current = runtime.get('resource_version')
        status, envelope = self.request('/api/v1/userapp/' + self.app + '/prod/delete',
                                        {'user_id': self.user, 'purge': False,
                                         'expected_resource_version': stale,
                                         'request_id': 'lock-stale-' + self.id[:16]}, timeout=60)
        self.check('lock_delete_stale_version_conflict',
                   bool(stale) and current != stale
                   and status == 200 and envelope.get('code') == 'ERR_CONFLICT'
                   and envelope.get('success') is False,
                   {'stale_version': stale, 'current_version': current,
                    'status': status, 'envelope': envelope})
        rows = self.inventory()
        deployment = [r for r in rows if r['kind'] == 'Deployment' and r['name'] == 'rcoder-app-' + self.app]
        pvcs = [r for r in rows if r['kind'] == 'PersistentVolumeClaim' and self.app in r['name']]
        self.check('lock_delete_version_keeps_resources', len(deployment) == 1 and bool(pvcs),
                   {'deployment': deployment, 'pvcs': pvcs})


    def content(self, expected):
        path = f'/api/v1/userapp/proxy/app/prod/{self.user}/{self.app}/'
        def read():
            return self.request(path, base=self.args.proxy_url.rstrip('/'), raw=True, timeout=15)
        result = self.poll(read, lambda r: r[0] == 200 and r[1].decode() == expected, 180)
        return result[0] == 200

    def deploy(self, artifact_a, release_a, artifact_b, release_b):
        path = '/api/v1/userapp/' + self.app + '/start'
        cold_request_id = 'cold-' + self.id[:20]
        before = time.monotonic()
        cold = self.api(path, {'user_id': self.user, 'request_id': cold_request_id, **artifact_a}, timeout=360)
        self.check('cold_deploy', time.monotonic() - before < 180, cold)
        self.check('content_A', self.content(self.id + '-A'))
        pod = self.prod_pod()
        self.save('production-A.json', pod)
        # D′：整请求 request_id 幂等——同参重放返回存储响应、Pod 不换、
        # by-request 查询定位该 Deploy 操作。
        replayed = self.api(path, {'user_id': self.user, 'request_id': cold_request_id, **artifact_a}, timeout=120)
        replay_pod = self.prod_pod()
        self.check('request_id_replay_no_redeploy',
                   replayed.get('release_id') == cold.get('release_id') and replay_pod['uid'] == pod['uid'],
                   {'replayed': replayed, 'pod_uid': pod['uid'], 'replay_pod_uid': replay_pod['uid']})
        view = self.api('/api/v1/userapp/' + self.app + '/operations/by-request?' + urllib.parse.urlencode(
            {'user_id': self.user, 'request_id': cold_request_id}))
        self.check('request_id_by_request',
                   view and view['kind'] == 'StartDeployment' and view['state'] == 'Succeeded', view)
        status, result = self.request(path, {'user_id': self.user, **artifact_b, 'deploy_mode': 'hot', 'env': {'UNACCEPTED_CHANGE': '1'}}, timeout=90)
        self.check('hot_env_rejected', status == 200 and result.get('code') == 'ERR_HOT_DEPLOY_ENV_CHANGE', result)
        status, result = self.request(path, {'user_id': self.user, **artifact_b, 'sha256': '0' * 64, 'deploy_mode': 'hot'}, timeout=180)
        self.check('hot_failure', status == 200 and result.get('code') == 'ERR_BACKEND_ERROR' and 'sha256 mismatch' in result.get('message', ''), result)
        self.check('old_content_healthy', self.content(self.id + '-A'))
        hot = self.api(path, {'user_id': self.user, **artifact_b, 'deploy_mode': 'hot'}, timeout=240)
        self.check('hot_deploy', True, hot)
        self.check('content_B', self.content(self.id + '-B'))
        after = self.prod_pod()
        self.save('production-B.json', after)
        self.check('hot_pod_preserved', (pod['uid'], [(c['imageID'], c['containerID']) for c in pod['images']]) == (after['uid'], [(c['imageID'], c['containerID']) for c in after['images']]))
        name = after['name']
        status = json.loads(self.kube('exec', name, '--', 'curl', '-fsS', 'http://127.0.0.1:3010/v1/deploy/status'))
        self.save('deploy-status.json', status)
        self.save('app-cli-version.json', {'pod_uid': after['uid'], 'version': self.kube('exec', name, '--', 'app-cli', '--version').strip()})
        data = status.get('data', status)
        operation = data.get('operation') or {}
        self.check('deploy_identity', operation.get('artifact_release_id') == release_b and bool(operation.get('operation_id'))
                   and data.get('protocol_version') == 4 and operation.get('request_release_id') == hot['release_id']
                   and operation.get('phase') == 'running' and operation.get('deployment_generation_id')
                   and operation.get('deploy_stage') == 'succeeded' and operation.get('persisted') is True, data)
        # 锁快失败/等待语义真实场景（持有者 = 热部署在途）
        self.lock_fail_fast(artifact_b)
        self.lock_start_waits(artifact_b)
        stop_request_id = 'stop-' + uuid.uuid4().hex
        for _ in range(2):
            status, envelope = self.request('/api/v1/userapp/' + self.app + '/stop?' + urllib.parse.urlencode({'user_id': self.user, 'request_id': stop_request_id}), {})
            self.check('stop_idempotent', status == 200 and envelope.get('code') == '0000', envelope)
        self.check('stop_request_operation', bool(envelope.get('operation_id')), envelope)
        view = self.api('/api/v1/userapp/' + self.app + '/operations/by-request?' + urllib.parse.urlencode({'user_id': self.user, 'request_id': stop_request_id}))
        self.save('stop-operation.json', view)
        self.check('stop_operation_by_request', view and view['operation_id'] == envelope['operation_id']
                   and view['kind'] == 'Stop' and view['state'] == 'Succeeded' and view['lifecycle_id'] == self.lifecycle_id, view)
        stopped = self.poll(lambda: [r for r in self.inventory() if r['kind'] == 'Pod' and r['name'].startswith('rcoder-app-' + self.app + '-')], lambda rows: not rows, 120)
        self.check('stop_zero_pods', not stopped)
        self.api(path, {'user_id': self.user}, timeout=240)
        self.check('wake', True)
        self.check('wake_content_B', self.content(self.id + '-B'))
        awakened = self.prod_pod()
        self.save('production-wake.json', awakened)
        self.check('wake_new_pod', awakened['uid'] != after['uid'], awakened['uid'])
        stable = self.lifecycle()
        self.check('lifecycle_stable_stop_wake', stable['lifecycle_id'] == self.lifecycle_id
                   and stable['state'] == 'Active' and stable['lifecycle_epoch'] == 1, stable)
        # delete 乐观锁护栏（stop/wake 换代后的过期版本必须被拒）
        self.delete_version_guard()

    def cleanup(self):
        if self.created:
            rows = self.inventory()
            owned = [r for r in rows if self.owned(r)]
            self.save('before-cleanup.json', owned)
            baseline_uids = {r['uid'] for r in self.baseline}
            if any(r['uid'] in baseline_uids for r in owned):
                raise AssertionError('Refusing cleanup of a preexisting identity')
            if any(r['kind'] == 'Deployment' and r['name'] == 'rcoder-app-' + self.app for r in owned):
                self.api_retry_conflict('/api/v1/userapp/' + self.app + '/prod/delete', {'user_id': self.user, 'purge': True})
            self.api_retry_conflict('/api/v1/userapp/' + self.app + '/delete/app', {'user_id': self.user})
            if self.lifecycle_id is None:
                remaining = self.poll(lambda: [r for r in self.inventory() if self.owned(r)], lambda r: not r, 180)
                self.check('cleanup', not remaining, remaining)
                return self.preserve_baseline()
            tombstone = self.lifecycle()
            self.save('lifecycle-tombstone.json', tombstone)
            self.check('tombstone_deleted', tombstone['state'] == 'Deleted'
                       and tombstone['lifecycle_id'] == self.lifecycle_id, tombstone)
            status, rejected = self.request('/api/v1/userapp/workspace', {'app_id': self.app, 'user_id': self.user}, timeout=120)
            self.save('tombstone-ensure.json', rejected)
            self.check('tombstone_old_ensure_rejected', status == 200 and rejected.get('code') != '0000', rejected)
            recreated = self.api('/api/v1/userapp/' + self.app + '/recreate', {'user_id': self.user, 'expected_lifecycle_id': self.lifecycle_id, 'request_id': 'recreate-' + self.id}, timeout=120)
            self.save('lifecycle-recreated.json', recreated)
            self.check('recreate_new_lifecycle', recreated['lifecycle_id'] != self.lifecycle_id
                       and recreated['lifecycle_epoch'] == tombstone['lifecycle_epoch'] + 1
                       and recreated['state'] == 'Active', recreated)
            self.api('/api/v1/userapp/' + self.app + '/delete/app', {'user_id': self.user, 'lifecycle_id': recreated['lifecycle_id']}, timeout=180)
            final = self.lifecycle()
            self.check('recreate_deleted', final['state'] == 'Deleted'
                       and final['lifecycle_id'] == recreated['lifecycle_id'], final)
            remaining = self.poll(lambda: [r for r in self.inventory() if self.owned(r)], lambda r: not r, 180)
            self.check('cleanup', not remaining, remaining)
        self.preserve_baseline()

    def preserve_baseline(self):
        after = self.inventory()
        self.save('after-cleanup.json', after)
        # Workload/PVC UIDs are stable; unrelated Pods can legitimately roll independently.
        stable = {'PersistentVolumeClaim', 'Deployment', 'StatefulSet'}
        preserved = {(r['kind'], r['name'], r['uid']) for r in after}
        missing = [r for r in self.baseline if r['kind'] in stable and (r['kind'], r['name'], r['uid']) not in preserved]
        self.check('baseline_preserved', not missing, missing)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ssh', default=os.environ.get('TEST_K8S_SSH'), required=not os.environ.get('TEST_K8S_SSH'))
    parser.add_argument('--namespace', default='nuwax-k8s-test')
    parser.add_argument('--context')
    parser.add_argument('--environment-id')
    parser.add_argument('--deployment', default='nuwax-k8s-test-rcoder')
    parser.add_argument('--url', required=True)
    parser.add_argument('--proxy-url', required=True)
    parser.add_argument('--internal-url', default='http://nuwax-k8s-test-rcoder:8086')
    args = parser.parse_args()
    run = Run(args)
    def interrupt(*_):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    error = None
    try:
        run.prepare()
        run.workspace()
        a, ra = run.build('A')
        b, rb = run.build('B')
        run.deploy(a, ra, b, rb)
    except (Exception, KeyboardInterrupt) as exc:
        error = repr(exc)
        print(error, flush=True)
    finally:
        try:
            run.cleanup()
        except (Exception, KeyboardInterrupt) as exc:
            run.summary['cleanup_error'] = repr(exc)
        if run.tunnel:
            run.tunnel.terminate()
            run.tunnel.wait(timeout=15)
        missing = REQUIRED - {r['id'] for r in run.assertions if r['ok']}
        run.summary.update(error=error, missing=sorted(missing), verdict='pass' if not error and not missing and all(r['ok'] for r in run.assertions) and not run.summary.get('cleanup_error') else 'fail')
        run.save('summary.json', run.summary)
    print(json.dumps(run.summary, ensure_ascii=False), flush=True)
    return 0 if run.summary['verdict'] == 'pass' else 1


if __name__ == '__main__':
    raise SystemExit(main())
