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
REQUIRED = {'cluster_ready', 'concurrent_ensure', 'ensure_retry', 'builder_identity',
            'concurrent_error_contract', 'cross_replica_files', 'build_A', 'build_B', 'artifact_A', 'artifact_B',
            'sse_terminal', 'sse_past_terminal', 'cross_replica_cancel', 'cold_deploy',
            'content_A', 'hot_env_rejected', 'hot_failure', 'old_content_healthy',
            'hot_deploy', 'content_B', 'hot_pod_preserved', 'deploy_identity',
            'stop_idempotent', 'stop_zero_pods', 'wake_new_pod', 'wake', 'wake_content_B', 'cleanup', 'baseline_preserved'}


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
        self.app = 'e2e-k8s-' + self.id[:16]
        self.user = 'e2e-' + self.id[:12]
        self.root = ROOT / 'reports' / self.id
        self.root.mkdir(parents=True)
        self.report_lock = threading.RLock()
        self.assertions = []
        self.requests = []
        self.tunnel = None
        self.baseline = []
        self.created = False
        self.base = args.url.rstrip('/')
        self.entries = []
        self.trace = uuid.uuid4().hex
        self.summary = {'run_id': self.id, 'app_id': self.app, 'namespace': args.namespace,
                        'ssh': args.ssh, 'planned': sorted(REQUIRED), 'verdict': 'aborted',
                        'head': self.local('git', 'rev-parse', 'HEAD').strip(),
                        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
        from run import source_fingerprint
        self.summary['worktree_sha256'] = source_fingerprint()
        self.save('summary.json', self.summary)
        print('Report: ' + str(self.root), flush=True)

    def local(self, *args):
        return subprocess.check_output(args, cwd=ROOT.parent, text=True, timeout=90)

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

    def content(self, expected):
        path = f'/api/v1/userapp/proxy/app/prod/{self.user}/{self.app}/'
        def read():
            return self.request(path, base=self.args.proxy_url.rstrip('/'), raw=True, timeout=15)
        result = self.poll(read, lambda r: r[0] == 200 and r[1].decode() == expected, 180)
        return result[0] == 200

    def deploy(self, artifact_a, release_a, artifact_b, release_b):
        path = '/api/v1/userapp/' + self.app + '/start'
        before = time.monotonic()
        cold = self.api(path, {'user_id': self.user, **artifact_a}, timeout=360)
        self.check('cold_deploy', time.monotonic() - before < 180, cold)
        self.check('content_A', self.content(self.id + '-A'))
        pod = self.prod_pod()
        self.save('production-A.json', pod)
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
        for _ in range(2):
            self.api('/api/v1/userapp/' + self.app + '/stop?user_id=' + self.user, {})
        self.check('stop_idempotent', True)
        stopped = self.poll(lambda: [r for r in self.inventory() if r['kind'] == 'Pod' and r['name'].startswith('rcoder-app-' + self.app + '-')], lambda rows: not rows, 120)
        self.check('stop_zero_pods', not stopped)
        self.api(path, {'user_id': self.user}, timeout=240)
        self.check('wake', True)
        self.check('wake_content_B', self.content(self.id + '-B'))
        awakened = self.prod_pod()
        self.save('production-wake.json', awakened)
        self.check('wake_new_pod', awakened['uid'] != after['uid'], awakened['uid'])

    def cleanup(self):
        if self.created:
            rows = self.inventory()
            owned = [r for r in rows if self.owned(r)]
            self.save('before-cleanup.json', owned)
            baseline_uids = {r['uid'] for r in self.baseline}
            if any(r['uid'] in baseline_uids for r in owned):
                raise AssertionError('Refusing cleanup of a preexisting identity')
            if any(r['kind'] == 'Deployment' and r['name'] == 'rcoder-app-' + self.app for r in owned):
                self.api('/api/v1/userapp/' + self.app + '/prod/delete', {'user_id': self.user, 'purge': True}, timeout=180)
            self.api('/api/v1/userapp/' + self.app + '/delete/app', {'user_id': self.user}, timeout=180)
            remaining = self.poll(lambda: [r for r in self.inventory() if self.owned(r)], lambda r: not r, 180)
            self.check('cleanup', not remaining, remaining)
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
