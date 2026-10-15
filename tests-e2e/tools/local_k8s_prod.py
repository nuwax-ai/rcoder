#!/usr/bin/env python3
"""Owned local K8s Prod acceptance preparation: real build/GET/SSE/artifact bytes.

The build stage does not claim Prod, Lease or R1 acceptance. Resource manifests
are proposals only; applying them and running Prod cases require Root dispatch.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import threading
import time
import urllib.parse
import uuid
import zipfile

from local_k8s_core import Harness as CoreHarness, ContractFailure, digest, require, safe, validate_inputs


BUSY_FIELDS = {'holder_operation_id', 'holder_kind', 'holder_traffic_wake', 'holder_state',
               'holder_step', 'retryable', 'retry_after_seconds'}


def validate_busy(body, holder_id, kind='start_deployment', traffic=False, retry_seconds=45):
    require(body.get('code') == 'ERR_OPERATION_IN_PROGRESS' and body.get('success') is False,
            'a real typed occupied request must use ERR_OPERATION_IN_PROGRESS')
    data = body.get('data')
    require(isinstance(data, dict) and BUSY_FIELDS <= data.keys(), 'typed busy data is incomplete')
    require(data['holder_operation_id'] == holder_id and data['holder_kind'] == kind
            and data['holder_traffic_wake'] is traffic and data['holder_state'] == 'running'
            and isinstance(data['holder_step'], str) and data['holder_step']
            and data['retryable'] is True and data['retry_after_seconds'] == retry_seconds,
            'holder payload must match the original durable live operation, not a lease annotation')
    require(not body.get('operation_id'), 'rejected request must not masquerade as an admitted caller')
    return data


def fixture_zip(marker):
    """A real Python HTTP service and process worker, with no AI/dependency download."""
    require(re.fullmatch(r'[a-z0-9-]+', marker), 'fixture marker must be literal owned data')
    archive = io.BytesIO()
    web = '''import os
from http.server import BaseHTTPRequestHandler,HTTPServer
MARKER=%r
class Handler(BaseHTTPRequestHandler):
 def do_GET(self):
  self.send_response(200);self.end_headers();self.wfile.write(MARKER.encode())
HTTPServer(("0.0.0.0",int(os.environ["PORT"])),Handler).serve_forever()
''' % marker
    package = '[build]\ncommand = ["python3", "-c", "from zipfile import ZipFile; z=ZipFile(\'artifact.zip\',\'w\'); z.write(\'main.py\'); z.close()"]\nartifact = "artifact.zip"\n'
    files = {
        'workspace.manifest.toml': 'schema_version = 1\n[workspace]\nname = "local-core-prod"\n',
        'web/project.manifest.toml': 'schema_version = 1\n[project]\nservice_id = "web"\nname = "HTTP fixture"\ntype = "python"\nkind = "web"\n' + package + '[run]\ncommand = ["python3", "main.py"]\n[proxy]\npath = "/"\nstrip_prefix = false\n',
        'web/main.py': web,
        'worker/project.manifest.toml': 'schema_version = 1\n[project]\nservice_id = "worker"\nname = "Worker fixture"\ntype = "python"\nkind = "worker"\n' + package + '[run]\ncommand = ["python3", "main.py"]\n[health]\nstartup_probe = "process"\n',
        'worker/main.py': 'import time\nwhile True: time.sleep(1)\n',
    }
    with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as zipped:
        for name, text in files.items():
            zipped.writestr(name, text)
    return archive.getvalue()


def multipart(app, content):
    boundary = 'local-prod-' + uuid.uuid4().hex
    fields = bytearray()
    for name, value in [('app_id', app), ('enable_git', 'false')]:
        fields.extend(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode())
    fields.extend(f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="source.zip"\r\nContent-Type: application/zip\r\n\r\n'.encode())
    fields.extend(content)
    fields.extend(f'\r\n--{boundary}--\r\n'.encode())
    return bytes(fields), 'multipart/form-data; boundary=' + boundary


def completed_sse(raw, expected):
    """Parse the actual ordered build SSE; logs containing 'completed' do not count."""
    events = []
    for block in raw.decode('utf-8').replace('\r\n', '\n').split('\n\n'):
        lines = block.splitlines()
        identifiers = [line[3:].strip() for line in lines if line.startswith('id:')]
        data = [line[5:].lstrip() for line in lines if line.startswith('data:')]
        if not data:
            continue
        require(len(identifiers) == 1 and identifiers[0].isdigit(), 'SSE record lacks a unique real sequence')
        events.append((int(identifiers[0]), json.loads('\n'.join(data))))
    sequences = [sequence for sequence, _ in events]
    require(sequences and sequences == sorted(set(sequences)), 'build SSE sequences repeat or regress')
    terminals = [event for _, event in events if event.get('event') in ('completed', 'failed', 'cancelled')]
    require(len(terminals) == 1 and terminals[0].get('event') == 'completed', 'build has no unique actual completed SSE terminal')
    require(terminals[0].get('release_id') == expected['release_id']
            and terminals[0].get('sha256') == expected['sha256'], 'SSE terminal disagrees with the same original task snapshot')
    return {'event_count': len(events), 'last_sequence': sequences[-1], 'terminal': terminals[0]}


def validate_import_resume(report, app, namespace, run_id):
    require(report.get('success') is False and report.get('stage') == 'build'
            and report.get('app_id') == app and report.get('namespace') == namespace
            and report.get('run_id') == run_id and report.get('builder') and report.get('lifecycle_id'),
            'resume must bind the exact failed original Stage1 app/lifecycle/resource receipt')
    require(not report.get('tasks') and not report.get('artifacts'), 'resume cannot replay an already-dispatched build or frozen artifact')
    replies = [row.get('evidence', {}) for row in report.get('timeline', []) if row.get('stage') == 'http_reply']
    require(not any(row.get('route') == '/api/v1/userapp/build' for row in replies), 'original build dispatch cannot be retried as an import fixture correction')
    imported = [row for row in replies if row.get('method') == 'POST' and row.get('route') == '/api/v1/userapp/init-project-template']
    require(len(imported) == 1 and imported[0].get('status') == 200 and imported[0].get('body') == {
        'success': True, 'message': 'Project template initialized successfully', 'workspace_root': '/home/user/' + app},
        'original report does not prove this exact successful flat import')
    return report['builder'], report['lifecycle_id']


ARTIFACT_SERVER = r'''import json,re,threading,time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from pathlib import Path
gates={};lock=threading.Lock()
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):
  parts=self.path.split('?',1)[0].strip('/').split('/')
  if parts==['health']:self.send_response(200);self.end_headers();self.wfile.write(b'ready');return
  if len(parts)==2 and parts[0] in ('release','gate') and re.fullmatch('[a-z0-9-]{1,63}',parts[1]):
   with lock:
    gate=gates.setdefault(parts[1],{'entered':False,'release':threading.Event()})
    if parts[0]=='release':gate['release'].set()
    value={'entered':gate['entered'],'released':gate['release'].is_set()}
   self.send_response(200);self.end_headers();self.wfile.write(json.dumps(value).encode());return
  if len(parts)==3 and parts[0]=='gated' and re.fullmatch('[a-z0-9-]{1,63}',parts[1]) and parts[2] in ('A.zip','B.zip'):
   with lock:
    gate=gates.setdefault(parts[1],{'entered':False,'release':threading.Event()});gate['entered']=True
   if not gate['release'].wait(240):self.send_error(504,'owned download gate expired');return
   filename=parts[2]
  elif len(parts)==2 and parts[0]=='artifacts' and parts[1] in ('A.zip','B.zip'):filename=parts[1]
  else:self.send_error(404);return
  raw=(Path('/supply')/filename).read_bytes()
  self.send_response(200);self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw)
ThreadingHTTPServer(('0.0.0.0',8019),Handler).serve_forever()
'''


def artifact_proposal(namespace, run_id, app, image, artifacts):
    """Generate owned immutable data/compute only, never apply or delete it."""
    require(re.fullmatch(r'rcoder-[a-z0-9-]+', namespace) and namespace != 'rcoder-dev', 'artifact namespace must be owned')
    require(re.fullmatch(r'[0-9a-f]{32}', run_id) and re.fullmatch(r'[a-z][a-z0-9]{0,21}', app), 'artifact identity invalid')
    require(re.fullmatch(r'[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}', image)
            and '://' not in image, 'artifact image must be an explicit immutable OCI reference')
    import base64
    name = 'core-artifacts-' + app
    labels = {'rcoder.e2e.owner': run_id, 'rcoder.e2e.app': app, 'rcoder.e2e.role': 'prod-artifacts'}
    def metadata():return {'name': name, 'namespace': namespace, 'labels': labels.copy()}
    binary = {}
    for version in ('A', 'B'):
        entry = artifacts[version];raw = Path(entry['path']).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == entry['sha256'], 'frozen artifact bytes changed')
        binary[version + '.zip'] = base64.b64encode(raw).decode()
    return [
        {'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': metadata(), 'immutable': True,
         'data': {'serve.py': ARTIFACT_SERVER}, 'binaryData': binary},
        {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': metadata(), 'spec': {
            'restartPolicy': 'Never', 'containers': [{'name': 'supply', 'image': image,
                'command': ['python3', '/supply/serve.py'], 'ports': [{'containerPort': 8019}],
                'resources': {'requests': {'cpu': '100m', 'memory': '64Mi'}, 'limits': {'cpu': '500m', 'memory': '256Mi'}},
                'readinessProbe': {'httpGet': {'path': '/health', 'port': 8019}},
                'volumeMounts': [{'name': 'supply', 'mountPath': '/supply', 'readOnly': True}]}],
            'volumes': [{'name': 'supply', 'configMap': {'name': name}}]}},
        {'apiVersion': 'v1', 'kind': 'Service', 'metadata': metadata(), 'spec': {'type': 'ClusterIP',
            'selector': labels.copy(), 'ports': [{'port': 8019, 'targetPort': 8019}]}}
    ]


class ProdHarness(CoreHarness):
    def __init__(self, root, url, app, budget=1800, resume_report=None):
        self.root, self.identity, self.kubeconfig, self.url = validate_inputs(root, url, app)
        self.app, self.namespace = app, self.identity['namespace']
        self.deadline, self.namespace_uid = time.monotonic() + budget, None
        self.lifecycle_id, self.last_operation, self.original = None, None, None
        self.lock = threading.Lock()
        self.resume_report = None
        suffix = ''
        if resume_report:
            original = Path(resume_report).resolve()
            require(original.is_file() and original.is_relative_to(self.root), 'resume report must be retained inside the original owned run')
            self.resume_report = json.loads(original.read_text())
            validate_import_resume(self.resume_report, app, self.namespace, self.identity['run_id'])
            suffix = '-resume-' + uuid.uuid4().hex[:12]
        self.report_path = self.root / ('local-k8s-prod-' + app + suffix + '.json')
        require(not self.report_path.exists(), 'never overwrite a previous Prod report')
        self.report = {'success': False, 'stage': 'build', 'prod_execution_passed': False,
                       'app_id': app, 'namespace': self.namespace, 'run_id': self.identity['run_id'],
                       'checks': [], 'timeline': [], 'tasks': [], 'artifacts': {},
                       'scope': 'real workspace/import/build/task GET/SSE/frozen bytes; not yet Prod or Lease acceptance',
                       'policy': 'no PVC/namespace deletion or purge; errors preserve original identity and evidence'}
        if resume_report:self.report['resumed_from'] = {'path': str(original), 'sha256': digest(original)}
        self.persist()

    def request(self, method, route, payload=None, raw=False, content_type='application/json', base=None):
        require(route.startswith('/') and not route.startswith('//'), 'relative owned HTTP route required')
        if method != 'GET':self.ownership()
        base = base or self.url
        require(base == self.url, 'another controller must have its own frozen process receipt before dispatch')
        data = payload if isinstance(payload, bytes) else json.dumps(payload).encode() if payload is not None else None
        intent = {'method': method, 'route': route, 'request': safe(payload) if isinstance(payload, dict)
                  else {'bytes': len(data or b''), 'sha256': hashlib.sha256(data or b'').hexdigest()},
                  'dispatched': method != 'GET', 'result_unknown': method != 'GET'}
        self.event('http_dispatch', intent)
        timeout = self.remaining(300)
        command = ['curl', '--silent', '--show-error', '--noproxy', '*', '--max-time', str(timeout),
                   '--request', method, '--header', 'Content-Type: ' + content_type,
                   '--header', 'Accept-Language: en-US', '--header', 'x-app-id: ' + self.app,
                   '--write-out', '\n%{http_code}']
        if data is not None:command += ['--data-binary', '@-']
        command.append(base + route)
        try:
            result = subprocess.run(command, input=data, capture_output=True, timeout=timeout + 1)
        except Exception as error:
            self.event('http_result_unknown', {**intent, 'cause': safe(str(error))});raise
        if result.returncode:
            self.event('http_result_unknown', {**intent, 'exit_code': result.returncode,
                       'cause': safe(result.stderr.decode(errors='replace'))})
            raise ContractFailure('HTTP transfer did not complete within its original absolute budget')
        content, status_text = result.stdout.rsplit(b'\n', 1)
        require(status_text.isdigit(), 'HTTP transport has no actual response status')
        status = int(status_text)
        if raw:
            self.event('http_bytes', {'method': method, 'route': route, 'status': status, 'bytes': len(content), 'sha256': hashlib.sha256(content).hexdigest()})
            require(status == 200, 'artifact/SSE HTTP request failed')
            return content
        body = json.loads(content)
        self.event('http_reply', {'method': method, 'route': route, 'status': status, 'body': body})
        if method == 'POST' and route == '/api/v1/userapp/init-project-template':
            require(status == 200 and body.get('success') is True
                    and body.get('message') == 'Project template initialized successfully'
                    and body.get('workspace_root') == '/home/user/' + self.app
                    and 'code' not in body and 'data' not in body,
                    'template import is not its documented flat response for this exact workspace: ' + str(safe(body)))
            return body
        require(status == 200 and body.get('success') is True and body.get('code') == '0000', 'real HTTP product failure: ' + str(safe(body)))
        return body['data']

    def build(self, version, import_source=True):
        marker = self.app + '-' + version.lower()
        archive = fixture_zip(marker)
        form, content_type = multipart(self.app, archive)
        if import_source:self.request('POST', '/api/v1/userapp/init-project-template', form, content_type=content_type)
        admitted = self.request('POST', '/api/v1/userapp/build', {'app_id': self.app})
        task_id = admitted.get('task_id', '')
        require(re.fullmatch(r'[A-Za-z0-9_-]+', task_id), 'real build admission has no task ID')
        query = urllib.parse.urlencode({'app_id': self.app})
        route = '/api/v1/userapp/tasks/' + task_id + '?' + query
        self.report['tasks'].append({'version': version, 'task_id': task_id, 'query': route});self.persist()
        deadline = min(self.deadline, time.monotonic() + 300)
        snapshot = None
        while time.monotonic() < deadline:
            snapshot = self.request('GET', route)
            require(snapshot.get('id') == task_id and snapshot.get('app_id') == self.app
                    and snapshot.get('kind') == 'build', 'original scoped build task identity changed or missing')
            if snapshot.get('status') in ('completed', 'failed', 'cancelled'):break
            time.sleep(1)
        require(snapshot and snapshot.get('status') == 'completed', 'original real build did not complete: ' + str(safe(snapshot)))
        require(isinstance(snapshot.get('release_id'), str) and re.fullmatch(r'[a-zA-Z0-9-]+', snapshot['release_id'])
                and re.fullmatch(r'[0-9a-f]{64}', snapshot.get('sha256', '')), 'build has no real artifact identity')
        stream = self.request('GET', '/api/v1/userapp/tasks/' + task_id + '/logs/stream?' + query + '&from_seq=0', raw=True)
        stream_path = self.root / ('prod-build-' + self.app + '-' + version + '.sse')
        require(not stream_path.exists(), 'never overwrite original task SSE');stream_path.write_bytes(stream)
        self.check('original build ' + version + ' has ordered unique completed SSE', True, completed_sse(stream, snapshot))
        download = '/api/v1/userapp/static/' + self.app + '?' + urllib.parse.urlencode({'release_id': snapshot['release_id']})
        artifact = self.request('GET', download, raw=True)
        observed = hashlib.sha256(artifact).hexdigest()
        require(observed == snapshot['sha256'], 'downloaded bytes differ from the real original build snapshot')
        path = self.root / ('prod-artifact-' + self.app + '-' + version + '.zip')
        require(not path.exists(), 'never overwrite frozen artifact');path.write_bytes(artifact);path.chmod(0o444)
        with zipfile.ZipFile(io.BytesIO(artifact)) as package:
            lock = package.read('release.lock.toml')
        entry = {'path': str(path), 'sha256': observed, 'release_id': snapshot['release_id'], 'task_id': task_id,
                 'marker': marker, 'release_lock_sha256': hashlib.sha256(lock).hexdigest()}
        self.report['artifacts'][version] = entry
        self.check('real artifact ' + version + ' bytes match original task SHA', True, entry)

    def run_build(self, artifact_image=None):
        self.ownership()
        server = self.kube(['config', 'view', '--minify', '-o', 'jsonpath={.clusters[0].cluster.server}'], json_output=False)
        require(server == self.identity['cluster_api'], 'private cluster API identity differs')
        before = self.input_proof();before['prod_harness_sha256'] = digest(__file__)
        self.report['inputs_before'] = before
        if self.resume_report:
            require(self.resume_report['inputs_before']['source']['source_inputs_sha256'] == before['source']['source_inputs_sha256'], 'resume source differs from original host build')
        else:
            require(not any(self.selected(kind) for kind in ('sts', 'pods', 'pvc')), 'Prod app already has owned builder resources; use a fresh declared app')
            created = self.request('POST', '/api/v1/userapp/workspace', {'app_id': self.app})
            require(created.get('app_id') == self.app and created.get('container_name'), 'workspace response is not the real declared application')
        builder = self.wait_physical(True);self.report['builder'] = builder
        lifecycle = self.request('GET', '/api/v1/userapp/' + self.app + '/lifecycle')
        require(lifecycle.get('state') == 'Active' and lifecycle.get('app_id') == self.app and lifecycle.get('lifecycle_id'), 'real active lifecycle unavailable')
        self.lifecycle_id = lifecycle['lifecycle_id'];self.report['lifecycle_id'] = self.lifecycle_id
        if self.resume_report:
            previous, lifecycle_id = validate_import_resume(self.resume_report, self.app, self.namespace, self.identity['run_id'])
            require(lifecycle_id == self.lifecycle_id and all(builder[k]['uid'] == previous[k]['uid'] for k in ('sts','pod','pvc'))
                    and builder['pvc']['volume_name'] == previous['pvc']['volume_name'], 'resume changed original lifecycle/Pod/PVC/STS/PV identity')
            with zipfile.ZipFile(io.BytesIO(fixture_zip(self.app + '-a'))) as zipped:
                expected = {name: hashlib.sha256(zipped.read(name)).hexdigest() for name in zipped.namelist()}
            code = 'import hashlib,json,sys\nfrom pathlib import Path\np=Path("/home/user")/sys.argv[1]\nfiles=json.loads(sys.argv[2])\nprint(json.dumps({"files":{n:hashlib.sha256((p/n).read_bytes()).hexdigest() for n in files},"artifacts":[x.name for x in (p/"builds").glob("workspace-package-*.zip")]}))'
            proof = json.loads(self.kube(['exec',builder['pod']['name'],'-c','agent','--','python3','-c',code,self.app,json.dumps(list(expected))],json_output=False))
            require(proof['files'] == expected and proof['artifacts'] == [], 'resume Source bytes differ or an actual artifact already exists')
            self.check('resume matches original imported Source and immutable physical identity',True,{'builder':builder,'source':proof})
        for version in ('A', 'B'):self.build(version, import_source=not (self.resume_report and version == 'A'))
        if artifact_image:
            proposals = artifact_proposal(self.namespace, self.identity['run_id'], self.app, artifact_image, self.report['artifacts'])
            proposal_path = self.root / ('prod-artifact-resources-' + self.app + '.json')
            require(not proposal_path.exists(), 'do not overwrite reviewed resource proposals')
            proposal_path.write_text(json.dumps(proposals, indent=2) + '\n')
            self.report['artifact_resource_proposal'] = str(proposal_path)
        self.report['execution_gates'] = ['Root authorizes actual mutations', 'second controller process/source receipt matches the same private PG/namespace',
            'both controllers can reach the actual Prod Pod IP ports 3010/60000', 'owned artifact Pod and ClusterIP serve the frozen ZIP SHA',
            'runtime image/Pod imageID and PVC UID are captured', 'real live hot holder and actual Lease are observed before conflict/wait dispatch']
        self.report['planned_prod_cases'] = ['cold real deploy and replay', 'hot holder cross-controller R2 seven fields',
            'R1 release within original 30s', 'R1 real 30s exhausted with no admission/late execution', 'R1 actual network disconnect then no late admission']
        after = self.input_proof();after['prod_harness_sha256'] = digest(__file__)
        self.check('build source host binary and harness remain frozen', before == after, {'before': before, 'after': after})
        self.report.update(success=True, stage='build_ready', prod_execution_passed=False)
        self.persist()

    def failure_evidence(self, error):
        self.report['error'] = safe(str(error));self.report['success'] = False
        self.report['cleanup'] = 'not attempted: original tasks/operations and all PVCs retained'
        captures = {}
        for kind in ('sts', 'pods', 'pvc'):
            try:captures[kind] = self.kube(['get', kind, '-l', 'rcoder.io/identifier=' + self.app, '-o', 'json'])
            except Exception as failure:captures[kind] = {'error': safe(str(failure))}
        # Do not export complete Pods: environments can contain private credentials.
        self.report['failure_resources'] = {kind: [{'kind': row.get('kind'), 'metadata': {k: row.get('metadata', {}).get(k) for k in ('name', 'namespace', 'uid', 'resourceVersion', 'labels')}}
            for row in value.get('items', [])] if isinstance(value, dict) and 'items' in value else value for kind, value in captures.items()}
        self.persist()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, type=Path)
    parser.add_argument('--url', required=True)
    parser.add_argument('--app', required=True)
    parser.add_argument('--artifact-image', help='Immutable OCI reference for a reviewed proposal; no resources are applied')
    parser.add_argument('--budget-seconds', type=int, default=1800)
    parser.add_argument('--resume-report', type=Path, help='Only resume the proved flat-import fixture failure; retain the original report and emit a new UUID report')
    args = parser.parse_args()
    require(600 <= args.budget_seconds <= 3600, 'build parent budget must be bounded')
    harness = None
    try:
        harness = ProdHarness(args.root, args.url, args.app, args.budget_seconds, args.resume_report)
        harness.run_build(args.artifact_image)
    except Exception as error:
        if harness:harness.failure_evidence(error)
        print(json.dumps({'success': False, 'stage': 'build', 'error': safe(str(error))}), flush=True);return 1
    print(json.dumps({'success': True, 'stage': 'build_ready', 'prod_execution_passed': False, 'report': str(harness.report_path)}), flush=True)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
