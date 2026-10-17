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
            'lock_busy_start_conflict',
            'lock_delete_stale_version_conflict', 'lock_delete_version_keeps_resources',
            'lease_dead_holder_takeover', 'lease_released_after_operation',
            'lifecycle_active', 'stop_request_operation', 'stop_operation_by_request',
            'lifecycle_stable_stop_wake', 'request_id_replay_no_redeploy', 'request_id_by_request',
            'stop_idempotent', 'stop_zero_pods', 'wake_new_pod', 'wake', 'wake_content_B', 'cleanup',
            'tombstone_deleted', 'tombstone_old_ensure_rejected', 'recreate_new_lifecycle',
            'recreate_deleted', 'baseline_preserved', 'worker_artifact_contract', 'worker_process_running'}
REQUIRED |= {'dev_readonly_seed_data', 'dev_compute_stop_identity', 'dev_stopped_resource_identity',
             'dev_stopped_rv_advanced', 'dev_stopped_storage_both_replicas',
             'dev_stopped_logs_both_replicas', 'dev_stopped_no_wake',
             'dev_readonly_annotation_removed', 'dev_compute_restart_identity',
             'dev_readonly_data_preserved'}
REQUIRED |= {'recreate_workspace_usable', 'recreate_files_git', 'build_L2', 'artifact_L2',
             'recreate_deploy_operation', 'recreate_content_L2',
             'recreate_compute_stop', 'recreate_compute_restart', 'recreate_data_preserved',
             'recreate_old_lifecycle_rejected', 'recreate_history_preserved',
             'recreate_owned_pvc_identity'}
REQUIRED |= {'traffic_wake_without_start', 'traffic_wake_single_instance',
             'traffic_wake_pvc_data_preserved', 'wake_explicit_start_idempotent'}

# The independent lifecycle scenario has its own complete contract. It never
# narrows the default all-scenario contract or runs traffic wake as a prerequisite.
RECREATE_REQUIRED = {
    'cluster_ready', 'concurrent_ensure', 'concurrent_error_contract', 'ensure_retry',
    'builder_identity', 'lifecycle_active', 'cross_replica_files',
    'recreate_l1_files_git', 'import_L1', 'build_L1', 'artifact_L1',
    'recreate_l1_deploy_operation', 'recreate_l1_content',
    'recreate_l1_compute_history', 'recreate_l1_data_preserved',
    'tombstone_deleted', 'tombstone_old_ensure_rejected', 'recreate_new_lifecycle',
    'recreate_workspace_usable', 'recreate_files_git', 'build_L2', 'artifact_L2',
    'recreate_deploy_operation', 'recreate_content_L2', 'recreate_compute_stop',
    'recreate_compute_restart', 'recreate_data_preserved',
    'recreate_old_lifecycle_rejected', 'recreate_history_preserved',
    'recreate_owned_pvc_identity', 'recreate_deleted', 'cleanup', 'baseline_preserved',
    'worker_artifact_contract',
}
SCENARIO_REQUIRED = {'all': frozenset(REQUIRED), 'recreate': frozenset(RECREATE_REQUIRED)}
SCENARIO_IDENTITIES = {'all': 'k8s_userapp', 'recreate': 'k8s_userapp_recreate'}

RESTART_ADMISSION_WAIT_SECONDS = 30
RESTART_EXECUTION_TIMEOUT_SECONDS = 300
# The formal /restart controller calls restart_app_enhanced/deploy_controlled,
# including the no-URL request; it does not use the independent compute kind.
RESTART_OPERATION_KIND = 'RestartDeployment'


class NoProxyRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, url):
        return None


def wire_name(name):
    """The existing operation view uses PascalCase; busy data uses snake_case."""
    return re.sub(r'(?<!^)(?=[A-Z])', '_', name).lower()


def operation_busy_data(status, envelope):
    """Validate the structured busy contract without interpreting its message."""
    if (status != 200 or not isinstance(envelope, dict)
            or envelope.get('code') != 'ERR_OPERATION_IN_PROGRESS'
            or envelope.get('success') is not False):
        raise AssertionError('Not a formal UserApp operation-busy response')
    data = envelope.get('data')
    fields = ('holder_operation_id', 'holder_kind', 'holder_state', 'holder_step')
    if (not isinstance(data, dict) or any(key not in data for key in fields)
            or type(data.get('holder_traffic_wake')) is not bool
            or type(data.get('retryable')) is not bool
            or type(data.get('retry_after_seconds')) is not int
            or data['retry_after_seconds'] < 0):
        raise AssertionError('Operation-busy data is incomplete or has invalid types')
    if all(data[key] is None for key in fields):
        if data['holder_traffic_wake'] or data['retryable'] or data['retry_after_seconds'] != 0:
            raise AssertionError('An unknown holder must not authorize retry')
        return data
    if not all(isinstance(data[key], str) and data[key] for key in fields):
        raise AssertionError('Operation-busy holder identity is partially populated')
    blocker = envelope.get('blocker')
    if (not isinstance(blocker, dict)
            or not all(isinstance(blocker.get(key), str) and blocker[key]
                       for key in ('operation_id', 'kind', 'state', 'step', 'scope'))
            or data['holder_operation_id'] != blocker['operation_id']
            or data['holder_kind'] != wire_name(blocker['kind'])
            or data['holder_state'] != wire_name(blocker['state'])
            or data['holder_step'] != blocker['step']):
        raise AssertionError('Operation-busy data and blocker describe different snapshots')
    if not data['retryable'] and data['retry_after_seconds'] != 0:
        raise AssertionError('Protected holder must not advertise a retry delay')
    if data['retryable'] and (
            data['holder_state'] not in ('pending', 'running', 'waiting_retry')
            or data['retry_after_seconds'] <= 0
            or envelope.get('operation_id')):
        raise AssertionError('Accepted or protected operation must not authorize retry')
    if data['retryable'] and (
            (data['holder_traffic_wake'] and (data['holder_kind'] != 'start'
                                             or data['retry_after_seconds'] != 20))
            or (not data['holder_traffic_wake'] and (
                data['holder_kind'] not in ('start_deployment', 'restart_deployment', 'hot_deploy', 'restart')
                or data['retry_after_seconds'] != 45))):
        raise AssertionError('Retry guidance does not match the verified holder kind')
    return data


def valid_busy_response(status, envelope):
    try:
        operation_busy_data(status, envelope)
    except AssertionError:
        return False
    return True


def busy_matches_holder(status, envelope, holder):
    try:
        data = operation_busy_data(status, envelope)
    except AssertionError:
        return False
    # State/step are checked against the response's own blocker above. A live
    # by-request query may have advanced since that snapshot.
    return (data['holder_operation_id'] == holder.get('operation_id')
            and data['holder_kind'] == wire_name(holder.get('kind', ''))
            and envelope['blocker']['scope'] == holder.get('scope')
            and data['holder_traffic_wake'] is False
            and data['retryable'] is True and data['retry_after_seconds'] == 45)


def restart_admission_after_holder(own, holder):
    return own is None or (holder.get('state') == 'Succeeded'
                           and own.get('operation_id') != holder.get('operation_id'))


def restart_result_matches(status, envelope, holder, final_holder, own, elapsed):
    if envelope.get('code') == 'ERR_OPERATION_IN_PROGRESS':
        return (busy_matches_holder(status, envelope, holder) and own is None
                and RESTART_ADMISSION_WAIT_SECONDS - 1 <= elapsed <= RESTART_ADMISSION_WAIT_SECONDS + 10)
    return (status == 200 and envelope.get('code') == '0000' and envelope.get('success') is True
            and own is not None and envelope.get('operation_id') == own.get('operation_id')
            and final_holder.get('operation_id') == holder.get('operation_id')
            and restart_admission_after_holder(own, final_holder)
            and own.get('kind') == RESTART_OPERATION_KIND and own.get('scope') == 'Prod'
            and own.get('state') == 'Succeeded')


def remaining_timeout(deadline, limit):
    if deadline is None:
        return limit
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError('Original REST observation deadline expired')
    return min(limit, remaining)


def dev_compute_view(view, app, lifecycle, action, operation_id):
    """Only the original public compute identity can finish this normal scenario."""
    if (not isinstance(view, dict) or not operation_id
            or view.get('operation_id') != operation_id or view.get('app_id') != app
            or view.get('lifecycle_id') != lifecycle or view.get('scope') != 'Dev'
            or view.get('action') != action):
        raise AssertionError('Development compute operation identity differs')
    state = view.get('state')
    if state in ('pending', 'running'):
        return False
    if state == 'succeeded':
        if view.get('stage') != 'completed':
            raise AssertionError('Succeeded development compute operation is not completed')
        return True
    if state in ('recovery_required', 'failed', 'superseded'):
        raise AssertionError('Development compute did not succeed: ' + str(view))
    raise AssertionError('Unknown development compute state: ' + str(state))


def stopped_dev_logs(status, envelope):
    # The existing locator diagnostic is wrapped by AppOperationError::Backend.
    # This tests that diagnostic, without changing or inventing a public code.
    return (status == 200 and isinstance(envelope, dict)
            and envelope.get('code') == 'ERR_BACKEND_ERROR' and envelope.get('success') is False
            and 'not running; its file logs are currently unavailable' in envelope.get('message', ''))


def dev_controller_identity(row, app, lifecycle, expected_uid=None):
    meta = row.get('metadata') or {}
    labels, annotations = meta.get('labels') or {}, meta.get('annotations') or {}
    if (row.get('kind') != 'StatefulSet' or meta.get('name') != 'rcoder-app-builder-' + app
            or not meta.get('uid') or not meta.get('resourceVersion') or meta.get('deletionTimestamp')
            or (expected_uid is not None and meta['uid'] != expected_uid)
            or labels.get('rcoder.io/identifier') != app
            or labels.get('rcoder.io/service-type') != 'user-app-builder'
            or annotations.get('rcoder.io/application-id') != app
            or annotations.get('rcoder.io/lifecycle-id') != lifecycle):
        raise AssertionError('Refusing a different development controller identity')
    return meta


def owned_userapp_pvc_identities(rows, app, lifecycle, baseline_uids):
    """Capture only exact, newly created UserApp volumes, never agent/baseline PVCs."""
    expected = {'rcoder-app-builder-' + app + '-workspace': 'user-app-builder',
                'rcoder-app-' + app + '-workspace': 'user-app'}
    claims = []
    for row in rows:
        if row.get('kind') != 'PersistentVolumeClaim':
            continue
        labels, annotations = row.get('labels') or {}, row.get('annotations') or {}
        if (row.get('name') not in expected or not row.get('uid') or row['uid'] in baseline_uids
                or labels.get('app.kubernetes.io/managed-by') != 'rcoder-runtime'
                or labels.get('service_type') != expected[row['name']]
                or annotations.get('rcoder.io/application-id') != app
                or annotations.get('rcoder.io/lifecycle-id') != lifecycle):
            raise AssertionError('Recreated PVC is not an exact new lifecycle-owned UserApp volume')
        claims.append((row['name'], row['uid']))
    if len({name for name, _uid in claims}) != len(claims):
        raise AssertionError('Recreated PVC names are ambiguous')
    return sorted(claims)


def stale_lifecycle_rejected(status, envelope, compute=False):
    return (status in ((409,) if compute else (200, 409)) and isinstance(envelope, dict)
            and envelope.get('code') == 'ERR_CONFLICT' and envelope.get('success') is False
            and not envelope.get('operation_id'))


def historical_compute_view_preserved(before, after):
    """Terminal lease release may advance revision without rewriting the public receipt."""
    if not isinstance(before, dict) or not isinstance(after, dict):
        return False
    original, current = before.get('revision'), after.get('revision')
    return (type(original) is int and type(current) is int and original >= 0 and current >= original
            and {key: value for key, value in before.items() if key != 'revision'}
            == {key: value for key, value in after.items() if key != 'revision'})


def rejected_without_execution(view, kind):
    return view is None or (view.get('kind') == kind and view.get('scope') == 'Prod'
                            and view.get('state') == 'Failed'
                            and view.get('error_code') == 'ERR_OPERATION_IN_PROGRESS')


def restarted_pod_ready(pod, app_id, old_uid, operation_id):
    labels = pod.get('labels') or {}
    annotations = pod.get('annotations') or {}
    images = pod.get('images') or []
    return (pod.get('kind') == 'Pod'
            and pod.get('name', '').startswith('rcoder-app-' + app_id + '-')
            and labels.get('rcoder.io/app-id') == app_id
            and labels.get('app.kubernetes.io/instance') == app_id
            and labels.get('app.kubernetes.io/managed-by') == 'rcoder-app-manager'
            and bool(pod.get('uid')) and pod['uid'] != old_uid
            and annotations.get('rcoder.io/restart-operation') == operation_id
            and not pod.get('deletionTimestamp') and pod.get('phase') == 'Running'
            and any(condition.get('type') == 'Ready' and condition.get('status') == 'True'
                    for condition in pod.get('conditions') or [])
            and bool(images) and all(image.get('ready') is True for image in images))


def compact_resource(row):
    meta = row['metadata']
    return {'kind': row.get('kind'), 'name': meta['name'], 'uid': meta['uid'],
            'resourceVersion': meta.get('resourceVersion'), 'generation': meta.get('generation'),
            'labels': meta.get('labels', {}),
            'deletionTimestamp': meta.get('deletionTimestamp'),
            'annotations': {key: value for key, value in (meta.get('annotations') or {}).items()
                            if key in ('rcoder.io/restart-operation', 'rcoder.io/application-id', 'rcoder.io/lifecycle-id')},
            'phase': row.get('status', {}).get('phase'),
            'volumeName': row.get('spec', {}).get('volumeName'),
            'podIP': row.get('status', {}).get('podIP'),
            'conditions': row.get('status', {}).get('conditions', []),
            'images': [{k: c.get(k) for k in ('name', 'image', 'imageID', 'containerID', 'ready', 'restartCount')}
                       for c in row.get('status', {}).get('containerStatuses', [])]}


class Run:
    def __init__(self, args):
        self.args = args
        self.scenario = getattr(args, 'scenario', 'all')
        self.required = SCENARIO_REQUIRED[self.scenario]
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
        self.recreated = False
        self.lifecycle_id = None
        self.base = args.url.rstrip('/')
        self.entries = []
        self.proxy_entries = []
        self.trace = uuid.uuid4().hex
        self.summary = {'run_id': self.id, 'app_id': self.app, 'namespace': args.namespace,
                        'ssh': args.ssh, 'scenario': self.scenario,
                        'report_identity': {'scenario': SCENARIO_IDENTITIES[self.scenario], 'backend': 'k8s'},
                        'planned': sorted(self.required), 'required': sorted(self.required), 'verdict': 'aborted',
                        'head': (ORIGIN_HEAD or self.local('git', 'rev-parse', 'HEAD')).strip(),
                        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
        from run import source_fingerprint
        self.summary['worktree_sha256'] = source_fingerprint()
        self.save('summary.json', self.summary)
        print('Report: ' + str(self.root), flush=True)

    def local(self, *args, deadline=None):
        result = subprocess.check_output(args, cwd=REPO, text=True,
                                         timeout=remaining_timeout(deadline, 90))
        remaining_timeout(deadline, 90)
        return result

    def kube(self, *args, deadline=None):
        context = getattr(self.args, 'context', None)
        server = getattr(self.args, 'kube_server', None)
        command = shlex.join(['kubectl', *(['--context', context] if context else []),
                              *(['--server', server] if server else []), '-n', self.args.namespace, *args])
        return self.local('ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', self.args.ssh, command,
                          deadline=deadline)

    def inventory(self, deadline=None):
        data = json.loads(self.kube('get', 'pods,statefulsets,deployments,pvc,services,configmaps', '-o', 'json',
                                    deadline=deadline))
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

    def first_proxy_get(self, path, entry, deadline):
        """One real proxy GET: no environment proxy, redirect follow, or retry."""
        headers = {'traceparent': '00-' + self.trace + '-' + uuid.uuid4().hex[:16] + '-01'}
        request = urllib.request.Request(entry + path, headers=headers, method='GET')
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoProxyRedirect())
        started = time.monotonic()
        try:
            response = opener.open(request, timeout=remaining_timeout(deadline, 150))
        except urllib.error.HTTPError as error:
            response = error
        with response:
            content, status = response.read(), response.status
        remaining_timeout(deadline, 150)
        self.requests.append({'path': path, 'method': 'GET', 'status': status,
                              'elapsed': time.monotonic() - started, 'base': entry, 'trace_id': self.trace,
                              'size': len(content), 'sha256': hashlib.sha256(content).hexdigest(),
                              'attempts': 1, 'redirects_followed': False, 'environment_proxy': False})
        self.save('requests.json', self.requests)
        return status, content

    def api_retry_operation_busy(self, path, body=None, timeout=180, attempts=40, interval=10):
        """Only structured, safely retryable busy rejection permits replay.

        Keep one request ID/input and the existing bounded retry window. Genuine
        ERR_CONFLICT and unknown/protected holder responses fail immediately.
        """
        body = dict(body or {})
        body.setdefault('request_id', 'cleanup-retry-' + uuid.uuid4().hex)
        deadline = time.monotonic() + attempts * interval
        last = None
        for _ in range(attempts):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            status, envelope = self.request(path, body, timeout=min(timeout, remaining))
            if status == 200 and envelope.get('code') == '0000' and envelope.get('success') is True:
                return envelope['data']
            if status == 200 and envelope.get('code') == 'ERR_OPERATION_IN_PROGRESS':
                data = operation_busy_data(status, envelope)
                if not data['retryable']:
                    raise AssertionError(f'{path}: busy holder does not authorize retry: {envelope}')
                last = envelope
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                time.sleep(min(max(interval, data['retry_after_seconds']), remaining))
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
        dep = json.loads(self.kube('get', self.args.workload_kind, self.args.deployment, '-o', 'json'))
        selector = ','.join(k + '=' + v for k, v in dep['spec']['selector']['matchLabels'].items())
        pods = json.loads(self.kube('get', 'pods', '-l', selector, '-o', 'json'))['items']
        ready = [p for p in pods if any(c['type'] == 'Ready' and c['status'] == 'True' for c in p['status'].get('conditions', []))]
        self.check('cluster_ready', len(ready) >= 2, [compact_resource(p) for p in pods])
        forwards, allocated_ports = [], set()
        for pod in ready:
            for target_port, entries in ((8086, self.entries), (self.args.proxy_port, self.proxy_entries)):
                while True:
                    with socket.socket() as listener:
                        listener.bind(('127.0.0.1', 0))
                        port = listener.getsockname()[1]
                    if port not in allocated_ports:
                        allocated_ports.add(port)
                        break
                forwards += ['-L', f'127.0.0.1:{port}:{pod["status"]["podIP"]}:{target_port}']
                entries.append(f'http://127.0.0.1:{port}')
        self.save('deployment.json', {'name': dep['metadata']['name'], 'uid': dep['metadata']['uid'], 'labels': dep['metadata'].get('labels'), 'images': [c['image'] for c in dep['spec']['template']['spec']['containers']]})
        self.save('replicas.json', [{'entry': entry, 'proxy_entry': proxy, **compact_resource(p)}
                                    for entry, proxy, p in zip(self.entries, self.proxy_entries, ready)])
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
        self.check('concurrent_error_contract', all(
            s == 200 and ((b.get('code') == '0000' and b.get('success') is True)
                         or (b.get('code') == 'ERR_CONFLICT' and b.get('success') is False)
                         or valid_busy_response(s, b)) for s, b in results), results, fatal=False)
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

    def dev_compute(self, action, budget, deadline=None, request_id=None, evidence_prefix='readonly-dev'):
        if action not in ('stop', 'restart'):
            raise ValueError('Explicit development compute action required')
        if deadline is None:
            deadline = time.monotonic() + budget
        body = {'app_id': self.app, 'app_stage': 'dev', 'service_type': 'userapp',
                'lifecycle_id': self.lifecycle_id,
                'request_id': request_id or 'readonly-' + action + '-' + self.id}
        status, response = self.request('/computer/pod/' + action, body, self.entries[0],
                                        timeout=remaining_timeout(deadline, 90))
        remaining_timeout(deadline, 90)
        view = response.get('data')
        operation_id = response.get('operation_id')
        if status not in (200, 202) or response.get('code') != '0000' or response.get('success') is not True:
            raise AssertionError('Development compute admission failed: ' + str(response))
        path = '/computer/pod/operations/' + self.app + '/' + str(operation_id)
        while True:
            self.save(evidence_prefix + '-' + action + '-operation.json', view)
            if dev_compute_view(view, self.app, self.lifecycle_id, action, operation_id):
                if not hasattr(self, 'compute_receipts'):
                    self.compute_receipts = []
                self.compute_receipts.append(json.loads(json.dumps(view)))
                return view
            time.sleep(min(.2, remaining_timeout(deadline, .2)))
            view = self.api(path, base=self.entries[-1], timeout=remaining_timeout(deadline, 30))
            remaining_timeout(deadline, 30)

    def dev_controller(self, expected_uid=None, deadline=None):
        row = json.loads(self.kube('get', 'statefulset', 'rcoder-app-builder-' + self.app,
                                   '-o', 'json', deadline=deadline))
        dev_controller_identity(row, self.app, self.lifecycle_id, expected_uid)
        return row

    def app_pvc_identities(self, resources):
        """Compare only this lifecycle's claims; unrelated new apps are allowed."""
        self.preserve_pvc_baseline(resources)
        return owned_userapp_pvc_identities(
            [row for row in resources if row['kind'] == 'PersistentVolumeClaim' and self.owned(row)],
            self.app, self.lifecycle_id, {row['uid'] for row in getattr(self, 'baseline', [])})

    def preserve_pvc_baseline(self, resources):
        present = {row['uid']: row for row in resources if row['kind'] == 'PersistentVolumeClaim'}
        for before in getattr(self, 'baseline', []):
            if before['kind'] != 'PersistentVolumeClaim':
                continue
            after = present.get(before['uid'])
            if (after is None or after['name'] != before['name'] or after.get('deletionTimestamp')
                    or (before.get('volumeName') is not None
                        and after.get('volumeName') != before['volumeName'])):
                raise AssertionError('A baseline PVC UID or persistent-volume binding changed')

    def stopped_dev_view(self, uid, deadline):
        row = self.dev_controller(uid, deadline)
        resources = self.inventory(deadline=deadline)
        pods = [r for r in resources if r['kind'] == 'Pod'
                and r['name'].startswith('rcoder-app-builder-' + self.app + '-')]
        if row.get('spec', {}).get('replicas') != 0 or pods:
            raise AssertionError('Read-only query woke development compute: ' + str(pods))
        return {'controller': compact_resource(row), 'replicas': 0,
                'claims': self.app_pvc_identities(resources)}

    def observe_restarted_dev(self, uid, claims, deadline):
        """Compute completion can precede kubelet Ready; share its original budget."""
        observed_uid = None
        last = None
        try:
            while True:
                remaining_timeout(deadline, 90)
                controller = self.dev_controller(uid, deadline)
                resources = self.inventory(deadline=deadline)
                builders = [r for r in resources if r['kind'] == 'Pod'
                            and r['name'].startswith('rcoder-app-builder-' + self.app + '-')]
                current_claims = self.app_pvc_identities(resources)
                last = {'controller': compact_resource(controller), 'pods': builders,
                        'claims': current_claims}
                self.save('readonly-dev-restart-readiness.json', last)
                remaining_timeout(deadline, 90)
                if controller.get('spec', {}).get('replicas') != 1 or current_claims != claims:
                    raise AssertionError('Development restart changed replica intent or PVC identity')
                if len(builders) > 1:
                    raise AssertionError('Development restart has multiple builder Pods')
                if builders:
                    pod = builders[0]
                    if (not pod.get('uid') or pod['uid'] == self.builder_uid
                            or pod.get('deletionTimestamp')
                            or (observed_uid is not None and pod['uid'] != observed_uid)):
                        raise AssertionError('Development restart Pod identity changed')
                    observed_uid = pod['uid']
                    if pod.get('phase') not in ('Pending', 'Running'):
                        raise AssertionError('Unexpected development restart Pod phase')
                    if (pod['phase'] == 'Running'
                            and any(c.get('type') == 'Ready' and c.get('status') == 'True'
                                    for c in pod.get('conditions', []))):
                        return controller, builders
                time.sleep(min(.2, remaining_timeout(deadline, .2)))
        except TimeoutError as error:
            raise TimeoutError('Development restart Ready deadline expired; last observation: '
                               + str(last)) from error

    def patch_readonly_annotation(self, uid, value, deadline):
        row = self.dev_controller(uid, deadline)
        meta = row['metadata']
        intent = self.dev_readonly_annotation
        current = (meta.get('annotations') or {}).get(intent['key'])
        if current is not None and current not in intent['values']:
            raise AssertionError('Refusing to overwrite another annotation value')
        # Record intent before dispatch so an unknown SSH outcome can still be
        # cleaned by its exact UID and this run's issued values.
        intent['values'].add(value)
        patch = {'metadata': {'uid': uid, 'resourceVersion': meta['resourceVersion'],
                              'annotations': {intent['key']: value}}}
        result = json.loads(self.kube('patch', 'statefulset', meta['name'], '--type=merge',
                                     '-p', json.dumps(patch), '-o', 'json', deadline=deadline))
        after = dev_controller_identity(result, self.app, self.lifecycle_id, uid)
        if ((after.get('annotations') or {}).get(intent['key']) != value
                or after['resourceVersion'] == meta['resourceVersion']):
            raise AssertionError('Conditional test annotation did not advance resourceVersion')
        return after['resourceVersion']

    def remove_readonly_annotation(self):
        intent = getattr(self, 'dev_readonly_annotation', None)
        if intent is None:
            return
        deadline = time.monotonic() + 30
        row = self.dev_controller(intent['uid'], deadline)
        meta = row['metadata']
        current = (meta.get('annotations') or {}).get(intent['key'])
        if current is None:
            self.dev_readonly_annotation = None
            return
        if current not in intent['values']:
            raise AssertionError('Refusing removal of a foreign annotation value')
        path = '/metadata/annotations/' + intent['key'].replace('~', '~0').replace('/', '~1')
        patch = [{'op': 'test', 'path': '/metadata/uid', 'value': intent['uid']},
                 {'op': 'test', 'path': '/metadata/resourceVersion', 'value': meta['resourceVersion']},
                 {'op': 'test', 'path': path, 'value': current}, {'op': 'remove', 'path': path}]
        result = json.loads(self.kube('patch', 'statefulset', meta['name'], '--type=json',
                                     '-p', json.dumps(patch), '-o', 'json', deadline=deadline))
        after = dev_controller_identity(result, self.app, self.lifecycle_id, intent['uid'])
        if intent['key'] in (after.get('annotations') or {}):
            raise AssertionError('Test annotation remains after conditional removal')
        self.dev_readonly_annotation = None

    def readonly_dev_pair(self, entry, deadline):
        payload = {'page': 1, 'page_size': 10, 'filters': {'app_ids': [self.app]}}
        storage = self.api('/api/v1/userapp/storage/dev/query', payload, entry,
                           timeout=remaining_timeout(deadline, 15))
        remaining_timeout(deadline, 15)
        items = storage.get('items')
        storage_ok = (isinstance(items, list) and len(items) == 1
                      and items[0].get('app_id') == self.app and items[0].get('exists') is True
                      and items[0].get('is_orphan') is False)
        path = '/api/v1/userapp/' + self.app + '/dev/logs/sources/query?' + urllib.parse.urlencode({'user_id': self.user})
        status, logs = self.request(path, {}, entry, timeout=remaining_timeout(deadline, 15))
        remaining_timeout(deadline, 15)
        if not storage_ok or not stopped_dev_logs(status, logs):
            raise AssertionError('Stopped development read contract differs: ' + str({'storage': storage, 'logs': logs}))
        return {'entry': self.entries.index(entry), 'storage_nonorphan': True,
                'logs_stopped': True, 'logs_code': logs['code']}

    def dev_git(self, suffix, extra=None, entry=None, query=False):
        context = {'workspaceType': 'userApp', 'serviceType': 'userapp', 'appId': self.app,
                   'workspacePath': '/home/user/' + self.app, 'userId': self.user, 'cId': self.app}
        context.update(extra or {})
        headers = {'X-Service-Type': 'userapp', 'X-Workspace-Type': 'userApp', 'X-App-Id': self.app}
        path = '/api/git/' + suffix
        status, response = self.request(path + ('?' + urllib.parse.urlencode(context) if query else ''),
                                        None if query else context, entry, headers)
        if status != 200 or response.get('success') is not True:
            raise AssertionError('Development Git request failed: ' + str(response))
        return response

    def dev_data_proof(self, filename, commit):
        proof = {'commit': commit}
        for ref in ('worktree', commit):
            response = self.dev_git('file-content', {'filePath': filename, 'ref': ref}, self.entries[-1])
            content = response.get('content')
            if content != self.id:
                raise AssertionError('Original development sentinel content differs')
            proof['sentinel_sha256' if ref == 'worktree' else 'git_blob_sha256'] = hashlib.sha256(content.encode()).hexdigest()
        log = self.dev_git('log', {'maxCount': 10}, self.entries[-1], query=True)
        if not any(row.get('hash') == commit for row in log.get('commits', [])):
            raise AssertionError('Original development Git commit is missing')
        payload = {'app_id': self.app, 'user_id': self.user}
        status, files = self.request('/api/v1/userapp/get-file-list?' + urllib.parse.urlencode(payload),
                                     base=self.entries[-1], headers={'X-App-Id': self.app})
        names = {f['name'] for f in files.get('files', [])}
        expected = {filename, *('replica-' + str(i) + '.txt' for i in range(len(self.entries)))}
        if status != 200 or files.get('success') is not True or not expected.issubset(names):
            raise AssertionError('Original development files are missing')
        proof['files'] = sorted(expected)
        return proof

    def dev_readonly_locator(self):
        """Real RV churn plus no-wake checks; deterministic cross-capture is a unit regression."""
        if len(self.entries) < 2 or len(set(self.entries[:2])) != 2 or not self.lifecycle_id:
            raise AssertionError('Two distinct RCoder replicas and a lifecycle are required')
        filename = 'readonly-sentinel-' + self.id[:12] + '.txt'
        status, seeded = self.request('/api/v1/userapp/generate-file',
                                      {'app_id': self.app, 'user_id': self.user,
                                       'file_name': filename, 'content': self.id}, self.entries[0],
                                      {'X-App-Id': self.app})
        if status != 200 or seeded.get('success') is not True:
            raise AssertionError('Development sentinel file creation failed')
        self.dev_git('init', entry=self.entries[0])
        committed = self.dev_git('commit', {'files': [filename], 'message': 'e2e readonly locator ' + self.id}, self.entries[0])
        commit = committed.get('commit')
        if not isinstance(commit, str) or not re.fullmatch(r'[0-9a-f]{40}', commit):
            raise AssertionError('Sentinel seed did not create an actual Git commit')
        data_before = self.dev_data_proof(filename, commit)
        before = self.dev_controller()
        uid = before['metadata']['uid']
        claims = self.app_pvc_identities(self.inventory())
        evidence = {'controller_uid': uid, 'old_pod_uid': self.builder_uid, 'claims': claims,
                    'data_before': data_before, 'reads': [], 'versions': [],
                    'cross_capture_rv_deterministic': False}
        self.save('dev-readonly-locator.json', evidence)
        self.check('dev_readonly_seed_data', True, data_before)
        self.dev_sentinel = (filename, commit, data_before)
        stop = self.dev_compute('stop', 120)
        self.check('dev_compute_stop_identity', True, stop)
        deadline = time.monotonic() + 90
        stopped = self.stopped_dev_view(uid, deadline)
        self.check('dev_stopped_resource_identity', stopped['claims'] == claims, stopped)
        key = 'rcoder.e2e/readonly-locator-' + self.id[:12]
        row = self.dev_controller(uid, deadline)
        if key in (row['metadata'].get('annotations') or {}):
            raise AssertionError('Refusing an existing test annotation')
        self.dev_readonly_annotation = {'uid': uid, 'key': key, 'values': set()}
        failure = None
        try:
            # Concurrent reads and UID/RV-conditioned metadata writes exercise
            # genuine API-server RV changes without changing the Pod template.
            # The report deliberately makes no per-capture scheduling claim.
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                for index in range(4):
                    futures = [pool.submit(self.readonly_dev_pair, entry, deadline) for entry in self.entries[:2]]
                    evidence['versions'].append(self.patch_readonly_annotation(uid, self.id + ':' + str(index), deadline))
                    evidence['reads'].extend(future.result() for future in futures)
                    current = self.stopped_dev_view(uid, deadline)
                    if current['claims'] != claims:
                        raise AssertionError('Read-only query changed a PVC identity')
                    self.save('dev-readonly-locator.json', evidence)
            lifecycle = self.api('/api/v1/userapp/' + self.app + '/lifecycle?'
                                 + urllib.parse.urlencode({'user_id': self.user}),
                                 timeout=remaining_timeout(deadline, 15))
            remaining_timeout(deadline, 15)
            if (lifecycle.get('lifecycle_id') != self.lifecycle_id or lifecycle.get('state') != 'Active'
                    or lifecycle.get('lifecycle_epoch') != 1):
                raise AssertionError('Development lifecycle changed during read-only queries')
            self.check('dev_stopped_rv_advanced', len(set(evidence['versions'])) == 4, evidence['versions'])
            self.check('dev_stopped_storage_both_replicas', {r['entry'] for r in evidence['reads'] if r['storage_nonorphan']} == {0, 1})
            self.check('dev_stopped_logs_both_replicas', {r['entry'] for r in evidence['reads'] if r['logs_stopped']} == {0, 1})
            self.check('dev_stopped_no_wake', True, current)
        except (Exception, KeyboardInterrupt) as error:
            failure = error
        try:
            self.remove_readonly_annotation()
        except (Exception, KeyboardInterrupt) as cleanup_error:
            evidence['annotation_cleanup_error'] = repr(cleanup_error)
            evidence['scenario_error'] = repr(failure) if failure else None
            self.save('dev-readonly-locator.json', evidence)
            if failure:
                raise RuntimeError('Read-only scenario and annotation cleanup both failed: ' + repr(cleanup_error)) from failure
            raise
        if failure:
            raise failure
        self.check('dev_readonly_annotation_removed', True)
        restart_deadline = time.monotonic() + 300
        restarted = self.dev_compute('restart', 300, deadline=restart_deadline)
        controller, builders = self.observe_restarted_dev(uid, claims, restart_deadline)
        valid = (len(builders) == 1 and builders[0]['uid'] != self.builder_uid
                 and builders[0].get('phase') == 'Running'
                 and any(c.get('type') == 'Ready' and c.get('status') == 'True' for c in builders[0].get('conditions', []))
                 and controller.get('spec', {}).get('replicas') == 1)
        self.check('dev_compute_restart_identity', valid, {'operation': restarted, 'pods': builders})
        self.builder_uid = builders[0]['uid']
        data_after = self.dev_data_proof(filename, commit)
        after_claims = self.app_pvc_identities(self.inventory())
        evidence.update(new_pod_uid=self.builder_uid, data_after=data_after,
                        controller_uid_after=controller['metadata']['uid'], claims_after=after_claims)
        self.save('dev-readonly-locator.json', evidence)
        self.check('dev_readonly_data_preserved', data_before == data_after and claims == after_claims, evidence)

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
            archive.writestr('worker/project.manifest.toml', '''schema_version = 1
[project]
service_id = "worker"
name = "Process worker acceptance"
type = "python"
kind = "worker"
[build]
command = ["python3", "-c", "from zipfile import ZipFile; z=ZipFile('artifact.zip','w'); z.write('main.py'); z.close()"]
artifact = "artifact.zip"
[devbuild]
command = ["python3", "-m", "py_compile", "main.py"]
[devrun]
command = ["python3", "main.py"]
[run]
command = ["python3", "main.py"]
[health]
startup_probe = "process"
''')
            archive.writestr('worker/main.py', 'import time\nwhile True:\n    time.sleep(1)\n')
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
        import tomllib
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            locked = tomllib.loads(archive.read('release.lock.toml').decode())
        workers = [s for s in locked['services'] if s['service_id'] == 'worker']
        self.check('worker_artifact_contract', len(workers) == 1 and workers[0]['health'].get('startup_probe') == 'process'
                   and workers[0]['kind'] == 'worker' and 'proxy' not in workers[0], workers)
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

    def compute_history(self, receipts, lifecycle):
        observed = []
        for receipt in receipts:
            if (receipt.get('app_id') != self.app or receipt.get('lifecycle_id') != lifecycle
                    or receipt.get('action') not in ('stop', 'restart')
                    or not dev_compute_view(receipt, self.app, lifecycle, receipt.get('action'),
                                            receipt.get('operation_id'))):
                raise AssertionError('Historical compute receipt is not this application lifecycle')
            view = self.api('/computer/pod/operations/' + self.app + '/' + receipt['operation_id'],
                            base=self.entries[-1])
            if not historical_compute_view_preserved(receipt, view):
                raise AssertionError('Historical compute receipt changed')
            observed.append(view)
        return observed

    def capture_recreated_resources(self, filename):
        rows = [row for row in self.inventory() if self.owned(row)]
        self.save(filename, rows)
        baseline_uids = {row['uid'] for row in self.baseline}
        if any(row['uid'] in baseline_uids for row in rows):
            raise AssertionError('Refusing recreated cleanup of a preexisting resource identity')
        claims = owned_userapp_pvc_identities(rows, self.app, self.lifecycle_id,
                                             baseline_uids)
        return rows, claims

    def recreated_runtime_identity(self):
        rows = [row for row in self.inventory() if self.owned(row)
                and row['kind'] in ('StatefulSet', 'Deployment', 'Pod', 'PersistentVolumeClaim')]
        controller = self.dev_controller()
        return {'resources': sorted((row['kind'], row['name'], row['uid'], row.get('generation')) for row in rows),
                'dev_replicas': controller.get('spec', {}).get('replicas')}

    def recreate_l1_business(self):
        """Seed actual L1 business and public history without traffic/lock fault scenarios."""
        filename = 'recreate-l1-sentinel-' + self.id[:12] + '.txt'
        status, result = self.request('/api/v1/userapp/generate-file',
            {'app_id': self.app, 'user_id': self.user, 'file_name': filename, 'content': self.id},
            self.entries[0], {'X-App-Id': self.app})
        if status != 200 or result.get('success') is not True:
            raise AssertionError('L1 sentinel write failed')
        self.dev_git('init', entry=self.entries[0])
        committed = self.dev_git('commit', {'files': [filename], 'message': 'e2e recreate L1 ' + self.id},
                                  self.entries[0])
        commit = committed.get('commit')
        if not isinstance(commit, str) or not re.fullmatch(r'[0-9a-f]{40}', commit):
            raise AssertionError('L1 fixture did not create an actual Git commit')
        data_before = self.dev_data_proof(filename, commit)
        self.check('recreate_l1_files_git', True, data_before)
        artifact, _release = self.build('L1')
        request_id = 'recreate-l1-deploy-' + self.id
        deployed = self.api('/api/v1/userapp/' + self.app + '/start',
            {'user_id': self.user, 'lifecycle_id': self.lifecycle_id, 'request_id': request_id, **artifact},
            self.entries[0], timeout=360)
        operation = self.operation_view(request_id)
        self.check('recreate_l1_deploy_operation', operation and operation['kind'] == 'StartDeployment'
                   and operation['scope'] == 'Prod' and operation['state'] == 'Succeeded'
                   and deployed.get('operation_id') == operation['operation_id'], operation)
        self.check('recreate_l1_content', self.content(self.id + '-L1'))
        controller = self.dev_controller()
        claims = self.app_pvc_identities(self.inventory())
        stop = self.dev_compute('stop', 120, request_id='recreate-l1-stop-' + self.id,
                                evidence_prefix='recreate-l1-dev')
        stopped = self.stopped_dev_view(controller['metadata']['uid'], time.monotonic() + 90)
        deadline = time.monotonic() + 300
        restart = self.dev_compute('restart', 300, deadline=deadline,
                                   request_id='recreate-l1-restart-' + self.id,
                                   evidence_prefix='recreate-l1-dev')
        _controller, builders = self.observe_restarted_dev(controller['metadata']['uid'], claims, deadline)
        self.builder_uid = builders[0]['uid']
        history = [stop, restart]
        observed = self.compute_history(history, self.lifecycle_id)
        self.check('recreate_l1_compute_history', len(observed) == 2
                   and stop['operation_id'] != restart['operation_id']
                   and {row['action'] for row in observed} == {'stop', 'restart'}
                   and stopped['claims'] == claims, observed)
        data_after = self.dev_data_proof(filename, commit)
        self.check('recreate_l1_data_preserved', data_before == data_after
                   and set(claims).issubset(self.app_pvc_identities(self.inventory()))
                   and self.content(self.id + '-L1'), {'before': data_before, 'after': data_after})
        self.save('recreate-l1-business.json', {'lifecycle_id': self.lifecycle_id,
                  'history': observed, 'claims': claims, 'data': data_after})

    def recreate_lifecycle(self):
        """Delete/recreate is an explicit business phase, never finally cleanup."""
        if not self.created or not self.lifecycle_id:
            raise AssertionError('Recreate requires this invocation\'s registered L1')
        old_lifecycle = self.lifecycle_id
        rows, old_claims = self.capture_recreated_resources('before-cleanup.json')
        history = [row for row in getattr(self, 'compute_receipts', [])
                   if row.get('lifecycle_id') == old_lifecycle]
        self.save('recreate-compute-history-before.json', history)
        self.delete_owned_resources(rows)
        self.poll(lambda: [row for row in self.inventory() if self.owned(row)], lambda rows: not rows, 180)
        tombstone = self.lifecycle()
        self.save('lifecycle-tombstone.json', tombstone)
        self.check('tombstone_deleted', tombstone['state'] == 'Deleted'
                   and tombstone['lifecycle_id'] == old_lifecycle, tombstone)
        status, rejected = self.request('/api/v1/userapp/workspace',
                                        {'app_id': self.app, 'user_id': self.user}, timeout=120)
        self.save('tombstone-ensure.json', rejected)
        self.check('tombstone_old_ensure_rejected', status == 200 and rejected.get('code') != '0000', rejected)
        recreated = self.api('/api/v1/userapp/' + self.app + '/recreate',
            {'user_id': self.user, 'expected_lifecycle_id': old_lifecycle,
             'request_id': 'recreate-' + self.id}, timeout=120)
        self.save('lifecycle-recreated.json', recreated)
        # Capture the newly acknowledged lifecycle before assertions so a later
        # business failure still permits only its exact original-API cleanup.
        if isinstance(recreated.get('lifecycle_id'), str) and recreated['lifecycle_id']:
            self.lifecycle_id = recreated['lifecycle_id']
            self.recreated = self.lifecycle_id != old_lifecycle
        self.check('recreate_new_lifecycle', self.recreated
                   and recreated['lifecycle_epoch'] == tombstone['lifecycle_epoch'] + 1
                   and recreated['state'] == 'Active', recreated)
        self.recreated_business(old_lifecycle, history, old_claims)

    def recreated_business(self, old_lifecycle, history, old_claims):
        """Use the recreated lifecycle through business APIs, then prove old fences still hold."""
        payload = {'app_id': self.app, 'user_id': self.user}
        for entry in self.entries[:2]:
            self.api('/api/v1/userapp/workspace', payload, entry, timeout=240)
        rows, claims = self.capture_recreated_resources('recreate-after-ensure.json')
        controller = self.dev_controller()
        builders = [row for row in rows if row['kind'] == 'Pod'
                    and row['name'].startswith('rcoder-app-builder-' + self.app + '-')]
        self.check('recreate_workspace_usable', len(builders) == 1 and bool(claims)
                   and controller.get('spec', {}).get('replicas') == 1, rows)
        self.builder_uid = builders[0]['uid']
        self.check('recreate_owned_pvc_identity',
                   not ({uid for _name, uid in claims} & {uid for _name, uid in old_claims}),
                   {'previous': old_claims, 'current': claims})

        filename = 'recreated-sentinel-' + self.id[:12] + '.txt'
        for index, entry in enumerate(self.entries):
            status, result = self.request('/api/v1/userapp/generate-file',
                                          {**payload, 'file_name': 'replica-' + str(index) + '.txt',
                                           'content': self.id}, entry, {'X-App-Id': self.app})
            if status != 200 or result.get('success') is not True:
                raise AssertionError('Recreated cross-replica file write failed')
        status, result = self.request('/api/v1/userapp/generate-file',
                                      {**payload, 'file_name': filename, 'content': self.id},
                                      self.entries[0], {'X-App-Id': self.app})
        if status != 200 or result.get('success') is not True:
            raise AssertionError('Recreated sentinel write failed')
        self.dev_git('init', entry=self.entries[0])
        committed = self.dev_git('commit', {'files': [filename], 'message': 'e2e recreated lifecycle ' + self.id},
                                  self.entries[0])
        commit = committed.get('commit')
        if not isinstance(commit, str) or not re.fullmatch(r'[0-9a-f]{40}', commit):
            raise AssertionError('Recreated fixture did not create an actual Git commit')
        data_before = self.dev_data_proof(filename, commit)
        self.check('recreate_files_git', True, data_before)
        artifact, _release = self.build('L2')
        request_id = 'recreate-deploy-' + self.id
        deployed = self.api('/api/v1/userapp/' + self.app + '/start',
                            {'user_id': self.user, 'lifecycle_id': self.lifecycle_id,
                             'request_id': request_id, **artifact}, self.entries[0], timeout=360)
        operation = self.operation_view(request_id)
        self.check('recreate_deploy_operation', operation and operation['kind'] == 'StartDeployment'
                   and operation['scope'] == 'Prod' and operation['state'] == 'Succeeded'
                   and deployed.get('operation_id') == operation['operation_id'], operation)
        self.check('recreate_content_L2', self.content(self.id + '-L2'))

        # Freeze actual L2 resources before sending explicitly stale L1 intents.
        # A code-only refusal is insufficient if it also changed the current Pod.
        before_stale = self.recreated_runtime_identity()
        rejected = []
        for action in ('stop', 'restart'):
            status, response = self.request('/computer/pod/' + action,
                {'app_id': self.app, 'app_stage': 'dev', 'service_type': 'userapp',
                 'lifecycle_id': old_lifecycle, 'request_id': 'recreate-old-' + action + '-' + self.id},
                self.entries[0], timeout=90)
            rejected.append({'action': action, 'status': status, 'response': response})
            if not stale_lifecycle_rejected(status, response, compute=True):
                raise AssertionError('Old lifecycle compute request was not rejected: ' + str(response))
        status, response = self.request('/api/v1/userapp/' + self.app + '/start',
            {'user_id': self.user, 'lifecycle_id': old_lifecycle,
             'request_id': 'recreate-old-deploy-' + self.id, **artifact}, self.entries[-1], timeout=90)
        rejected.append({'action': 'start_deployment', 'status': status, 'response': response})
        after_stale = self.recreated_runtime_identity()
        self.save('recreate-stale-lifecycle.json', {'before': before_stale, 'after': after_stale,
                                                  'responses': rejected})
        self.check('recreate_old_lifecycle_rejected', stale_lifecycle_rejected(status, response)
                   and before_stale == after_stale and self.content(self.id + '-L2'), rejected)

        claims_before_stop = self.app_pvc_identities(self.inventory())
        controller_uid = controller['metadata']['uid']
        stopped = self.dev_compute('stop', 120, request_id='recreate-stop-' + self.id,
                                   evidence_prefix='recreate-dev')
        view = self.stopped_dev_view(controller_uid, time.monotonic() + 90)
        old_ids = {row['operation_id'] for row in history}
        self.check('recreate_compute_stop', stopped['operation_id'] not in old_ids
                   and view['claims'] == claims_before_stop, {'operation': stopped, 'resources': view})
        restart_deadline = time.monotonic() + 300
        restarted = self.dev_compute('restart', 300, deadline=restart_deadline,
                                     request_id='recreate-restart-' + self.id, evidence_prefix='recreate-dev')
        _controller, builders = self.observe_restarted_dev(controller_uid, claims_before_stop, restart_deadline)
        self.check('recreate_compute_restart', restarted['operation_id'] not in old_ids
                   and restarted['operation_id'] != stopped['operation_id'] and len(builders) == 1,
                   {'operation': restarted, 'pods': builders})
        self.builder_uid = builders[0]['uid']
        data_after = self.dev_data_proof(filename, commit)
        _rows, final_claims = self.capture_recreated_resources('recreate-after-business.json')
        current = self.lifecycle()
        self.check('recreate_data_preserved', data_before == data_after
                   and set(claims).issubset(final_claims) and current['lifecycle_id'] == self.lifecycle_id
                   and current['state'] == 'Active' and self.content(self.id + '-L2'),
                   {'before': data_before, 'after': data_after, 'claims': final_claims})
        observed_history = self.compute_history(history, old_lifecycle)
        self.save('recreate-compute-history.json', {'before': history, 'after': observed_history})
        self.check('recreate_history_preserved', bool(history)
                   and {row['action'] for row in history} == {'stop', 'restart'}
                   and len(observed_history) == len(history)
                   and all(historical_compute_view_preserved(before, after)
                           for before, after in zip(history, observed_history)), observed_history)

    def delete_owned_resources(self, rows):
        """Reuse exact per-lifecycle requests if finally follows an unknown reply."""
        identity = hashlib.sha256((self.id + ':' + (self.lifecycle_id or 'unresolved')).encode()).hexdigest()[:32]
        payload = {'user_id': self.user}
        if self.lifecycle_id:
            payload['lifecycle_id'] = self.lifecycle_id
        if any(row['kind'] == 'Deployment' and row['name'] == 'rcoder-app-' + self.app for row in rows):
            self.api_retry_operation_busy('/api/v1/userapp/' + self.app + '/prod/delete',
                {**payload, 'purge': True, 'request_id': 'e2e-prod-delete-' + identity})
        self.api_retry_operation_busy('/api/v1/userapp/' + self.app + '/delete/app',
            {**payload, 'request_id': 'e2e-app-delete-' + identity})

    def cleanup_recreated(self):
        """Cleanup remains eligible even if a recreated business assertion fails."""
        rows, _claims = self.capture_recreated_resources('recreate-before-cleanup.json')
        self.delete_owned_resources(rows)
        final = self.lifecycle()
        self.check('recreate_deleted', final['state'] == 'Deleted'
                   and final['lifecycle_id'] == self.lifecycle_id, final)
        remaining = self.poll(lambda: [row for row in self.inventory() if self.owned(row)], lambda rows: not rows, 180)
        self.check('cleanup', not remaining, remaining)

    def prod_pod(self, deadline=None):
        rows = self.inventory(deadline=deadline)
        pods = [r for r in rows if r['kind'] == 'Pod' and r['name'].startswith('rcoder-app-' + self.app + '-')]
        if len(pods) != 1:
            raise AssertionError('Expected one production pod: ' + str(pods))
        return pods[0]

    # ===== 操作占用：Stop/Delete/Start 快失败；Restart 可受理前等待 =====

    def wait_deployment_quiesced(self, interval=3, budget=90):
        """等待应用 Deployment 的 resourceVersion 静默（连续两次读取不变）。

        env 收敛比对 resourceVersion 判并发，kube-controller 的 status 写入
        同样推进它——滚动刚结束的 status 抖动会让紧随其后的热部署误报
        "changed concurrently" 并落 RecoveryRequired（不可重试的租约保留）。
        """
        def version():
            row = json.loads(self.kube('get', 'deployment', 'rcoder-app-' + self.app, '-o', 'json'))
            return row['metadata']['resourceVersion']

        # 3 次连续不变才算静默——控制器 status 后续写入（readiness 探针
        # 周期 5s）可能滞后于首次稳定
        stable = 0
        last = version()
        deadline = time.monotonic() + budget
        while time.monotonic() < deadline:
            time.sleep(interval)
            current = version()
            if current == last:
                stable += 1
                if stable >= 2:
                    return
            else:
                stable = 0
                last = current
        self.check('lock_holder_deployment_quiesced', False,
                   {'last_resource_version': last, 'budget_s': budget})

    def lock_window_holder(self, artifact, admitted_check):
        """在副本0发起一个在途持有者（热部署），以操作受理记录为可观察同步点。

        返回 (holder_thread, rid, view_path, holder_outcome)。admitted_check：
        持有者被受理后需要满足的断言 id（不同窗口分别登记）。

        env 收敛以 Deployment resourceVersion 判并发——kube-controller 的
        status 写入同样推进 resourceVersion。持有者紧接 hot_deploy #2 启动会
        撞上滚动后 status 抖动误报 "changed concurrently"，且该失败落
        RecoveryRequired + 租约保留（不可重试）。消除竞态源：启动前等待
        Deployment resourceVersion 静默（连续两次读取不变）。
        """
        self.wait_deployment_quiesced()
        holder_outcome = []
        live = {'rid': None}

        def call():
            rid = 'lock-holder-' + uuid.uuid4().hex[:16]
            live['rid'] = rid
            status, envelope = self.request('/api/v1/userapp/' + self.app + '/start',
                                            {'user_id': self.user, 'request_id': rid,
                                             **artifact, 'deploy_mode': 'hot'},
                                            self.entries[0], timeout=300)
            holder_outcome.append((status, envelope, rid))

        thread = threading.Thread(target=call, daemon=True)
        thread.start()

        def view_path_for(rid):
            return ('/api/v1/userapp/' + self.app + '/operations/by-request?'
                    + urllib.parse.urlencode({'user_id': self.user, 'request_id': rid}))

        def current_admitted():
            rid = live['rid']
            if not rid:
                return None
            return self.operation_view(rid)

        operation = self.poll(current_admitted, bool, 120)
        self.check(admitted_check,
                   thread.is_alive() and operation.get('state') in ('Pending', 'Running', 'WaitingRetry')
                   and operation.get('app_id') == self.app and operation.get('lifecycle_id') == self.lifecycle_id
                   and operation.get('request_id') == live['rid'] and operation.get('kind') == 'StartDeployment'
                   and operation.get('scope') == 'Prod',
                   {'operation': operation, 'holder_http_pending': thread.is_alive()})
        return thread, live['rid'], view_path_for(live['rid'] or ''), holder_outcome, operation

    def lock_fail_fast(self, artifact_b):
        """忙锁窗口：外部 Stop/Delete 立即 ERR_OPERATION_IN_PROGRESS，
        不等待持有者、无副作用、不排队；释放后不自动执行。"""
        stop_query = '/api/v1/userapp/' + self.app + '/stop?' + urllib.parse.urlencode(
            {'user_id': self.user, 'request_id': 'lock-stop-' + self.id[:16]})
        thread, rid, view_path, holder_outcome, holder = self.lock_window_holder(artifact_b, 'lock_holder_admitted')
        timings = {}
        try:
            # 同实例：stop 打到持有者所在副本（进程内锁冲突）
            started = time.monotonic()
            status, envelope = self.request(stop_query, {}, base=self.entries[0], timeout=30)
            timings['stop_same_replica_ms'] = int((time.monotonic() - started) * 1000)
            self.check('lock_busy_stop_conflict',
                       busy_matches_holder(status, envelope, holder),
                       {'status': status, 'envelope': envelope, 'elapsed_ms': timings['stop_same_replica_ms']})
            # 跨副本：stop 打到另一 rcoder 实例（ConfigMap 租约互斥）
            started = time.monotonic()
            status, cross = self.request(stop_query, {}, base=self.entries[1], timeout=30)
            timings['stop_other_replica_ms'] = int((time.monotonic() - started) * 1000)
            self.check('lock_cross_replica_conflict',
                       busy_matches_holder(status, cross, holder),
                       {'status': status, 'envelope': cross, 'elapsed_ms': timings['stop_other_replica_ms']})
            # Delete remains fail-fast; Restart has its own admission-wait case.
            started = time.monotonic()
            status, deleted = self.request('/api/v1/userapp/' + self.app + '/prod/delete',
                                           {'user_id': self.user, 'purge': False,
                                            'request_id': 'lock-delete-' + self.id[:16]},
                                           base=self.entries[1], timeout=30)
            timings['delete_ms'] = int((time.monotonic() - started) * 1000)
            self.check('lock_busy_delete_conflict',
                       busy_matches_holder(status, deleted, holder),
                       {'status': status, 'envelope': deleted, 'elapsed_ms': timings['delete_ms']})
            # 冲突请求已返回而持有者仍在途：证明没有等待持锁业务完成
            _, view = self.request(view_path, base=self.entries[1])
            operation = view.get('data') or {}
            self.check('lock_holder_still_inflight',
                       operation.get('operation_id') == holder['operation_id']
                       and operation.get('state') in ('Pending', 'Running', 'WaitingRetry')
                       and thread.is_alive(),
                       {'operation': operation, 'holder_http_pending': thread.is_alive()})
        finally:
            thread.join(300)
        holder_status, holder_envelope, _ = holder_outcome[0] if holder_outcome else (0, {}, '')
        self.check('lock_holder_completes',
                   holder_status == 200 and holder_envelope.get('code') == '0000'
                   and holder_envelope.get('operation_id') == holder['operation_id'],
                   {'status': holder_status, 'envelope': holder_envelope})
        # 释放后不自动执行被拒请求：给假想的排队 stop 一个执行窗口后，
        # 应用仍应 1 副本 Running、无 Stop 操作记录、内容不受影响
        time.sleep(2)
        pod = self.prod_pod()
        rejected = []
        for prefix, kind in [('lock-stop-', 'Stop'), ('lock-delete-', 'DeleteCompute')]:
            view = self.operation_view(prefix + self.id[:16])
            # Identity was verified by operation_view. A real rejected record
            # may exist, but an accepted/executed or unrelated record may not pass.
            rejected.append({'kind': kind, 'operation': view,
                             'unexecuted': rejected_without_execution(view, kind)})
        self.check('lock_rejected_no_auto_execution',
                   pod['phase'] == 'Running' and all(row['unexecuted'] for row in rejected),
                   {'pod': pod, 'rejected': rejected})
        self.check('lock_no_side_effects', self.content(self.id + '-B'))
        runtime = self.api('/api/v1/userapp/' + self.app + '?' + urllib.parse.urlencode({'user_id': self.user}))
        self.pre_stop_version = runtime.get('resource_version')
        self.save('lock-failfast.json', {'timings': timings, 'pre_stop_version': self.pre_stop_version})

    def operation_view(self, request_id, deadline=None):
        path = '/api/v1/userapp/' + self.app + '/operations/by-request?' + urllib.parse.urlencode(
            {'user_id': self.user, 'request_id': request_id})
        status, envelope = self.request(path, base=self.entries[1],
                                        timeout=remaining_timeout(deadline, 10))
        remaining_timeout(deadline, 10)
        if (status != 200 or not isinstance(envelope, dict) or envelope.get('code') != '0000'
                or envelope.get('success') is not True or 'data' not in envelope):
            raise AssertionError('Original operation query failed: ' + str(envelope))
        # For a known application by-request absence is an explicit successful
        # null result. ERR_NOT_FOUND belongs to the explicit operation-ID route.
        view = envelope['data']
        if view is not None and (
                not isinstance(view, dict) or not view.get('operation_id')
                or view.get('app_id') != self.app or view.get('lifecycle_id') != self.lifecycle_id
                or view.get('request_id') != request_id):
            raise AssertionError('Original operation query changed identity: ' + str(view))
        return view

    def observe_restart_rollout(self, old_uid, operation_id, deadline):
        path = f'/api/v1/userapp/proxy/app/prod/{self.user}/{self.app}/'
        while True:
            remaining_timeout(deadline, 90)
            rows = self.inventory(deadline=deadline)
            remaining_timeout(deadline, 90)
            pods = [row for row in rows if row['kind'] == 'Pod'
                    and row['name'].startswith('rcoder-app-' + self.app + '-') and self.owned(row)]
            # This is an observation filter, not a grant to adopt or control a
            # same-name object. The original successful operation owns replay.
            if len(pods) == 1 and restarted_pod_ready(pods[0], self.app, old_uid, operation_id):
                status, content = self.request(path, base=self.args.proxy_url.rstrip('/'), raw=True,
                                               timeout=remaining_timeout(deadline, 75))
                remaining_timeout(deadline, 75)
                if status != 200:
                    raise AssertionError(f'Restart content query failed: HTTP {status}')
                if content.decode() == self.id + '-B':
                    return pods[0]
            time.sleep(remaining_timeout(deadline, .2))

    def replay_deployment_view(self, deadline):
        rows = self.inventory(deadline=deadline)
        remaining_timeout(deadline, 90)
        deployments = [row for row in rows if row['kind'] == 'Deployment'
                       and row['name'] == 'rcoder-app-' + self.app and self.owned(row)]
        if len(deployments) != 1:
            raise AssertionError('Original production Deployment is not uniquely observable')
        deployment = deployments[0]
        annotations = deployment.get('annotations') or {}
        labels = deployment.get('labels') or {}
        if (not deployment.get('uid') or type(deployment.get('generation')) is not int
                or labels.get('app.kubernetes.io/instance') != self.app
                or labels.get('app.kubernetes.io/managed-by') != 'rcoder-app-manager'
                or annotations.get('rcoder.io/application-id') != self.app
                or annotations.get('rcoder.io/lifecycle-id') != self.lifecycle_id):
            raise AssertionError('Original production Deployment identity changed')
        # Status/RV may progress independently; a new rollout changes generation.
        return {'uid': deployment['uid'], 'generation': deployment['generation'],
                'app_id': self.app, 'lifecycle_id': self.lifecycle_id}

    def lock_restart_wait(self, artifact_b):
        """Restart may wait before admission; it must never overlap its holder.

        A real hot deployment can finish inside the default 30-second admission
        window. In that case this request must own a distinct successful Restart.
        If the budget expires, it must remain unaccepted and never execute later.
        """
        thread, rid, _, holder_outcome, holder = self.lock_window_holder(artifact_b, 'lock_restart_holder_admitted')
        request_id = 'lock-restart-' + self.id[:16]
        body = {'user_id': self.user, 'request_id': request_id}
        path = '/api/v1/userapp/' + self.app + '/restart'
        timeout = RESTART_ADMISSION_WAIT_SECONDS + RESTART_EXECUTION_TIMEOUT_SECONDS
        # Capture the original physical Pod before sending this Restart. Its
        # normal rollout must converge before UID changes can diagnose a replay.
        old_uid = self.prod_pod()['uid']
        deadline = time.monotonic() + timeout
        outcome, observations = [], []

        def call():
            started = time.monotonic()
            status, envelope = self.request(path, body, base=self.entries[1],
                                            timeout=remaining_timeout(deadline, timeout))
            outcome.append((status, envelope, time.monotonic() - started))

        restart = threading.Thread(target=call, daemon=True)
        restart.start()
        try:
            while restart.is_alive():
                if time.monotonic() >= deadline:
                    raise TimeoutError('Restart exceeded admission plus execution budget')
                own = self.operation_view(request_id, deadline=deadline)
                if own is not None:
                    current_holder = self.operation_view(rid, deadline=deadline)
                    ordered = current_holder is not None and restart_admission_after_holder(own, current_holder)
                    observations.append({'own': own, 'holder': current_holder, 'ordered': ordered})
                    if not ordered:
                        self.check('restart_never_overlaps_holder', False, observations[-1])
                time.sleep(remaining_timeout(deadline, .2))
            if not outcome:
                raise AssertionError('Restart HTTP result was not confirmed')
            status, envelope, elapsed = outcome[0]
            own, final_holder = (self.operation_view(request_id, deadline=deadline),
                                 self.operation_view(rid, deadline=deadline))
            self.check('lock_busy_restart_conflict', final_holder is not None
                       and restart_result_matches(status, envelope, holder, final_holder, own, elapsed),
                       {'contract': 'wait then admit after holder, or exhaust budget without admission',
                        'status': status, 'envelope': envelope, 'elapsed_s': elapsed,
                        'holder': final_holder, 'own': own, 'admission_observations': observations})
        finally:
            for pending in (restart, thread):
                pending.join(max(0, deadline - time.monotonic()))
        holder_status, holder_envelope, _ = holder_outcome[0] if holder_outcome else (0, {}, '')
        self.check('restart_holder_completes', holder_status == 200 and holder_envelope.get('code') == '0000'
                   and holder_envelope.get('operation_id') == holder['operation_id'], holder_envelope)
        if own is None:
            time.sleep(remaining_timeout(deadline, 2))
            self.check('restart_rejected_no_auto_execution', self.operation_view(request_id, deadline=deadline) is None
                       and self.prod_pod(deadline=deadline)['phase'] == 'Running'
                       and self.content(self.id + '-B', deadline=deadline))
        else:
            pod = self.observe_restart_rollout(old_uid, own['operation_id'], deadline)
            before_replay = self.replay_deployment_view(deadline)
            self.check('restart_original_rollout_ready', True,
                       {'original_pod_uid': old_uid, 'restart_operation_id': own['operation_id'], 'pod': pod})
            status, replay = self.request(path, body, base=self.entries[0],
                                          timeout=remaining_timeout(deadline, timeout))
            remaining_timeout(deadline, timeout)
            replay_deployment = self.replay_deployment_view(deadline)
            after_replay = self.observe_restart_rollout(old_uid, own['operation_id'], deadline)
            self.check('restart_request_id_replay', status == 200 and replay.get('code') == '0000'
                       and replay.get('success') is True and replay.get('operation_id') == own['operation_id']
                       and before_replay == replay_deployment and after_replay['uid'] == pod['uid'],
                       {'response': replay, 'deployment_before': before_replay,
                        'deployment_after': replay_deployment, 'pod_uid_before': pod['uid'],
                        'pod_uid_after': after_replay['uid']})
            self.check('restart_wait_no_unrelated_effects', True,
                       {'pod_uid': after_replay['uid'], 'content': self.id + '-B'})

    def lock_start_waits(self, artifact_b):
        """忙锁窗口的第二探测：无 url start（HTTP 面）同为快失败——外部生命
        周期操作（deploy_controlled 批次语义）返回 ERR_OPERATION_IN_PROGRESS 不排队；
        排队等待语义属于内部路径（流量唤醒/回收器/恢复），由组件测试覆盖
        （Docker flock 轮询 + K8s 租约轮询单测）与套件 wake 流程验证。"""
        thread, rid, view_path, holder_outcome, holder = self.lock_window_holder(artifact_b, 'lock_holder_admitted_2')
        request_id = 'lock-start-' + self.id[:16]
        started = time.monotonic()
        status, envelope = self.request('/api/v1/userapp/' + self.app + '/start',
                                        {'user_id': self.user, 'request_id': request_id},
                                        base=self.entries[1], timeout=30)
        elapsed_ms = int((time.monotonic() - started) * 1000)
        current_holder = self.operation_view(rid)
        still_pending = (thread.is_alive() and current_holder is not None
                         and current_holder['operation_id'] == holder['operation_id']
                         and current_holder.get('state') in ('Pending', 'Running', 'WaitingRetry'))
        thread.join(300)
        self.check('lock_busy_start_conflict',
                   busy_matches_holder(status, envelope, holder) and still_pending,
                   {'status': status, 'envelope': envelope, 'elapsed_ms': elapsed_ms, 'holder': current_holder})
        holder_status, holder_envelope, _ = holder_outcome[0] if holder_outcome else (0, {}, '')
        self.check('start_holder_completes', holder_status == 200 and holder_envelope.get('code') == '0000'
                   and holder_envelope.get('operation_id') == holder['operation_id'], holder_envelope)
        time.sleep(2)
        self.check('start_rejected_no_auto_execution', self.operation_view(request_id) is None
                   and self.prod_pod()['phase'] == 'Running' and self.content(self.id + '-B'))

    def lease_dead_holder_takeover(self):
        """死持有者租约接管（coordination.Lease TTL 语义，替代原 ConfigMap
        永久残留）：预置 renewTime 一小时前的 rcoder-operation-prod-{app}
        Lease（模拟持有者副本崩溃/被 kill 后的残留锁），随后的控制操作必须
        在 acquire 的 409 分支判定过期并 CAS 接管成功——不报 occupied、无需
        人工 kubectl delete；操作完成按正常路径释放 Lease（对象消失）。
        修复前语义：409 一律 OperationInProgress，残留锁永久挡住该 app 的
        一切操作（09-21 app 141/154 事故形态，需人工 SQL + kubectl 解救）。"""
        lease_name = 'rcoder-operation-prod-' + self.app
        remote_path = '/tmp/rcoder-e2e-dead-lease.json'
        # coordination.Lease 的 acquireTime/renewTime 是 MicroTime（RFC3339 必须
        # 带 6 位小数秒）；秒级时间戳会被 API server 拒绝（r7 两轮确定性复现）。
        stale_epoch = time.time() - 3600
        stale = time.strftime('%Y-%m-%dT%H:%M:%S', time.gmtime(stale_epoch)) \
            + '.{:06d}Z'.format(int((stale_epoch % 1) * 1_000_000))
        manifest = {
            'apiVersion': 'coordination.k8s.io/v1', 'kind': 'Lease',
            'metadata': {'name': lease_name, 'namespace': self.args.namespace},
            'spec': {
                'holderIdentity': 'dead-holder:dead-operation',
                'leaseDurationSeconds': 60,
                'acquireTime': stale, 'renewTime': stale,
            },
        }
        subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
                        self.args.ssh, 'cat > ' + remote_path],
                       input=json.dumps(manifest), text=True, check=True, timeout=30)
        self.kube('apply', '-f', remote_path)
        placed = json.loads(self.kube('get', 'lease', lease_name, '-o', 'json'))
        placed_holder = (placed.get('spec') or {}).get('holderIdentity')
        restart_request = 'lease-takeover-' + uuid.uuid4().hex[:16]
        started = time.monotonic()
        status, envelope = self.request('/api/v1/userapp/' + self.app + '/restart',
                                        {'user_id': self.user, 'request_id': restart_request},
                                        timeout=300)
        elapsed_ms = int((time.monotonic() - started) * 1000)
        occupied = 'occupied' in envelope.get('message', '') or envelope.get('code') == 'ERR_CONFLICT'
        self.check('lease_dead_holder_takeover',
                   placed_holder == 'dead-holder:dead-operation'
                   and status == 200 and envelope.get('code') == '0000'
                   and not occupied and bool(envelope.get('operation_id')),
                   {'placed_holder': placed_holder, 'status': status,
                    'envelope': envelope, 'elapsed_ms': elapsed_ms})
        # 接管后的正常收尾：操作成功即显式释放（uid + 实时 RV precondition），
        # Lease 对象消失——证明接管走完了与正常 acquire 完全相同的生命周期。
        # poll 超时抛 TimeoutError：收敛为失败断言（带残留证据），不崩套件。
        try:
            self.poll(lambda: self.lease_rows(lease_name), lambda rows: not rows, 60)
            released = True
            residue = []
        except TimeoutError as error:
            released = False
            residue = self.lease_rows(lease_name)
            print('lease residue after operation: ' + str(residue)[:400], flush=True)
        self.check('lease_released_after_operation', released,
                   {'lease': lease_name, 'still_present': residue,
                    'poll_error': None if released else str(error)[:400]})
        self.kube('delete', '-f', remote_path, '--ignore-not-found')

    def lease_rows(self, name):
        try:
            data = json.loads(self.kube('get', 'lease', name, '-o', 'json'))
        except subprocess.CalledProcessError:
            return []
        return [data] if data.get('metadata', {}).get('name') == name else []

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


    def content(self, expected, deadline=None):
        path = f'/api/v1/userapp/proxy/app/prod/{self.user}/{self.app}/'
        def read():
            # Traffic wake queues the request until the replacement pod is
            # ready (~24s observed under load); a per-request timeout tighter
            # than that window aborts the whole case on a healthy wake path.
            return self.request(path, base=self.args.proxy_url.rstrip('/'), raw=True,
                                timeout=remaining_timeout(deadline, 75))
        if deadline is None:
            result = self.poll(read, lambda r: r[0] == 200 and r[1].decode() == expected, 180)
        else:
            while True:
                remaining_timeout(deadline, 75)
                result = read()
                remaining_timeout(deadline, 75)
                if result[0] != 200:
                    raise AssertionError(f'Original content query failed: HTTP {result[0]}')
                if result[1].decode() == expected:
                    break
                time.sleep(remaining_timeout(deadline, 2))
        return result[0] == 200

    def traffic_wake(self, previous_pod, claims_before, data_before):
        """The first concurrent business requests must wake compute without ensure/start."""
        if len(self.proxy_entries) < 2 or len(set(self.proxy_entries[:2])) != 2:
            raise AssertionError('Two distinct captured replica proxy entries are required for traffic wake')
        path = f'/api/v1/userapp/proxy/app/prod/{self.user}/{self.app}/'
        deadline = time.monotonic() + 180
        # The two SSH forwards bind these requests to separate captured main
        # replicas, rather than depending on an unspecified load-balancer choice.
        def first_request(entry):
            result = self.first_proxy_get(path, entry, deadline)
            remaining_timeout(deadline, 150)
            return result
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(first_request, self.proxy_entries[:2]))
        self.check('traffic_wake_without_start',
                   all(status == 200 and content == (self.id + '-B').encode() for status, content in results),
                   [{'entry': entry, 'status': status, 'sha256': hashlib.sha256(content).hexdigest()}
                    for entry, (status, content) in zip(self.proxy_entries[:2], results)])
        awakened = self.prod_pod()
        self.save('production-wake.json', awakened)
        self.check('traffic_wake_single_instance', awakened['uid'] != previous_pod['uid']
                   and awakened.get('phase') == 'Running'
                   and any(row.get('type') == 'Ready' and row.get('status') == 'True'
                           for row in awakened.get('conditions', [])), awakened)
        claims_after = self.app_pvc_identities(self.inventory())
        filename, commit, _initial = self.dev_sentinel
        data_after = self.dev_data_proof(filename, commit)
        self.check('traffic_wake_pvc_data_preserved', claims_before == claims_after and data_before == data_after,
                   {'claims_before': claims_before, 'claims_after': claims_after,
                    'data_before': data_before, 'data_after': data_after})
        self.check('wake', True)
        self.check('wake_content_B', True)
        self.check('wake_new_pod', awakened['uid'] != previous_pod['uid'], awakened['uid'])
        # Keep the former explicit-Start coverage, after traffic already restored
        # service. It must reuse the new Pod and cannot act as a hidden pre-wake.
        self.api('/api/v1/userapp/' + self.app + '/start', {'user_id': self.user}, timeout=240)
        explicit = self.prod_pod()
        self.check('wake_explicit_start_idempotent', explicit['uid'] == awakened['uid']
                   and self.content(self.id + '-B'), {'traffic_pod': awakened['uid'], 'explicit_pod': explicit['uid']})
        return awakened

    def worker_running(self, pod):
        # Inspect the process and its actual port from the lock; business HTTP
        # working alone does not prove that an enabled worker started correctly.
        code = '''import json,pathlib,socket,tomllib
roots=[]
for p in pathlib.Path('/proc').iterdir():
 if p.name.isdigit():
  try:
   argv=(p/'cmdline').read_bytes().split(b'\\0')
   if len(argv)>1 and argv[0].endswith(b'python3') and argv[1]==b'main.py': roots.append(p)
  except OSError: pass
out={'roots':[int(p.name) for p in roots]}
if len(roots)==1:
 cwd=(roots[0]/'cwd').resolve()
 lock=tomllib.loads((cwd.parent/'release.lock.toml').read_text())
 spec=next(s for s in lock['services'] if s['service_id']=='worker')
 s=socket.socket();s.settimeout(1);out['no_listener']=s.connect_ex(('127.0.0.1',spec['port']))!=0;s.close()
print(json.dumps(out))'''
        evidence = json.loads(self.kube('exec', pod['name'], '--', 'python3', '-c', code))
        self.check('worker_process_running', len(evidence['roots']) == 1 and evidence.get('no_listener') is True,
                   {'pod_uid': pod['uid'], **evidence})

    def deploy(self, artifact_a, release_a, artifact_b, release_b):
        path = '/api/v1/userapp/' + self.app + '/start'
        cold_request_id = 'cold-' + self.id[:20]
        before = time.monotonic()
        cold = self.api(path, {'user_id': self.user, 'request_id': cold_request_id, **artifact_a}, timeout=360)
        self.check('cold_deploy', time.monotonic() - before < 180, cold)
        self.check('content_A', self.content(self.id + '-A'))
        pod = self.prod_pod()
        self.save('production-A.json', pod)
        self.worker_running(pod)
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
        self.worker_running(after)
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
        # 操作占用/受理等待真实场景（持有者 = 热部署在途）
        self.lock_fail_fast(artifact_b)
        self.lock_restart_wait(artifact_b)
        self.lock_start_waits(artifact_b)
        self.lease_dead_holder_takeover()
        claims_before_stop = owned_userapp_pvc_identities(
            [row for row in self.inventory() if self.owned(row)], self.app, self.lifecycle_id,
            {row['uid'] for row in self.baseline})
        filename, commit, _initial = self.dev_sentinel
        data_before_stop = self.dev_data_proof(filename, commit)
        before_stop = self.prod_pod()
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
        self.traffic_wake(before_stop, claims_before_stop, data_before_stop)
        stable = self.lifecycle()
        self.check('lifecycle_stable_stop_wake', stable['lifecycle_id'] == self.lifecycle_id
                   and stable['state'] == 'Active' and stable['lifecycle_epoch'] == 1, stable)
        # delete 乐观锁护栏（stop/wake 换代后的过期版本必须被拒）
        self.delete_version_guard()

    def exercise(self):
        self.prepare()
        self.workspace()
        if self.scenario == 'recreate':
            self.recreate_l1_business()
        elif self.scenario == 'all':
            self.dev_readonly_locator()
            a, ra = self.build('A')
            b, rb = self.build('B')
            self.deploy(a, ra, b, rb)
        else:
            raise ValueError('Unknown K8s UserApp scenario')
        self.recreate_lifecycle()

    def cleanup(self):
        failure = None
        try:
            self.cleanup_owned()
        except (Exception, KeyboardInterrupt) as error:
            failure = error
        try:
            self.preserve_baseline()
        except (Exception, KeyboardInterrupt) as error:
            if failure:
                raise RuntimeError('Application cleanup and baseline preservation both failed: '
                                   + repr(error)) from failure
            raise
        if failure:
            raise failure

    def cleanup_owned(self):
        if self.created:
            if getattr(self, 'recreated', False):
                self.cleanup_recreated()
                return
            rows = self.inventory()
            owned = [r for r in rows if self.owned(r)]
            self.save('before-cleanup.json', owned)
            baseline_uids = {r['uid'] for r in self.baseline}
            if any(r['uid'] in baseline_uids for r in owned):
                raise AssertionError('Refusing cleanup of a preexisting identity')
            self.delete_owned_resources(owned)
            remaining = self.poll(lambda: [r for r in self.inventory() if self.owned(r)], lambda r: not r, 180)
            self.check('cleanup', not remaining, remaining)

    def preserve_baseline(self):
        after = self.inventory()
        self.save('after-cleanup.json', after)
        self.preserve_pvc_baseline(after)
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
    parser.add_argument('--kube-server', help='Explicit Kubernetes API server; normal TLS verification remains enabled')
    parser.add_argument('--environment-id')
    parser.add_argument('--deployment', default='nuwax-k8s-test-rcoder', help='RCoder workload name')
    parser.add_argument('--workload-kind', choices=['deployment', 'statefulset'], default='deployment',
                        help='Use statefulset for the Helm per-replica-cache deployment')
    parser.add_argument('--url', required=True)
    parser.add_argument('--proxy-url', required=True)
    parser.add_argument('--proxy-port', type=int, default=8088,
                        help='RCoder proxy listen port inside each captured main Pod')
    parser.add_argument('--internal-url', default='http://nuwax-k8s-test-rcoder:8086')
    parser.add_argument('--scenario', choices=SCENARIO_REQUIRED, default='all',
                        help='all keeps the full contract; recreate runs an independent L1-to-L2 lifecycle')
    args = parser.parse_args()
    run = Run(args)
    def interrupt(*_):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    error = None
    try:
        run.exercise()
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
        missing = run.required - {r['id'] for r in run.assertions if r['ok']}
        run.summary.update(error=error, missing=sorted(missing), verdict='pass' if not error and not missing and all(r['ok'] for r in run.assertions) and not run.summary.get('cleanup_error') else 'fail')
        run.save('summary.json', run.summary)
    print(json.dumps(run.summary, ensure_ascii=False), flush=True)
    return 0 if run.summary['verdict'] == 'pass' else 1


if __name__ == '__main__':
    raise SystemExit(main())
