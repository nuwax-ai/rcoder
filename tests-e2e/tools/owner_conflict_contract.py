"""Opt-in real Computer owner-conflict acceptance; no model service is needed.

Only HTTP ensure/chat/stop mutate resources. Docker/kubectl are read-only
observers. This deliberately does not register a new broad test framework.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

SERVICE = 'computer-agent-runner'
CODE = 'ERR_COMPUTER_OWNER_CONFLICT'
REASON = 'COMPUTER_OWNER_USER_MISMATCH'
STAGE = 'computer.registration.before_dispatch'


def identities():
    run = uuid.uuid4().hex
    return {'run_id': run, 'user_a': 'oc' + run[:14] + 'a',
            'user_b': 'oc' + run[:14] + 'b', 'project': 'oc' + run[14:],
            'cleanup_project_b': 'oc' + run[14:] + 'b'}


def payload(user, project):
    return {'user_id': user, 'project_id': project, 'service_type': SERVICE}


def chat_payload(user, project):
    # A regression must not reach a real provider using inherited credentials.
    # This is an unreachable local endpoint, not a fake model response.
    return {**payload(user, project), 'prompt': 'Owner contract: reject before dispatch.',
            'request_id': uuid.uuid4().hex, 'session_id': None,
            'model_provider': {'id': 'owner-contract-no-model', 'name': 'openai',
                'base_url': 'http://127.0.0.1:1/v1', 'api_key': 'unused-contract-key',
                'requires_openai_auth': True, 'default_model': 'no-model',
                'api_protocol': 'openai', 'wire_api': 'chat'}}


def save(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with path.open('w') as stream:
        os.chmod(path, 0o600)
        json.dump(data, stream, indent=2, ensure_ascii=False)
        stream.write('\n')


def command(argv):
    result = subprocess.run(argv, capture_output=True, text=True, timeout=90)
    if result.returncode:
        raise RuntimeError('Read-only observer failed: ' + argv[0]
                           + ' (stderr SHA256 ' + hashlib.sha256(result.stderr.encode()).hexdigest() + ')')
    return result.stdout


def success(reply):
    if reply['status'] != 200 or reply['body'].get('code') != '0000' or reply['body'].get('success') is not True:
        raise AssertionError('Expected HTTP200 successful platform envelope')
    return reply['body'].get('data')


def conflict(reply, user_a, user_b):
    body = reply['body']
    detail = body.get('error_detail') or {}
    assert reply['status'] == 409 and body.get('code') == CODE, 'Not a typed HTTP409 owner conflict'
    assert body.get('success') is False and body.get('data') is None, 'Conflict must not accept a task'
    assert detail.get('reason_code') == REASON and detail.get('stage') == STAGE
    assert detail.get('retryable') is False, 'Owner conflicts must never authorize automatic retry'
    assert isinstance(body.get('message'), str) and '账号' in body['message'] and '消息未发送' in body['message']
    assert detail.get('detail') == body['message'] and isinstance(detail.get('hint'), str) and detail['hint']
    assert body.get('tid') == reply['trace_id'], 'Actual trace ID must survive the HTTP envelope'
    assert not body.get('operation_id')
    encoded = json.dumps(body, ensure_ascii=False)
    assert user_a not in encoded and user_b not in encoded, 'UI disclosed an owner identity'


class Http:
    def __init__(self, url, root, timeout):
        parsed = urllib.parse.urlsplit(url)
        if parsed.scheme not in ('http', 'https') or not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError('An explicit HTTP(S) RCoder base URL without credentials is required')
        self.base = url.rstrip('/')
        self.root, self.deadline, self.calls = Path(root), time.monotonic() + timeout, []

    def call(self, method, route, body=None, cleanup=False):
        remaining = 120 if cleanup else min(120, self.deadline - time.monotonic())
        if remaining <= 0:
            raise TimeoutError('Owner contract total request budget expired')
        trace = uuid.uuid4().hex
        headers = {'Content-Type': 'application/json', 'Accept-Language': 'zh-CN',
                   'traceparent': '00-' + trace + '-' + uuid.uuid4().hex[:16] + '-01'}
        if os.environ.get('RCODER_API_KEY'):
            headers['X-API-Key'] = os.environ['RCODER_API_KEY']
        request = urllib.request.Request(self.base + route, method=method, headers=headers,
                                        data=None if body is None else json.dumps(body).encode())
        try:
            response = urllib.request.urlopen(request, timeout=remaining)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            raw = response.read(1024 * 1024 + 1)
            if len(raw) > 1024 * 1024:
                raise ValueError('Unexpectedly large HTTP envelope')
            envelope = json.loads(raw)
            if not isinstance(envelope, dict):
                raise ValueError('Expected JSON object envelope')
            reply = {'status': response.status, 'trace_id': trace, 'body': envelope}
        name = '%02d-%s.json' % (len(self.calls), route.split('?')[0].strip('/').replace('/', '-'))
        # Actual response bytes/values remain local0600, for Java consumption.
        save(self.root / 'responses' / name, reply)
        self.calls.append({'method': method, 'route': route, 'trace_id': trace,
                           'response': str(self.root / 'responses' / name), 'status': reply['status']})
        return reply

    def status(self, user, cleanup=False):
        route = '/computer/pod/status?' + urllib.parse.urlencode({'user_id': user, 'service_type': SERVICE})
        return success(self.call('GET', route, cleanup=cleanup))


# The observer emits only the exact test traces and safe owner/forward facts.
LOG_READER = r'''import json,pathlib
traces=set(TRACE_IDS);rows=[]
for f in sorted(pathlib.Path('/app/logs').glob('rcoder.20*'))[-2:]:
 if not f.is_file():continue
 with f.open(errors='replace') as stream:
  for n,line in enumerate(stream,1):
   if not any(t in line for t in traces):continue
   try:o=json.loads(line)
   except ValueError:continue
   spans=o.get('spans',[])+([o['span']] if isinstance(o.get('span'),dict) else [])
   trace=next((s.get('trace_id') for s in spans if s.get('trace_id') in traces),None)
   if trace is None:continue
   fields=o.get('fields',{});message=fields.get('message','');target=o.get('target','')
   owner=message=='Computer registration owner conflict before dispatch'
   forward=(target.endswith('computer_chat_handler::forward') and 'Forwarding request' in message) or (target.endswith('grpc::chat_client') and 'Sending Chat request' in message)
   if owner or forward:
    rows.append({'file':str(f),'line':n,'timestamp':o.get('timestamp'),'trace_id':trace,'target':target,'owner_conflict':owner,'chat_forward':forward,'fields':{k:v for k,v in fields.items() if k in ['dimension','reason_code','requested_user_id','registered_user_id','dispatched','project_id']}})
print(json.dumps(rows))
'''


def option_string(value):
    if isinstance(value, str) and value.startswith('Some(') and value.endswith(')'):
        return json.loads(value[5:-1])
    return value


def log_proof(rows, traces, ids):
    for trace in traces:
        scoped = [r for r in rows if r['trace_id'] == trace]
        assert not any(r['chat_forward'] for r in scoped), 'Rejected request reached Chat forward'
        owners = [r['fields'] for r in scoped if r['owner_conflict']]
        assert owners, 'Missing positive structured owner evidence for rejected request'
        assert all(f.get('dimension') == 'user' and f.get('reason_code') == REASON
                   and f.get('dispatched') is False and f.get('project_id') == ids['project']
                   and option_string(f.get('registered_user_id')) == ids['user_a']
                   and option_string(f.get('requested_user_id')) == ids['user_b'] for f in owners)


class Observer:
    def __init__(self, args):
        self.args = args
        self.k8s = bool(args.namespace)
        if self.k8s:
            if not (args.namespace == 'nuwax-k8s-test' or re.fullmatch(r'rcoder-e2e-[a-z0-9-]{1,36}', args.namespace)):
                raise ValueError('Only an explicit test namespace is allowed')
            if not args.ssh or args.ssh.startswith('-'):
                raise ValueError('K8s read-only observer requires explicit --ssh')
        elif not re.fullmatch(r'[a-f0-9]{12,64}', args.controller_id or ''):
            raise ValueError('Compose observer requires captured --controller-id')
        self.controllers = self.controller_ids()
        if not self.controllers:
            raise ValueError('No main controller available for positive log proof')

    def kube(self, *args):
        kubectl = ['kubectl'] + (['--context', self.args.context] if self.args.context else [])
        if self.args.server:
            kubectl += ['--server', self.args.server]
        return command(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', self.args.ssh,
                        shlex.join(kubectl + ['-n', self.args.namespace, *args])])

    def controller_ids(self):
        if not self.k8s:
            c = json.loads(command(['docker', 'inspect', self.args.controller_id]))[0]
            assert c['Id'].startswith(self.args.controller_id) and c['State']['Running']
            return {c['Id']: c['Name']}
        selector = ('app.kubernetes.io/component=rcoder-main,'
                    'app.kubernetes.io/instance=' + self.args.namespace)
        pods = json.loads(self.kube('get', 'pods', '-l', selector, '-o', 'json'))['items']
        result = {}
        for pod in pods:
            meta = pod['metadata']; labels = meta.get('labels', {})
            assert meta.get('namespace') == self.args.namespace
            assert labels.get('app.kubernetes.io/component') == 'rcoder-main'
            assert labels.get('app.kubernetes.io/instance') == self.args.namespace
            if not meta.get('deletionTimestamp'):
                result[meta['uid']] = meta['name']
        return result

    def claims(self):
        if not self.k8s:
            return {}
        return {p['metadata']['name']: {'uid': p['metadata']['uid'], 'pv': p['spec'].get('volumeName')}
                for p in json.loads(self.kube('get', 'pvc', '-o', 'json'))['items']}

    def existing(self):
        if self.k8s:
            return {}  # PVC identity is guarded separately; only owned compute is stopped.
        ids = command(['docker', 'ps', '-aq', '--no-trunc']).split()
        if not ids:
            return {}
        return {c['Id']: {'image': c['Image'], 'running': c['State']['Running']}
                for c in json.loads(command(['docker', 'inspect', *ids]))}

    def capture(self, user):
        if not self.k8s:
            ids = command(['docker', 'ps', '-aq', '--no-trunc', '--filter', 'label=identifier=' + user]).split()
            result = []
            for c in json.loads(command(['docker', 'inspect', *ids])) if ids else []:
                labels = c['Config'].get('Labels') or {}
                assert labels.get('identifier') == user and labels.get('service-type') == SERVICE
                result.append({'kind': 'Container', 'name': c['Name'], 'uid': c['Id'],
                               'mounts': [{k: m.get(k) for k in ['Type', 'Name', 'Source', 'Destination']} for m in c['Mounts']]})
            return result
        rows = json.loads(self.kube('get', 'pods,statefulsets,services', '-l', 'rcoder.io/identifier=' + user, '-o', 'json'))['items']
        result = []
        for row in rows:
            m = row['metadata']; labels = m.get('labels', {})
            assert labels.get('rcoder.io/identifier') == user
            assert labels.get('rcoder.io/service-type') == SERVICE and labels.get('app.kubernetes.io/managed-by') == 'rcoder-runtime'
            result.append({'kind': row['kind'], 'name': m['name'], 'uid': m['uid']})
        return result

    def logs(self, traces):
        assert self.controller_ids() == self.controllers, 'Main controller incarnation changed during acceptance'
        code = LOG_READER.replace('TRACE_IDS', repr(traces))
        rows = []
        for uid, name in self.controllers.items():
            raw = self.kube('exec', name, '-c', 'rcoder', '--', 'python3', '-c', code) if self.k8s else command(['docker', 'exec', uid, 'python3', '-c', code])
            rows.extend({**r, 'main_uid': uid} for r in json.loads(raw))
        return rows

    def retained(self, resources):
        for row in resources:
            for mount in row.get('mounts', []):
                if mount['Type'] == 'volume':
                    command(['docker', 'volume', 'inspect', mount['Name']])
                elif mount['Type'] == 'bind' and mount['Source'] != '/var/run/docker.sock':
                    assert Path(mount['Source']).exists(), 'An original bind source disappeared'


def physical(resources):
    return [r['uid'] for r in resources if r['kind'] in ('Container', 'Pod')]


def cleanup(api, observer, ids, captured, claims_before):
    result = {'success': True, 'users': [], 'volumes_removed': False}
    # Include claims created by an ensure whose response may have been lost.
    # An existing UID/PV mismatch must not be hidden by dict.update, and a
    # preservation failure must not skip safe cleanup of our captured compute.
    retained_claims = dict(claims_before)
    try:
        current_claims = observer.claims()
        assert all(current_claims.get(k) == v for k, v in claims_before.items()), 'Agent PVC identity/PV changed before cleanup'
        retained_claims.update(current_claims)
    except Exception as error:
        result['success'] = False
        result['claim_observation_error'] = str(error)
    retained = list(captured.values())
    for key, project in [('user_b', ids['cleanup_project_b']), ('user_a', ids['project'])]:
        user = ids[key]
        try:
            actual = observer.capture(user)
            old = captured.get(user)
            if old is not None and actual and actual != old:
                raise AssertionError('Owned compute changed before cleanup; refusing logical-name stop')
            if old is None:
                # An ensure may create compute before returning an error/timeout.
                # The initial absence + exact random-user labels are its provenance.
                captured[user] = actual
                retained.append(actual)
            state = api.status(user, cleanup=True)
            if state.get('container_id') and state['container_id'] not in physical(actual):
                raise AssertionError('HTTP and captured runtime identity disagree; refusing stop')
            response = api.call('POST', '/computer/pod/stop', payload(user, project), cleanup=True)
            success(response)
            deadline = time.monotonic() + 30
            while observer.capture(user):
                if time.monotonic() >= deadline:
                    raise TimeoutError('Owned compute still exists after platform stop')
                time.sleep(.5)
            after = api.status(user, cleanup=True)
            assert after.get('status') == 'not_found' and not after.get('container_id')
            result['users'].append({'user': user, 'project': project, 'captured': actual, 'stopped': True})
        except Exception as error:
            result['success'] = False
            result['users'].append({'user': user, 'project': project, 'stopped': False, 'error': str(error)})
    try:
        after = observer.claims()
        assert all(after.get(name) == old for name, old in retained_claims.items()), 'Agent PVC identity/PV changed'
        for rows in retained:
            observer.retained(rows)
        result['retained_claims'] = after
    except Exception as error:
        result['success'] = False
        result['data_preservation_error'] = str(error)
    return result


def exercise(api, observer, ids, root):
    report = {'version': 1, 'run_id': ids['run_id'], 'identities': ids, 'success': False,
              'llm_credentials_supplied': False, 'model_endpoint': 'local closed port only',
              'all_rpc_zero_claimed': False, 'checks': [], 'responses': [], 'cleanup': None}
    captured = {}
    baseline = observer.existing()
    claims = observer.claims()
    touched = False
    try:
        for key in ('user_a', 'user_b'):
            assert not observer.capture(ids[key]), 'Random test identity already owns compute'
            assert success(api.call('GET', '/computer/pod/status?' + urllib.parse.urlencode({'user_id': ids[key], 'service_type': SERVICE}))).get('status') == 'not_found'
        touched = True
        a = success(api.call('POST', '/computer/pod/ensure', payload(ids['user_a'], ids['project'])))
        a_id = a['container_info']['container_id']
        captured[ids['user_a']] = observer.capture(ids['user_a'])
        save(Path(root) / 'captured-compute.json', captured)
        assert physical(captured[ids['user_a']]) == [a_id], 'A ensure does not match captured runtime instance'
        assert api.status(ids['user_a']).get('container_id') == a_id
        report['checks'].append('A original ensure registered real captured compute')
        traces = []
        for route, request in [('/computer/chat', chat_payload(ids['user_b'], ids['project'])),
                               ('/computer/pod/ensure', payload(ids['user_b'], ids['project']))]:
            reply = api.call('POST', route, request)
            traces.append(reply['trace_id'])
            conflict(reply, ids['user_a'], ids['user_b'])
            resources = observer.capture(ids['user_b'])
            if resources:
                previous = captured.setdefault(ids['user_b'], resources)
                assert previous == resources, 'B compute identity changed across rejected requests'
                save(Path(root) / 'captured-compute.json', captured)
            assert observer.capture(ids['user_a']) == captured[ids['user_a']]
            assert api.status(ids['user_a']).get('container_id') == a_id
            report['checks'].append(route + ' typed user conflict; A physical identity unchanged')
        # This successful original API revalidation complements actual registered
        # A/B values in the structured conflict events; it is not a fake marker.
        a_again = success(api.call('POST', '/computer/pod/ensure', payload(ids['user_a'], ids['project'])))
        assert a_again['container_info']['container_id'] == a_id
        assert observer.capture(ids['user_a']) == captured[ids['user_a']]
        report['checks'].append('A same project ensure still succeeds with original physical identity')
        deadline = time.monotonic() + 10
        while True:
            rows = observer.logs(traces)
            try:
                log_proof(rows, traces, ids)
                break
            except AssertionError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(.5)
        save(Path(root) / 'owner-trace-evidence.json', rows)
        report['checks'].append('Both trace-scoped real owner=A/requester=B events; no Chat forward')
        report['success'] = True
    except Exception as error:
        report['error'] = type(error).__name__ + ': ' + str(error)
    finally:
        try:
            report['cleanup'] = cleanup(api, observer, ids, captured, claims) if touched else {'success': True, 'not_started': True}
            assert report['cleanup']['success'], 'Owned platform cleanup incomplete'
            after = observer.existing()
            assert all(after.get(uid) == value for uid, value in baseline.items()), 'Preexisting container changed'
            report['preexisting_resources_preserved'] = True
        except Exception as error:
            report['success'] = False
            report['cleanup_error'] = type(error).__name__ + ': ' + str(error)
        report['responses'] = api.calls
        report['captured_compute'] = captured
        report['completed_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        save(Path(root) / 'report.json', report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True, help='Explicit RCoder test HTTP base URL')
    parser.add_argument('--report-dir', required=True)
    parser.add_argument('--controller-id', help='Captured isolated Compose main container ID (log observer only)')
    parser.add_argument('--namespace', help='Explicit test namespace; switches observer to K8s')
    parser.add_argument('--ssh', help='SSH transport for read-only kubectl')
    parser.add_argument('--context')
    parser.add_argument('--server', help='Optional explicit Kubernetes API server, retaining TLS verification')
    parser.add_argument('--timeout', type=float, default=360)
    parser.add_argument('--execute', action='store_true', help='Execute real owned test; otherwise print plan only')
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error('--timeout must be positive')
    ids = identities()
    if not args.execute:
        print(json.dumps({'execute': False, 'identities': ids, 'steps': ['A ensure', 'B chat typed409',
            'B ensure typed409', 'A same-project ensure and exact physical identity',
            'trace owner/no-Chat proof', 'platform stop B with independent project; platform stop A; retain data']}))
        return 0
    os.umask(0o077)
    root = Path(args.report_dir).resolve()
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    api = Http(args.url, root, args.timeout)
    observer = Observer(args)
    save(root / 'identities.json', ids)
    save(root / 'cleanup-plan.json', [payload(ids['user_b'], ids['cleanup_project_b']),
                                      payload(ids['user_a'], ids['project'])])
    print(json.dumps({'status': 'OWNED_FIXTURE_START', 'identities': ids,
                      'scope': 'only these new random users; no customer resource reuse'}), flush=True)
    report = exercise(api, observer, ids, root)
    print(json.dumps({'report': str(root / 'report.json'), 'success': report['success']}))
    return 0 if report['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
