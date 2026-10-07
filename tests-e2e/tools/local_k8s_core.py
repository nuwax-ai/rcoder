#!/usr/bin/env python3
"""真实本机 K8s Dev 计算回归；只在成功末尾 Stop，永不删除 PVC/namespace。

Compute operation 只有 GET 查询，不虚构 SSE 路由或任务 ID。失败保留现场。
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

from userapp_root_logs import source_snapshot


class ContractFailure(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise ContractFailure(message)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def safe(value):
    """Sanitize diagnostic exports; Pod env and private inputs are never collected."""
    if isinstance(value, dict):
        return {key: '[REDACTED]' if re.search(r'password|passwd|secret|token|api.?key', key, re.I)
                else safe(item) for key, item in value.items()}
    if isinstance(value, list):
        return [safe(item) for item in value]
    if isinstance(value, str):
        value = re.sub(r'([a-z][a-z0-9+.-]*://)[^\s/@]+(?::[^\s/@]*)?@', r'\1[REDACTED]@', value, flags=re.I)
        value = re.sub(r'(?i)(password|passwd|PGPASSWORD|POSTGRES_PASSWORD|token|api.?key)(\s*[:=]\s*)([^\s,;]+)', r'\1\2[REDACTED]', value)
        value = re.sub(r'(?i)(Bearer\s+)\S+', r'\1[REDACTED]', value)
    return value


def validate_inputs(root, url, app):
    root = Path(root).resolve()
    identity = json.loads((root / 'identity.json').read_text())
    require(identity.get('root') == str(root), 'identity.root must be this exact run directory')
    run_id = identity.get('run_id', '')
    require(re.fullmatch(r'[0-9a-f]{32}', run_id), 'a real root-declared run ID is required')
    namespace = identity.get('namespace', '')
    require(re.fullmatch(r'rcoder-[a-z0-9-]+', namespace) and namespace != 'rcoder-dev', 'use a new owned rcoder-* namespace, never rcoder-dev')
    require(identity.get('context') == 'orbstack', 'this harness is limited to the explicit orbstack context')
    server = urllib.parse.urlsplit(identity.get('cluster_api', ''))
    require(server.scheme == 'https' and server.hostname in ('127.0.0.1', 'localhost') and server.port
            and not server.username and not server.password, 'identity must bind the local cluster API')
    kubeconfig = Path(identity.get('kubeconfig', '')).resolve()
    require(kubeconfig.is_file() and kubeconfig.is_relative_to(root), 'explicit private kubeconfig must be inside the owned run directory')
    require(re.fullmatch(r'[a-z][a-z0-9]{0,21}', app), 'app must be a lowercase unique ID of at most 22 characters')
    require(app in identity.get('apps', []), 'root must declare this app in identity.apps before running')
    endpoint = urllib.parse.urlsplit(url)
    require(endpoint.scheme == 'http' and endpoint.hostname in ('127.0.0.1', 'localhost') and endpoint.port
            and not endpoint.username and not endpoint.password and not endpoint.query and not endpoint.fragment
            and endpoint.path in ('', '/'), 'RCoder URL must be an explicit local HTTP endpoint without credentials')
    return root, identity, kubeconfig, url.rstrip('/')


def resource_identity(resource):
    metadata = resource.get('metadata', {})
    return {key: metadata.get(key) for key in ['name', 'namespace', 'uid', 'resourceVersion']}


def builder_pvc_name(app):
    # This isolated local fixture explicitly uses the rcoder-app-builder prefix.
    require(re.fullmatch(r'[a-z][a-z0-9]{0,21}', app), 'PVC app must be the declared valid application ID')
    return 'rcoder-app-builder-' + app + '-workspace'


def validate_owned_resource(resource, namespace, app, family='user-app-builder', pvc=False):
    metadata = resource.get('metadata', {})
    labels = metadata.get('labels', {})
    require(metadata.get('namespace') == namespace and metadata.get('uid'), 'resource namespace/UID differs or is absent')
    require(labels.get('app.kubernetes.io/managed-by') == 'rcoder-runtime', 'resource is not managed by this runtime')
    if pvc:
        # Lifecycle-owned PVC creation does not require identifier labels or ownerReferences.
        require(metadata.get('name') == builder_pvc_name(app), 'PVC name is not the exact declared application workspace')
        require('rcoder.io/identifier' not in labels or labels['rcoder.io/identifier'] == app, 'PVC carries another application identifier')
    else:
        require(labels.get('rcoder.io/identifier') == app, 'resource identifier is not the declared app')
    require(labels.get('service_type' if pvc else 'rcoder.io/service-type') == family, 'resource family differs')


def validate_operation(record, app, lifecycle_id, operation_id, action):
    require(record.get('app_id') == app and record.get('lifecycle_id') == lifecycle_id
            and record.get('operation_id') == operation_id and record.get('action') == action
            and record.get('scope') == 'Dev', 'original compute operation identity changed')
    state = record.get('state')
    require(state in ('pending', 'running', 'succeeded'), 'compute operation did not succeed: ' + str(state))
    if state == 'succeeded':
        require(record.get('stage') == 'completed', 'success lacks the completed execution stage')
        return True
    return False


class Harness:
    def __init__(self, root, url, app, budget=1200):
        self.root, self.identity, self.kubeconfig, self.url = validate_inputs(root, url, app)
        self.app = app
        self.namespace = self.identity['namespace']
        self.deadline = time.monotonic() + budget
        self.namespace_uid = None
        self.lifecycle_id = None
        self.original = None
        self.last_operation = None
        self.pending_compute_request = None
        self.lock = threading.Lock()
        self.report_path = self.root / ('local-k8s-core-' + app + '.json')
        require(not self.report_path.exists(), 'use another declared app/report; old evidence is never overwritten')
        self.report = {'success': False, 'app_id': app, 'namespace': self.namespace, 'run_id': self.identity['run_id'],
                       'scope': 'real local K8s dev compute, no AI or Prod deployment', 'checks': [], 'timeline': [],
                       'sse_contract': 'compute operations have GET only; this compute-only run creates no business diagnostic task/SSE',
                       'policy': 'no PVC/namespace deletion, no purge, failure preserves evidence; successful final cleanup is compute Stop only'}
        self.persist()

    def persist(self):
        temporary = self.report_path.with_suffix('.json.tmp')
        temporary.write_text(json.dumps(safe(self.report), ensure_ascii=False, indent=2) + '\n')
        os.replace(temporary, self.report_path)

    def event(self, stage, value):
        with self.lock:
            self.report['timeline'].append({'at': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'stage': stage, 'evidence': safe(value)})
            self.persist()

    def check(self, name, condition, evidence):
        with self.lock:
            self.report['checks'].append({'name': name, 'passed': bool(condition), 'evidence': safe(evidence)})
            self.persist()
        require(condition, name)
        print(name + ' PASS', flush=True)

    def remaining(self, cap):
        remaining = self.deadline - time.monotonic()
        require(remaining > 0, 'parent harness deadline ended; do not dispatch new mutations')
        return min(cap, remaining)

    def kube(self, args, json_output=True):
        command = ['kubectl', '--kubeconfig', str(self.kubeconfig), '--context', 'orbstack', '--request-timeout=30s',
                   '--namespace', self.namespace, *args]
        result = subprocess.run(command, capture_output=True, text=True, timeout=self.remaining(45))
        if result.returncode != 0:
            self.event('kubectl_failed', {'arguments': args[:4], 'exit_code': result.returncode, 'cause': result.stderr})
            raise ContractFailure('kubectl failed: ' + safe(result.stderr.strip()))
        return json.loads(result.stdout) if json_output else result.stdout.strip()

    def ownership(self):
        namespace = self.kube(['get', 'namespace', self.namespace, '-o', 'json'])
        metadata = namespace['metadata']
        require(metadata.get('labels', {}).get('rcoder.e2e.owner') == self.identity['run_id'], 'namespace owner label changed')
        require(metadata.get('uid'), 'namespace UID absent')
        if self.namespace_uid is None:
            self.namespace_uid = metadata['uid']
        require(metadata['uid'] == self.namespace_uid, 'namespace was replaced; no further mutations authorized')
        return resource_identity(namespace)

    def http(self, method, route, payload=None, mutation=False):
        if mutation:
            self.ownership()
        require(route.startswith('/') and not route.startswith('//'), 'HTTP route must stay on the selected RCoder')
        request = urllib.request.Request(self.url + route, method=method,
                                         data=json.dumps(payload).encode() if payload is not None else None,
                                         headers={'content-type': 'application/json', 'accept-language': 'en-US'})
        start = time.monotonic()
        if mutation and route in ('/computer/pod/stop', '/computer/pod/restart') and self.pending_compute_request is not None:
            self.pending_compute_request.update(dispatched=True, accepted_result_unknown=True)
            self.event('compute_dispatched', self.pending_compute_request)
        try:
            with urllib.request.urlopen(request, timeout=self.remaining(300)) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read()
        except Exception as error:
            self.event('http_transport_failed', {'method': method, 'route': route, 'cause': str(error),
                                                'request': payload, 'accepted_result_unknown': mutation})
            raise
        try:
            body = json.loads(raw)
        except ValueError as error:
            self.event('http_invalid_json', {'method': method, 'route': route, 'http_status': status})
            raise ContractFailure('HTTP response is not the documented JSON envelope') from error
        self.event('http', {'method': method, 'route': route, 'http_status': status, 'elapsed_seconds': round(time.monotonic()-start, 3), 'body': body})
        require(200 <= status < 300 and body.get('code') == '0000' and body.get('success') is True,
                'HTTP product failure: ' + str(safe({'status': status, 'body': body})))
        return status, body

    def selected(self, kind):
        if kind == 'pvc':
            self.ownership()
            raw = self.kube(['get', 'pvc', builder_pvc_name(self.app), '--ignore-not-found=true', '-o', 'json'], json_output=False)
            rows = [json.loads(raw)] if raw else []
        else:
            selector = 'rcoder.io/identifier=' + self.app
            rows = self.kube(['get', kind, '-l', selector, '-o', 'json'])['items']
        for row in rows:
            validate_owned_resource(row, self.namespace, self.app, pvc=kind == 'pvc')
        return rows

    def physical(self, running):
        self.ownership()
        statefulsets, pods, pvcs = self.selected('sts'), self.selected('pods'), self.selected('pvc')
        require(len(statefulsets) == 1 and len(pvcs) == 1, 'expected exactly one owned STS and PVC')
        require(len(pods) == (1 if running else 0), 'owned Pod count differs from compute state')
        sts, pvc = statefulsets[0], pvcs[0]
        volume = next((item for item in sts.get('spec', {}).get('template', {}).get('spec', {}).get('volumes', [])
                       if item.get('name') == 'workspace'), {})
        require(volume.get('persistentVolumeClaim', {}).get('claimName') == builder_pvc_name(self.app), 'STS workspace claim is not the exact declared PVC')
        require(pvc.get('status', {}).get('phase') == 'Bound' and pvc.get('spec', {}).get('volumeName'), 'owned PVC binding has not been confirmed')
        require(sts['spec'].get('replicas') == (1 if running else 0), 'STS replicas differ from the confirmed compute state')
        snapshot = {'sts': resource_identity(sts), 'pvc': {**resource_identity(pvc), 'volume_name': pvc['spec']['volumeName'], 'phase': pvc['status']['phase']}, 'pod': None}
        if self.original is not None:
            require(snapshot['pvc']['uid'] == self.original['pvc']['uid']
                    and snapshot['pvc']['volume_name'] == self.original['pvc']['volume_name'], 'captured PVC or bound PV was replaced')
        if running:
            pod = pods[0]
            require(pod.get('status', {}).get('phase') == 'Running', 'owned Pod is not Running')
            statuses = pod.get('status', {}).get('containerStatuses', [])
            require(statuses and all(item.get('ready') for item in statuses), 'owned Pod containers are not Ready')
            require(any(item.get('uid') == sts['metadata']['uid'] and item.get('kind') == 'StatefulSet'
                        for item in pod['metadata'].get('ownerReferences', [])), 'Pod does not belong to the captured StatefulSet')
            volume = next((item for item in pod['spec'].get('volumes', []) if item.get('name') == 'workspace'), {})
            require(volume.get('persistentVolumeClaim', {}).get('claimName') == pvc['metadata']['name'], 'Pod workspace claim does not match the owned PVC')
            snapshot['pod'] = {**resource_identity(pod), 'image_status': [{key: item.get(key) for key in ['name', 'imageID', 'containerID', 'ready', 'restartCount']} for item in statuses]}
        return snapshot

    def wait_physical(self, running, cap=300):
        deadline = min(self.deadline, time.monotonic() + cap)
        last = None
        while time.monotonic() < deadline:
            try:
                return self.physical(running)
            except ContractFailure as error:
                last = str(error)
            time.sleep(1)
        raise ContractFailure('physical state deadline ended: ' + str(last))

    def workspace_exec(self, snapshot, write=False):
        self.ownership()
        current = self.physical(True)
        require(current['pod']['uid'] == snapshot['pod']['uid'] and current['pvc']['uid'] == snapshot['pvc']['uid'], 'captured Pod/PVC changed before workspace probe')
        program = r'''import hashlib,json,sys
from pathlib import Path
app,marker,mode=sys.argv[1:];root=Path('/home/user')/app
if not root.is_dir():raise RuntimeError('platform workspace root absent')
sentinel=root/'.local-k8s-core-sentinel';manifest=root/'workspace.manifest.toml'
if mode=='prepare':
 if sentinel.exists():raise RuntimeError('workspace sentinel already exists; app was not fresh')
 if manifest.exists():
  backup=root/'workspace.manifest.core-before';backup.write_bytes(manifest.read_bytes())
 sentinel.write_text(marker);manifest.write_text('[workspace\ninvalid=true\n')
print(json.dumps({'sentinel_matches':sentinel.read_text()==marker,'manifest_sha256':hashlib.sha256(manifest.read_bytes()).hexdigest(),'manifest_invalid_fixture':manifest.read_text()=='[workspace\ninvalid=true\n','workspace_root':str(root)}))
'''
        return json.loads(self.kube(['exec', snapshot['pod']['name'], '-c', 'agent', '--', 'python3', '-c', program,
                                    self.app, 'owned-' + self.identity['run_id'], 'prepare' if write else 'observe'], json_output=False))

    def readiness(self):
        _, body = self.http('GET', '/api/v1/userapp/' + self.app + '/dev/readiness?user_id=' + self.app)
        return body['data']

    def control(self, action, request_id):
        body = {'app_id': self.app, 'app_stage': 'dev', 'service_type': 'userapp',
                'lifecycle_id': self.lifecycle_id, 'request_id': request_id}
        self.pending_compute_request = {**body, 'action': action, 'dispatched': False,
                                        'accepted_result_unknown': False, 'receipt_verified': False}
        status, envelope = self.http('POST', '/computer/pod/' + action, body, mutation=True)
        require(status == 202, 'K8s compute control must return a real HTTP 202 receipt')
        operation = envelope['data']
        operation_id = operation.get('operation_id', '')
        require(re.fullmatch(r'[A-Za-z0-9_.:-]+', operation_id), 'accepted operation ID absent or invalid')
        require(envelope.get('operation_id') == operation_id, 'top-level operation identity does not match admission')
        validate_operation(operation, self.app, self.lifecycle_id, operation_id, action)
        require(operation.get('status_url') == '/computer/pod/operations/' + self.app + '/' + operation_id, 'operation status URL is not its exact original query')
        self.pending_compute_request.update(accepted_result_unknown=False, receipt_verified=True, operation_id=operation_id)
        self.last_operation = {'id': operation_id, 'action': action, 'status_url': operation['status_url'], 'request_id': request_id}
        return self.last_operation.copy()

    def wait_operation(self, captured, cap=300):
        deadline = min(self.deadline, time.monotonic() + cap)
        last = None
        while time.monotonic() < deadline:
            readiness = self.readiness()
            _, envelope = self.http('GET', captured['status_url'])
            record = envelope['data']
            complete = validate_operation(record, self.app, self.lifecycle_id, captured['id'], captured['action'])
            container = readiness.get('container', {})
            observed = container.get('operation', {})
            require(observed.get('operation_id') == captured['id'] and observed.get('action') == captured['action'], 'readiness replaced or lost the original operation')
            last = {'operation': record, 'container': container, 'ready': readiness.get('ready')}
            if observed.get('state') in ('pending', 'running'):
                require(readiness.get('ready') is False and container.get('status') == ('stopping' if captured['action'] == 'stop' else 'restarting'), 'readiness hides accepted compute progress')
            if complete and observed.get('state') == 'succeeded':
                require(container.get('status') == ('stopped' if captured['action'] == 'stop' else 'running'), 'terminal readiness disagrees with original compute receipt')
                return last
            time.sleep(0.5)
        raise ContractFailure('original compute operation deadline ended: ' + str(safe(last)))

    def input_proof(self):
        repo = Path(__file__).resolve().parents[2]
        build_path = self.root / 'host-build-after-identity.json'
        require(build_path.is_file(), 'freeze current host build identity before real verification')
        build = json.loads(build_path.read_text())
        expected_source = build.get('source_inputs_sha256', '')
        require(isinstance(expected_source, str) and re.fullmatch(r'[0-9a-f]{64}', expected_source),
                'host build receipt must bind the complete source_inputs_sha256; missing identity is not a pass')
        source = source_snapshot(repo)
        require(source['source_inputs_sha256'] == expected_source, 'source differs from the captured host build')
        binary = Path(build.get('frozen_binary', '')).resolve()
        require(binary.is_file() and binary.is_relative_to(self.root) and build.get('cargo_exit') == 0, 'host binary is not an owned successful build')
        require(digest(binary) == build.get('sha256'), 'host binary differs from frozen build receipt')
        process = json.loads((self.root / 'host-primary-process.json').read_text())
        require(process.get('namespace') == self.namespace and process.get('url', '').rstrip('/') == self.url
                and Path(process.get('executable', '')).resolve() == binary, 'host process identity/config target differs')
        pid = process.get('pid')
        require(isinstance(pid, int) and pid > 0, 'captured host PID absent')
        result = subprocess.run(['ps', '-p', str(pid), '-o', 'lstart=', '-o', 'comm='], capture_output=True, text=True, timeout=self.remaining(10))
        require(result.returncode == 0 and str(binary) in result.stdout, 'captured host process no longer runs the frozen executable')
        return {'source': {key: source[key] for key in ['commit', 'source_inputs_sha256', 'diff_sha256']},
                'harness_sha256': digest(__file__), 'binary': str(binary), 'binary_sha256': digest(binary),
                'source_bound_to_build': True,
                'host_pid': pid, 'host_process_start_and_executable': result.stdout.strip()}

    def run(self):
        context = self.kube(['config', 'current-context'], json_output=False)
        require(context == 'orbstack', 'private kubeconfig context is not orbstack')
        server = self.kube(['config', 'view', '--minify', '-o', 'jsonpath={.clusters[0].cluster.server}'], json_output=False)
        require(server == self.identity['cluster_api'], 'private kubeconfig cluster API differs from captured local identity')
        self.check('owned namespace identity confirmed', True, self.ownership())
        before = self.input_proof()
        self.report['inputs_before'] = before
        self.check('declared app has no prior workload or PVC', not any(self.selected(kind) for kind in ['sts', 'pods', 'pvc']), {'app_id': self.app})
        barrier = threading.Barrier(2)
        def ensure():
            barrier.wait(timeout=10)
            return self.http('POST', '/api/v1/userapp/workspace', {'app_id': self.app}, mutation=True)[1]['data']
        with ThreadPoolExecutor(max_workers=2) as workers:
            futures = [workers.submit(ensure) for _ in range(2)]
            replies = [future.result(timeout=self.remaining(310)) for future in futures]
        names = [reply.get('container_name') for reply in replies]
        self.check('two real ensure callers reuse the same application container', all(reply.get('app_id') == self.app for reply in replies) and names[0] and names[0] == names[1], replies)
        self.original = self.wait_physical(True)
        self.check('concurrent ensure yields exactly one STS Pod and PVC', True, self.original)
        _, lifecycle = self.http('GET', '/api/v1/userapp/' + self.app + '/lifecycle')
        self.lifecycle_id = lifecycle['data'].get('lifecycle_id')
        require(lifecycle['data'].get('app_id') == self.app and lifecycle['data'].get('state') == 'Active' and self.lifecycle_id, 'authoritative active lifecycle absent')
        marked = self.workspace_exec(self.original, write=True)
        self.check('owned workspace sentinel and invalid manifest retained', marked['sentinel_matches'] and marked['manifest_invalid_fixture'], marked)
        stopped = self.control('stop', 'core-first-stop-' + self.app)
        replayed = self.control('stop', stopped['request_id'])
        self.check('same Stop request replays the original operation', replayed == stopped, {'accepted': stopped, 'replay': replayed})
        terminal = self.wait_operation(stopped)
        physical = self.wait_physical(False)
        self.check('invalid manifest does not block real compute Stop', terminal['operation']['state'] == 'succeeded' and physical['pvc']['uid'] == self.original['pvc']['uid'], {'original_operation': terminal, 'physical': physical})
        for index in range(3):
            observed = self.readiness()
            still = self.physical(False)
            self.check('readiness observation preserves stopped compute ' + str(index+1), observed.get('ready') is False and still['pvc']['uid'] == physical['pvc']['uid']
                       and still['sts']['uid'] == physical['sts']['uid'] and observed.get('container', {}).get('operation', {}).get('operation_id') == stopped['id'], {'readiness': observed, 'physical': still})
            time.sleep(0.5)
        restarted = self.control('restart', 'core-restart-' + self.app)
        restart_terminal = self.wait_operation(restarted)
        resumed = self.wait_physical(True)
        proof = self.workspace_exec(resumed)
        self.check('Restart changes Pod UID and keeps original PVC and data', resumed['pod']['uid'] != self.original['pod']['uid']
                   and resumed['pvc']['uid'] == self.original['pvc']['uid'] and resumed['pvc']['volume_name'] == self.original['pvc']['volume_name']
                   and proof['sentinel_matches'] and proof['manifest_sha256'] == marked['manifest_sha256'], {'original': self.original, 'resumed': resumed, 'workspace': proof, 'operation': restart_terminal})
        final = self.control('stop', 'core-final-stop-' + self.app)
        final_terminal = self.wait_operation(final)
        final_physical = self.wait_physical(False)
        self.check('successful final Stop preserves the original PVC', final_terminal['operation']['state'] == 'succeeded' and final_physical['pvc']['uid'] == self.original['pvc']['uid'], {'operation': final_terminal, 'physical': final_physical})
        after = self.input_proof()
        self.report['inputs_after'] = after
        self.check('source host binary and process remain frozen', after == before, {'before': before, 'after': after})
        self.report['success'] = bool(self.report['checks']) and all(item['passed'] for item in self.report['checks'])
        self.report['volume_retained'] = True
        self.report['final_compute_stop'] = final
        self.persist()

    def failure_evidence(self, error):
        self.report['error'] = safe(str(error))
        self.report['volume_retained'] = 'not deleted by harness; physical state may be unknown'
        self.report['last_known_operation'] = self.last_operation
        self.report['pending_compute_request'] = self.pending_compute_request
        # Read-only diagnostics never try recovery, another Stop, or deletion.
        captures = {}
        for kind in ['sts', 'pods', 'pvc']:
            try:
                captures[kind] = [resource_identity(item) for item in self.selected(kind)]
            except Exception as failure:
                captures[kind] = {'capture_error': safe(str(failure))}
        self.report['failure_resources'] = captures
        if self.last_operation is not None and time.monotonic() < self.deadline:
            try:
                _, envelope = self.http('GET', self.last_operation['status_url'])
                self.report['failure_original_operation'] = envelope['data']
            except Exception as failure:
                self.report['failure_original_operation'] = {'capture_error': safe(str(failure))}
        self.persist()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, type=Path)
    parser.add_argument('--url', required=True)
    parser.add_argument('--app', required=True)
    parser.add_argument('--budget-seconds', type=int, default=1200)
    args = parser.parse_args()
    require(600 <= args.budget_seconds <= 3600, 'explicit parent budget must cover bounded cold startup/control stages')
    try:
        harness = Harness(args.root, args.url, args.app, args.budget_seconds)
    except Exception as error:
        print(json.dumps({'success': False, 'phase': 'admission', 'error': safe(str(error))}, ensure_ascii=False), flush=True)
        return 1
    try:
        harness.run()
    except Exception as error:
        harness.failure_evidence(error)
        print(json.dumps({'success': False, 'error': safe(str(error)), 'report': str(harness.report_path)}, ensure_ascii=False), flush=True)
        return 1
    print(json.dumps({'success': harness.report['success'], 'checks': len(harness.report['checks']), 'report': str(harness.report_path)}, ensure_ascii=False), flush=True)
    return 0 if harness.report['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
