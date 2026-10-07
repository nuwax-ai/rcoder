#!/usr/bin/env python3
"""真实本机 K8s Dev 业务回归；故障只针对声明应用，永不删除卷或 namespace。

此脚本调用真实 RCoder HTTP、任务 GET/SSE 及捕获 Pod 内的管理/业务入口。
协议单元测试不代表此脚本的真实集群验收。失败保留现场，不补偿 Stop。
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
import zipfile

from app_cli_recovery import OWNER_IDENTITY_CODE
from local_k8s_core import ContractFailure, digest, require, safe, validate_inputs
from local_k8s_prod import ProdHarness, completed_sse, multipart


BUILD_SOURCE = r'''import json,os,time,zipfile
from pathlib import Path
root=Path('.');gate=root/'.business-build-gate'
if gate.exists():
 marker=gate.read_text();fields=Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()
 (root/'.business-build-entered').write_text(json.dumps({'gate':marker,'pid':os.getpid(),'start_time':fields[19]}))
 deadline=time.monotonic()+120
 while not (root/'.business-build-release').exists() or (root/'.business-build-release').read_text()!=marker:
  if time.monotonic()>deadline:raise RuntimeError('owned build gate expired without release')
  time.sleep(.1)
with (root/'builds.log').open('a') as log:log.write('actual-build\n')
with zipfile.ZipFile('artifact.zip','w',zipfile.ZIP_DEFLATED) as archive:
 for path in sorted(root.rglob('*')):
  if path.is_file() and not any(part in ('__pycache__','.git') for part in path.parts) and path.name not in ('artifact.zip','project.manifest.toml','builds.log','starts.log','migration-attempts.log') and not path.name.startswith('.business-'):
   archive.write(path,path.as_posix())
print('owned build completed',flush=True)
'''

# One exact previously executed fixture, not a fallback for arbitrary old state.
PRE_MANIFEST_FIX_BUILD_SHA256 = '3dc8b6f283ef42363fe05318ec3349716086f6ce82201bff800c5bca798534e9'


def fixture_files(app, run_id, version):
    require(re.fullmatch(r'[a-z][a-z0-9]{0,21}', app) and re.fullmatch(r'[0-9a-f]{32}', run_id)
            and version in ('a', 'b', 'c'), 'fixture must use declared literal application/version data')
    marker = app + '-' + version
    manifest = ('schema_version=1\n[project]\nservice_id="web"\nname="Owned Source ' + version + '"\n'
                'type="python"\nkind="web"\n[build]\ncommand=["python3","build.py"]\nartifact="artifact.zip"\n'
                '[devbuild]\ncommand=["python3","build.py"]\n[run]\ncommand=["python3","main.py"]\n'
                'migrate=["python3","scripts/migrate.py"]\nshutdown_timeout_seconds=3\n'
                '[devrun]\ncommand=["python3","main.py"]\n[health]\nreadiness_path="/"\n'
                '[proxy]\npath="/"\nstrip_prefix=false\n')
    main = '''import os
from pathlib import Path
from http.server import BaseHTTPRequestHandler,HTTPServer
MARKER=%r
with Path('starts.log').open('a') as log:log.write(MARKER+'\\n')
class Handler(BaseHTTPRequestHandler):
 def do_GET(self):
  self.send_response(200);self.end_headers();self.wfile.write(MARKER.encode())
HTTPServer(('0.0.0.0',int(os.environ['PORT'])),Handler).serve_forever()
''' % marker
    migrate = '''import sys
from pathlib import Path
with Path('migration-attempts.log').open('a') as log:log.write('actual-exit-17\\n')
print('owned-migration-stdout-before-exit-17',flush=True)
print('owned-migration-stderr-before-exit-17',file=sys.stderr,flush=True)
sys.exit(17)
'''
    return {'workspace.manifest.toml': 'schema_version=1\n[workspace]\nname="owned-k8s-business"\n',
            '.local-k8s-business-sentinel': 'owned-' + run_id,
            'web/project.manifest.toml': manifest, 'web/main.py': main,
            'web/build.py': BUILD_SOURCE, 'web/scripts/migrate.py': migrate}


def source_zip(files):
    out = io.BytesIO()
    with zipfile.ZipFile(out, 'w', zipfile.ZIP_DEFLATED) as archive:
        for name, content in files.items():archive.writestr(name, content)
    return out.getvalue()


def task_identity(snapshot, app, task_id, kind):
    require(snapshot.get('id') == task_id and snapshot.get('app_id') == app and snapshot.get('kind') == kind,
            'original scoped business task identity changed')
    require(snapshot.get('status') in ('pending', 'running', 'completed', 'failed', 'cancelled'), 'unknown task state cannot be success')


def resume_report_identity(old, app, run_id, namespace):
    require(old.get('success') is False and old.get('app_id') == app and old.get('run_id') == run_id
            and old.get('namespace') == namespace, 'resume report is not this original failed owned application')
    tasks = old.get('tasks', [])
    require(len(tasks) == 1 and tasks[0].get('kind') == 'build', 'resume only this pre-start pure-build failure')
    captured = tasks[0];require(re.fullmatch(r'[A-Za-z0-9_-]+', captured.get('id', '')), 'original failed task ID absent')
    query = urllib.parse.urlencode({'app_id': app})
    require(captured.get('route') == '/api/v1/userapp/tasks/' + captured['id'] + '?' + query
            and captured.get('sse_route') == '/api/v1/userapp/tasks/' + captured['id'] + '/logs/stream?' + query + '&from_seq=0',
            'resume may query only the exact original scoped task')
    pending = old.get('pending_business_request', {})
    require(pending.get('app_id') == app and pending.get('action') == 'build' and pending.get('receipt_verified') is True
            and pending.get('acceptance_unknown') is False and pending.get('task_id') == captured['id'], 'original build admission identity is unknown')
    allowed_posts = {'/api/v1/userapp/workspace', '/api/v1/userapp/init-project-template', '/api/v1/userapp/build'}
    for event in old.get('timeline', []):
        evidence = event.get('evidence', {})
        require(not (event.get('stage') == 'http_dispatch' and evidence.get('method') == 'POST' and evidence.get('route') not in allowed_posts),
                'old execution has Start/Restart or another mutation; this narrow resume is not authorized')
    require('captured_owner_before_kill' not in old, 'never replay a report that already dispatched a process fault')
    resources = [item['evidence'] for item in old.get('checks', [])
                 if item.get('name') == 'actual builder Pod container PVC identities captured' and item.get('passed') is True]
    require(len(resources) == 1, 'original physical builder identity absent or ambiguous')
    return captured, resources[0]


def gated_stop_report_identity(old, proof, app, run_id, namespace):
    require(old.get('success') is False and old.get('app_id') == app and old.get('run_id') == run_id
            and old.get('namespace') == namespace and old.get('namespace_uid'), 'gated resume is not this failed owned execution')
    tasks = old.get('tasks', [])
    require(len(tasks) == 3 and [task.get('kind') for task in tasks] == ['build', 'dev_start', 'dev_restart']
            and len({task.get('id') for task in tasks}) == 3, 'gated resume must retain the original build, Source start and gated restart identities')
    required = {'real packaged artifact includes migration scripts', 'start actually builds current Source and serves HTTP',
                'actual migration exit 17 is diagnosed and HTTP remains available'}
    passed = {check.get('name') for check in old.get('checks', []) if check.get('passed') is True}
    require(required <= passed and all(check.get('passed') is True for check in old.get('checks', []))
            and 'captured_owner_before_kill' not in old, 'prior Source stage failed or an owner fault was already dispatched')
    physical = old.get('resume_origin', {}).get('physical')
    require(isinstance(physical, dict) and physical.get('pod', {}).get('uid'), 'original builder physical identity absent')
    captured = tasks[-1]
    for task in tasks:
        require(re.fullmatch(r'[A-Za-z0-9_-]+', task.get('id', '')), 'prior business task ID absent or invalid')
        query = urllib.parse.urlencode({'app_id': app})
        require(task.get('route') == '/api/v1/userapp/tasks/' + task['id'] + '?' + query
                and task.get('sse_route') == '/api/v1/userapp/tasks/' + task['id'] + '/logs/stream?' + query + '&from_seq=0',
                'prior business task route is not its exact original app scope')
    for key in ('task_before', 'task_after', 'stop'):
        envelope = proof.get(key, {})
        require(envelope.get('success') is True and envelope.get('code') == '0000', 'manual Stop proof lacks an actual successful ' + key + ' envelope')
    before, after = proof['task_before']['data'], proof['task_after']['data']
    for record in (before, after):task_identity(record, app, captured['id'], 'dev_restart')
    require(before['status'] == 'running' and before.get('stage') == 'building' and after['status'] == 'cancelled'
            and after.get('seq', 0) > before.get('seq', 0) and proof['stop']['data'].get('app_id') == app
            and proof.get('pod_uid') == physical['pod']['uid'], 'manual Stop must cancel the same actual running build in the same Pod')
    gate = proof.get('gate_before', {})
    require(gate.get('gate') == 'gate-' + run_id and gate.get('alive') is True and isinstance(gate.get('pid'), int)
            and gate['pid'] > 1 and isinstance(gate.get('start_time'), str) and gate['start_time'].isdigit()
            and gate.get('cwd') == '/home/user/' + app + '/web', 'manual Stop lacks the captured owned build window')
    return captured, physical


def gate_exit_confirmed(captured, observed):
    gate = observed.get('gate') or {}
    return observed.get('gate_process_alive') is False and all(gate.get(key) == captured.get(key) for key in ('gate', 'pid', 'start_time'))


def task_sse(raw, snapshot, expected):
    events = []
    for block in raw.decode('utf-8').replace('\r\n', '\n').split('\n\n'):
        lines = block.splitlines()
        ids = [line[3:].strip() for line in lines if line.startswith('id:')]
        data = [line[5:].lstrip() for line in lines if line.startswith('data:')]
        if not data:continue
        require(len(ids) == 1 and ids[0].isdigit(), 'actual SSE data must retain its real sequence')
        events.append({'sequence': int(ids[0]), 'event': json.loads('\n'.join(data))})
    sequence = [item['sequence'] for item in events]
    require(sequence and sequence == sorted(set(sequence)), 'SSE sequence regressed or repeated')
    terminals = [item for item in events if item['event'].get('event') in ('completed', 'failed', 'cancelled')]
    require(snapshot.get('status') == expected and len(terminals) == 1
            and terminals[0] == events[-1] and terminals[0]['event'].get('event') == expected,
            'unique final SSE terminal must agree with the same original task GET')
    if expected == 'completed':
        require(any(item['event'].get('event') == 'service_start_ok' and item['event'].get('service') == 'web' for item in events),
                'build_ok alone does not establish actual Source startup')
        require(not any(item['event'].get('event') == 'service_start_fail' and item['event'].get('service') == 'web' for item in events),
                'failed service startup cannot be a business success')
    return events


def agent_container(snapshot):
    statuses = [item for item in snapshot.get('pod', {}).get('image_status', []) if item.get('name') == 'agent']
    require(len(statuses) == 1 and statuses[0].get('containerID') and statuses[0].get('imageID'), 'actual agent container identity absent')
    return {key: statuses[0][key] for key in ('containerID', 'imageID')}


def same_physical(before, after):
    require(before['pod']['uid'] == after['pod']['uid'] and before['pvc']['uid'] == after['pvc']['uid']
            and before['pvc']['volume_name'] == after['pvc']['volume_name'] and agent_container(before) == agent_container(after),
            'in-container recovery must preserve the actual Pod, container, PVC and PV identity')


PROCESS_FAULT_CODE = r'''
import hashlib,select,time
def owned_process(pid,authority,proc=Path('/proc')):
 p=proc/str(pid);fields=(p/'stat').read_text().rsplit(')',1)[1].split();exe=(p/'exe').stat()
 env=dict(part.split(bytes([61]),1) for part in (p/'environ').read_bytes().split(bytes([0])) if bytes([61]) in part)
 source=authority['workspace'];state=authority['state_root'];app=authority['application_id']
 require(fields[0]!='Z' and env.get(b'PROJECT_ID')==app.encode() and env.get(b'APP_CLI_RUNTIME_WORKSPACE')==source.encode()
  and env.get(b'APP_CLI_STATE_ROOT')==state.encode(),'app-cli process is not a live verified application binding',pid=pid)
 return {'pid':pid,'start_time':fields[19],'executable_id':[exe.st_dev,exe.st_ino],
  'cmdline_sha256':hashlib.sha256((p/'cmdline').read_bytes()).hexdigest()}
def signal_captured_processes(owner,captured,proc=Path('/proc')):
 require(hasattr(os,'pidfd_open') and hasattr(signal,'pidfd_send_signal'),'precise pidfd support is required for fault injection')
 require(captured and len({entry['pid'] for entry in captured})==len(captured)
  and owner['pid'] in [entry['pid'] for entry in captured],'captured process set absent, duplicated or missing the owner')
 validate_captured_owner(owner,proc)
 descriptors=[]
 try:
  for entry in captured:descriptors.append((entry,os.pidfd_open(entry['pid'])))
  validate_captured_owner(owner,proc)
  for entry,fd in descriptors:
   require(owned_process(entry['pid'],owner['authority'],proc)==entry,'captured app-cli process changed before signal',pid=entry['pid'])
  for entry,fd in descriptors:signal.pidfd_send_signal(fd,signal.SIGKILL)
  pending={fd for entry,fd in descriptors};poll=select.poll()
  for fd in pending:poll.register(fd,select.POLLIN)
  deadline=time.monotonic()+10
  while pending and time.monotonic()<deadline:
   for fd,flags in poll.poll(100):
    if flags&select.POLLIN:pending.discard(fd);poll.unregister(fd)
  require(not pending,'captured app-cli process exit not confirmed')
  return {'signal':9,'captured_count':len(captured),'all_captured_exit_confirmed':True,
   'pids':[{'pid':entry['pid'],'start_time':entry['start_time']} for entry in captured]}
 finally:
  for entry,fd in descriptors:os.close(fd)
'''


GATE_PROCESS_CODE = r'''
def verify_build_process(gate,source,cwd,argv,executable_id,trusted_python_id):
 require(isinstance(gate.get('pid'),int) and gate['pid']>1 and isinstance(gate.get('start_time'),str)
  and gate['start_time'].isdigit(),'gate process identity is incomplete')
 require(cwd==source/'web' and executable_id==trusted_python_id,'gate native Python executable or declared workspace differs')
 allowed=[['python3','build.py'],['/usr/bin/python3','build.py'],['/usr/bin/python3','python3','build.py']]
 require(argv in allowed,'gate is not the exact native or observed wrapped Python build command',argv=argv,executable_id=executable_id)
'''


# The owner lock verification is shared with the real Docker recovery harness.
# Neither discovery content nor a raw PID authorizes the controlled signal.
POD_PROGRAM = OWNER_IDENTITY_CODE + PROCESS_FAULT_CODE + GATE_PROCESS_CODE + r'''
import select,time,subprocess,urllib.request,urllib.error
app,pod_uid,mode,payload=sys.argv[1:];payload=json.loads(payload)
source=Path('/home/user')/app;state=source/'state'/app
require(source.resolve(strict=True)==source and state.resolve(strict=True)==state,'physical source/state escaped declared application')
require(os.environ.get('PROJECT_ID')==app and os.environ.get('APP_CLI_RUNTIME_WORKSPACE')==str(source)
 and os.environ.get('APP_CLI_STATE_ROOT')==str(state) and os.environ.get('RCODER_PHYSICAL_POD_UID')==pod_uid,
 'container platform application/Pod binding differs')
def request(port,route):
 headers={}
 if port==3010:
  token=os.environ.get('APP_CLI_DEPLOY_TOKEN') or (state/'token').read_text().strip()
  headers['X-Deploy-Token']=token
 try:
  with urllib.request.urlopen(urllib.request.Request('http://127.0.0.1:'+str(port)+route,headers=headers),timeout=2) as response:
   raw=response.read()
   return {'status':response.status,'body':json.loads(raw) if port==3010 else raw.decode()}
 except urllib.error.HTTPError as error:return {'status':error.code,'body':json.loads(error.read()) if port==3010 else None}
 except urllib.error.URLError as error:return {'status':None,'transport_error':type(error.reason).__name__}
def kernel(route):
 value=request(3010,route);require(value['status']==200 and value['body'].get('success') is True,'management request did not succeed',route=route,status=value['status'])
 return value['body']['data']
def lines(name):
 path=source/'web'/name
 return path.read_text().splitlines() if path.exists() else []
def signature(pid):
 p=Path('/proc')/str(pid);fields=(p/'stat').read_text().rsplit(')',1)[1].split()
 return {'pid':pid,'start_time':fields[19],'state':fields[0]}
def alive(capture):
 try:value=signature(capture['pid']);return value['start_time']==capture['start_time'] and value['state']!='Z'
 except FileNotFoundError:return False
if mode=='write':
 allowed={'web/main.py','web/project.manifest.toml','web/build.py','web/.business-build-gate','web/.business-build-release'}
 require(payload and set(payload)<=allowed,'only owned Source/gate files may be changed')
 for name,content in payload.items():
  target=source/name;require(target.resolve(strict=False).is_relative_to(source),'fixture path escaped declared source')
  temporary=target.with_name(target.name+'.business-write');temporary.write_text(content);os.replace(temporary,target)
 result={'written':sorted(payload)}
elif mode=='probe':
 entered=source/'web/.business-build-entered';gate=json.loads(entered.read_text()) if entered.exists() else None
 if gate and alive(gate):
  proc=Path('/proc')/str(gate['pid']);argv=[part.decode() for part in (proc/'cmdline').read_bytes().split(bytes([0])) if part]
  executable=(proc/'exe').stat();trusted_python=Path('/usr/bin/python3').stat()
  verify_build_process(gate,source,(proc/'cwd').resolve(),argv,[executable.st_dev,executable.st_ino],[trusted_python.st_dev,trusted_python.st_ino])
 result={'business':request(9080,'/'),'runtime_identity':request(3010,'/v1/runtime/identity'),
  'runtime_status':request(3010,'/v1/runtime/status'),'sentinel_matches':(source/'.local-k8s-business-sentinel').read_text()==payload['sentinel'],
  'source_root':str(source),'source_files_sha256':{name:hashlib.sha256((source/name).read_bytes()).hexdigest() for name in
   ('workspace.manifest.toml','.local-k8s-business-sentinel','web/project.manifest.toml','web/main.py','web/build.py','web/scripts/migrate.py')},
  'builds':lines('builds.log'),'starts':lines('starts.log'),'migrations':lines('migration-attempts.log'),
  'gate':gate,'gate_process_alive':alive(gate) if gate else False}
elif mode=='operation':
 task_id=payload['task_id'];matches=[]
 for path in (state/'operations').glob('*.json'):
  stored=json.loads(path.read_text());req=stored['request'];view=stored['view']
  if req.get('request_context')!=task_id:continue
  identity=kernel('/v1/runtime/identity')
  require(identity.get('application_id')==app and identity.get('source_root')==str(source)
   and req.get('operation_id')==view.get('operation_id') and req.get('workspace_id')==identity['workspace_id']
   and req.get('kind')==view.get('kind') and req.get('kind') in ('start','restart')
   and req.get('profile',{}).get('profile')=='source' and req['profile'].get('input',{}).get('workspace_id')==identity['workspace_id'],
   'original task is not bound to a real Source receipt')
  operation=kernel('/v1/runtime/operations/'+view['operation_id'])
  events=kernel('/v1/runtime/operations/'+view['operation_id']+'/events?after_seq=0')
  require(operation['operation_id']==req['operation_id'] and operation.get('runtime_instance_id')==view.get('runtime_instance_id')
   and operation.get('request_digest')==view.get('request_digest') and operation.get('request_digest'),'original runtime receipt differs from query')
  require(all(event.get('operation_id')==req['operation_id'] for event in events['events']),'runtime events changed original operation')
  matches.append({'operation':operation,'events':events,'request_context':task_id,'source':True,'current_runtime_instance_id':identity['runtime_instance_id']})
 result=matches
elif mode in ('owner','kill'):
 if mode=='owner':
  value=subprocess.run(['/usr/local/bin/app-cli','owner','status','--workspace',str(source)],capture_output=True,text=True,timeout=10)
  require(value.returncode==0,'native owner status failed')
  native=json.loads(value.stdout);identity=kernel('/v1/runtime/identity')
  generation=json.loads((state/'work'/native['generation']/'generation.json').read_text());domain=generation['physical_domain']
  require(domain.get('instance')==pod_uid and domain.get('authority','').startswith('k8s:') and domain.get('volume','').startswith('pvc:'+payload['pvc_name']+':'),'owner physical domain differs from actual Pod/PVC')
  authority={'state_root':str(state),'workspace':str(source),'application_id':app,'native':native,'kernel':identity,'physical_domain':domain}
  owner=observe_owner(authority);owner['authority']=authority
  binary=Path('/usr/local/bin/app-cli').stat();processes=[]
  for item in Path('/proc').iterdir():
   if not item.name.isdigit():continue
   try:
    exe=(item/'exe').stat()
    if [exe.st_dev,exe.st_ino]!=[binary.st_dev,binary.st_ino]:continue
    if signature(int(item.name))['state']=='Z':continue
    processes.append(owned_process(int(item.name),authority))
   except FileNotFoundError:continue
  require(processes and owner['pid'] in [entry['pid'] for entry in processes],'verified owner absent from controlled process set')
  result={'owner':owner,'processes':processes}
 else:
  result=signal_captured_processes(payload['owner'],payload['processes'])
else:raise RuntimeError('unsupported owned business fixture mode')
print(json.dumps(result))
'''


class BusinessHarness(ProdHarness):
    def __init__(self, root, url, app, budget=1800, resume_failed_build_report=None, resume_namespace_proof=None,
                 resume_after_gated_stop_report=None, resume_stop_proof=None):
        self.root, self.identity, self.kubeconfig, self.url = validate_inputs(root, url, app)
        self.app, self.namespace = app, self.identity['namespace']
        self.deadline, self.namespace_uid = time.monotonic() + budget, None
        self.lifecycle_id, self.original, self.builder = None, None, None
        self.last_operation, self.pending_compute_request = None, None
        self.lock = threading.Lock()
        self.resume_path = Path(resume_failed_build_report).resolve() if resume_failed_build_report else None
        self.resume_namespace_proof = Path(resume_namespace_proof).resolve() if resume_namespace_proof else None
        self.gated_resume_path = Path(resume_after_gated_stop_report).resolve() if resume_after_gated_stop_report else None
        self.gated_stop_proof = Path(resume_stop_proof).resolve() if resume_stop_proof else None
        require(bool(self.resume_path) == bool(self.resume_namespace_proof), 'resume requires both the original failed report and prior namespace UID proof')
        require(bool(self.gated_resume_path) == bool(self.gated_stop_proof) and not (self.resume_path and self.gated_resume_path),
                'choose only one explicit resume stage and supply its exact original evidence')
        if self.resume_path:
            require(self.resume_path.is_file() and self.resume_path.parent == self.root
                    and self.resume_path.name == 'local-k8s-business-' + app + '.json'
                    and self.resume_namespace_proof.is_file() and self.resume_namespace_proof.parent == self.root
                    and self.resume_namespace_proof.stat().st_mtime <= self.resume_path.stat().st_mtime,
                    'resume evidence must be the owned original report and an older same-run namespace proof')
        if self.gated_resume_path:
            require(self.gated_resume_path.is_file() and self.gated_resume_path.parent == self.root
                    and self.gated_resume_path.name == 'local-k8s-business-' + app + '-resumed.json'
                    and self.gated_stop_proof.is_file() and self.gated_stop_proof.parent == self.root
                    and self.gated_stop_proof.stat().st_mtime >= self.gated_resume_path.stat().st_mtime,
                    'gated resume needs the original same-run report and later manual Stop proof')
        suffix = '-resumed2' if self.gated_resume_path else '-resumed' if self.resume_path else ''
        self.report_path = self.root / ('local-k8s-business-' + app + suffix + '.json')
        require(not self.report_path.exists(), 'never overwrite a previous business report')
        self.report = {'success': False, 'app_id': app, 'namespace': self.namespace, 'run_id': self.identity['run_id'],
                       'scope': 'real K8s Dev Source build/start, advisory migration, build Stop and same-container owner recovery',
                       'checks': [], 'timeline': [], 'tasks': [], 'pending_business_request': None,
                       'policy': 'no PVC/namespace deletion, precise owned PID faults only, failure preserves scene'}
        self.persist()

    def resume_source(self, before):
        old = json.loads(self.resume_path.read_text())
        captured, physical = resume_report_identity(old, self.app, self.identity['run_id'], self.namespace)
        for key in ('source', 'binary', 'binary_sha256', 'host_pid', 'host_process_start_and_executable', 'source_bound_to_build', 'harness_sha256'):
            require(old.get('inputs_before', {}).get(key) == before.get(key), 'original source/host build or process binding changed before resume: ' + key)
        self.original = physical;self.builder = physical
        same_physical(physical, self.physical(True))
        snapshot = self.request('GET', captured['route'])
        task_identity(snapshot, self.app, captured['id'], 'build')
        require(snapshot['status'] == 'failed' and snapshot.get('error') ==
                'start_file web/project.manifest.toml: invalid Zip archive: Duplicate filename: web/project.manifest.toml',
                'resume requires the actual original Failed duplicate-manifest build')
        raw = self.request('GET', captured['sse_route'], raw=True)
        events = task_sse(raw, snapshot, 'failed')
        current = self.probe()
        expected = {name: hashlib.sha256(text.encode()).hexdigest() for name, text in fixture_files(self.app, self.identity['run_id'], 'a').items()}
        expected['web/build.py'] = PRE_MANIFEST_FIX_BUILD_SHA256
        require(current.get('source_root') == '/home/user/' + self.app and current.get('source_files_sha256') == expected
                and current['sentinel_matches'] and not current['starts'] and not current['migrations']
                and current['business'].get('status') is None, 'original fixture/source changed or business was already executed; do not overwrite it')
        self.report['resume_origin'] = {'path': str(self.resume_path), 'report_sha256': digest(self.resume_path),
            'namespace_proof': str(self.resume_namespace_proof), 'namespace_proof_sha256': digest(self.resume_namespace_proof),
            'task': snapshot, 'sse': events, 'actual_sse_sha256': hashlib.sha256(raw).hexdigest(), 'physical': physical, 'source_before': current}
        self.check('original failed pure build and unchanged owned source/container/volume confirmed', True, self.report['resume_origin'])
        self.exec_mode('write', {'web/build.py': BUILD_SOURCE})
        after = self.probe();expected['web/build.py'] = hashlib.sha256(BUILD_SOURCE.encode()).hexdigest()
        require(after['source_files_sha256'] == expected and after['builds'] == current['builds'] and after['starts'] == current['starts']
                and after['migrations'] == current['migrations'] and after['sentinel_matches'], 'repair changed more than the fixture build.py')
        self.check('only fixture build.py corrected; source and data retained', True, {'new_build_py_sha256': expected['web/build.py'], 'physical': self.physical(True)})

    def resume_gated_stop(self, before):
        old = json.loads(self.gated_resume_path.read_text());proof = json.loads(self.gated_stop_proof.read_text())
        captured, physical = gated_stop_report_identity(old, proof, self.app, self.identity['run_id'], self.namespace)
        for key in ('source', 'binary', 'binary_sha256', 'host_pid', 'host_process_start_and_executable', 'source_bound_to_build', 'harness_sha256'):
            require(old.get('inputs_before', {}).get(key) == before.get(key), 'source/host process changed before gated resume: ' + key)
        self.original = physical;self.builder = physical;same_physical(physical, self.physical(True))
        snapshot = self.snapshot(captured)
        require(snapshot['status'] == 'cancelled', 'old gated request is not actually Cancelled; do not replay or create another build')
        raw = self.request('GET', captured['sse_route'], raw=True);events = task_sse(raw, snapshot, 'cancelled')
        current = self.probe();expected = {name: hashlib.sha256(text.encode()).hexdigest() for name, text in fixture_files(self.app, self.identity['run_id'], 'a').items()}
        prior_start = next(check['evidence']['business'] for check in old['checks'] if check['name'] == 'start actually builds current Source and serves HTTP')
        status = current['runtime_status'].get('body', {});gate = current.get('gate') or {}
        require(current.get('source_files_sha256') == expected and current['sentinel_matches'] and current['business'].get('status') is None
                and status.get('success') is True and status.get('data', {}).get('desired') == 'stopped'
                and status.get('data', {}).get('observed') == 'stopped' and status.get('data', {}).get('recovery_protection') is False
                and current['runtime_identity'].get('body', {}).get('success') is True
                and all(current[key] == prior_start[key] for key in ('builds', 'starts', 'migrations'))
                and current['gate_process_alive'] is False and all(gate.get(key) == proof['gate_before'][key] for key in ('gate', 'pid', 'start_time')),
                'old execution cleanup, stopped management, exact Source/data or original build exit has not been confirmed')
        # Re-query the prior real Start; retain its actual operation/SSE, never
        # replace the original failed report or relabel its five checks.
        started = old['tasks'][1];start_snapshot = self.snapshot(started)
        start_sse = task_sse(self.request('GET', started['sse_route'], raw=True), start_snapshot, 'completed')
        operations = self.exec_mode('operation', {'task_id': started['id']})
        prior_runtime = next(check['evidence']['runtime_operation_id'] for check in old['checks']
                             if check['name'] == 'actual migration exit 17 is diagnosed and HTTP remains available')
        require(len(operations) == 1 and operations[0]['operation'].get('operation_id') == prior_runtime
                and operations[0]['operation'].get('state') == 'succeeded', 'prior succeeded Source operation identity changed')
        self.report['prior_stage_checks'] = old['checks']
        self.report['resume_gate_origin'] = {'path': str(self.gated_resume_path), 'report_sha256': digest(self.gated_resume_path),
            'manual_stop_path': str(self.gated_stop_proof), 'manual_stop_sha256': digest(self.gated_stop_proof),
            'prior_task_ids': [task['id'] for task in old['tasks']], 'cancelled_task': snapshot, 'cancelled_sse': events,
            'source_start_task': start_snapshot, 'source_start_sse': start_sse, 'source_operation': operations,
            'physical': physical, 'stopped': current}
        self.check('original gated Stop Cancelled and prior real Source evidence retained in same physical application', True, self.report['resume_gate_origin'])

    def exec_mode(self, mode, payload):
        current = self.physical(True)
        same_physical(self.builder, current)
        raw = self.kube(['exec', self.builder['pod']['name'], '-c', 'agent', '--', 'python3', '-c', POD_PROGRAM,
                         self.app, self.builder['pod']['uid'], mode, json.dumps(payload)], json_output=False)
        return json.loads(raw)

    def probe(self):
        return self.exec_mode('probe', {'sentinel': 'owned-' + self.identity['run_id']})

    def wait(self, label, observe, accept, cap=120):
        deadline = min(self.deadline, time.monotonic() + cap)
        last = None
        while time.monotonic() < deadline:
            last = observe()
            if accept(last):return last
            time.sleep(.5)
        raise ContractFailure(label + ' deadline ended: ' + str(safe(last)))

    def task(self, action):
        route = '/api/v1/userapp/' + ('build' if action == 'build' else 'dev/' + action)
        self.report['pending_business_request'] = {'app_id': self.app, 'action': action, 'route': route,
                                                   'receipt_verified': False, 'acceptance_unknown': True}
        self.persist()
        admitted = self.request('POST', route, {'app_id': self.app})
        task_id = admitted.get('task_id', '')
        require(re.fullmatch(r'[A-Za-z0-9_-]+', task_id), 'real business admission has no task ID')
        previous_ids = [task['id'] for task in self.report['tasks']]
        if self.report.get('resume_origin'):previous_ids.append(self.report['resume_origin']['task']['id'])
        if self.report.get('resume_gate_origin'):previous_ids.extend(self.report['resume_gate_origin']['prior_task_ids'])
        require(task_id not in previous_ids, 'corrected input must create a new task, never replay historical success/failure')
        query = urllib.parse.urlencode({'app_id': self.app})
        captured = {'id': task_id, 'kind': 'build' if action == 'build' else 'dev_' + action,
                    'route': '/api/v1/userapp/tasks/' + task_id + '?' + query,
                    'sse_route': '/api/v1/userapp/tasks/' + task_id + '/logs/stream?' + query + '&from_seq=0'}
        self.report['pending_business_request'].update(receipt_verified=True, acceptance_unknown=False, task_id=task_id)
        self.report['tasks'].append(captured.copy());self.persist()
        return captured

    def snapshot(self, captured):
        record = self.request('GET', captured['route'])
        task_identity(record, self.app, captured['id'], captured['kind'])
        return record

    def terminal(self, captured, expected='completed'):
        snapshot = self.wait('original task terminal', lambda: self.snapshot(captured),
                             lambda record: record['status'] in ('completed', 'failed', 'cancelled'), cap=300)
        require(snapshot['status'] == expected, 'original business task failed: ' + str(safe(snapshot)))
        raw = self.request('GET', captured['sse_route'], raw=True)
        if captured['kind'] == 'build':events = completed_sse(raw, snapshot)
        else:events = task_sse(raw, snapshot, expected)
        path = self.root / ('business-task-' + self.app + '-' + captured['id'] + '.json')
        require(not path.exists(), 'never overwrite original task evidence')
        evidence = {'task': snapshot, 'sse': events, 'actual_sse_sha256': hashlib.sha256(raw).hexdigest()}
        path.write_text(json.dumps(safe(evidence), indent=2) + '\n')
        self.event('original_task_terminal', {'id': captured['id'], 'status': expected, 'evidence_path': str(path)})
        return evidence

    def started(self, action, marker):
        before = self.probe();captured = self.task(action);result = self.terminal(captured)
        actual = self.wait('actual business HTTP', self.probe, lambda value: value['business'].get('status') == 200 and value['business'].get('body') == marker)
        operation = self.exec_mode('operation', {'task_id': captured['id']})
        require(len(operation) == 1 and operation[0]['operation'].get('state') == 'succeeded'
                and operation[0]['operation'].get('kind') in (('start', 'restart') if action == 'start' else ('restart',)),
                'completed task is not bound to its own succeeded Source operation')
        self.check(action + ' actually builds current Source and serves HTTP', len(actual['builds']) == len(before['builds']) + 1 and actual['sentinel_matches'],
                   {'task_id': captured['id'], 'runtime': operation, 'physical': self.physical(True), 'business': actual})
        return result, actual, operation[0]

    def stopped(self):
        result = self.request('POST', '/api/v1/userapp/dev/stop', {'app_id': self.app})
        require(result.get('app_id') == self.app, 'Stop response differs from original application')
        def confirmed(value):
            body = value['runtime_status'].get('body', {})
            return value['business'].get('status') is None and body.get('success') is True \
                and body.get('data', {}).get('desired') == 'stopped' and body.get('data', {}).get('observed') == 'stopped'
        actual = self.wait('confirmed business Stop with retained management', self.probe, confirmed)
        require(actual['runtime_identity'].get('body', {}).get('success') is True, 'Stop removed management identity')
        return actual

    def run_business(self):
        if self.gated_resume_path:
            old = json.loads(self.gated_resume_path.read_text())
            require(old.get('run_id') == self.identity['run_id'] and old.get('namespace') == self.namespace and old.get('namespace_uid'),
                    'gated report has no captured same-run namespace UID')
            self.namespace_uid = old['namespace_uid']
        if self.resume_namespace_proof:
            proof = json.loads(self.resume_namespace_proof.read_text())
            require(proof.get('run_id') == self.identity['run_id'] and proof.get('namespace') == self.namespace, 'prior namespace proof belongs to another run')
            matches = [check['evidence'] for check in proof.get('checks', []) if check.get('name') == 'owned namespace identity confirmed' and check.get('passed') is True]
            require(len(matches) == 1 and matches[0].get('name') == self.namespace and matches[0].get('uid'), 'prior actual namespace UID proof absent')
            self.namespace_uid = matches[0]['uid']
        namespace = self.ownership();self.report['namespace_uid'] = namespace['uid']
        server = self.kube(['config', 'view', '--minify', '-o', 'jsonpath={.clusters[0].cluster.server}'], json_output=False)
        require(server == self.identity['cluster_api'], 'private API differs from owned local cluster')
        before = self.input_proof();before['business_harness_sha256'] = digest(__file__)
        self.report['inputs_before'] = before
        if self.gated_resume_path:
            self.resume_gated_stop(before)
        elif self.resume_path:
            self.resume_source(before)
        else:
            require(not any(self.selected(kind) for kind in ('sts', 'pods', 'pvc')), 'declare a fresh isolated business app')
            workspace = self.request('POST', '/api/v1/userapp/workspace', {'app_id': self.app})
            require(workspace.get('app_id') == self.app and workspace.get('container_name'), 'workspace not the real declared app')
            self.original = self.wait_physical(True);self.builder = self.original
            self.check('actual builder Pod container PVC identities captured', True, {**self.builder, 'agent': agent_container(self.builder)})
            form, content_type = multipart(self.app, source_zip(fixture_files(self.app, self.identity['run_id'], 'a')))
            self.request('POST', '/api/v1/userapp/init-project-template', form, content_type=content_type)
        if not self.gated_resume_path:
            pure = self.task('build');artifact_task = self.terminal(pure)['task']
            raw = self.request('GET', '/api/v1/userapp/static/' + self.app + '?' + urllib.parse.urlencode({'release_id': artifact_task['release_id']}), raw=True)
            require(hashlib.sha256(raw).hexdigest() == artifact_task['sha256'], 'actual build bytes do not match original task SHA')
            with zipfile.ZipFile(io.BytesIO(raw)) as artifact:
                require(artifact.read('web/scripts/migrate.py').decode() == fixture_files(self.app, self.identity['run_id'], 'a')['web/scripts/migrate.py'], 'actual artifact omitted the runtime scripts directory')
            self.check('real packaged artifact includes migration scripts', True, {'task_id': pure['id'], 'sha256': artifact_task['sha256']})
            result, actual, initial_operation = self.started('start', self.app + '-a')
            events = [item['event'] for item in result['sse']]
            logs = [event.get('line', '') for event in events if event.get('event') == 'log' and event.get('service') == 'web']
            self.check('actual migration exit 17 is diagnosed and HTTP remains available', len(actual['migrations']) == 1
                       and any('owned-migration-stdout-before-exit-17' in line for line in logs)
                       and any('owned-migration-stderr-before-exit-17' in line for line in logs)
                       and any('run.migrate' in line and 'Exit' in line and '17' in line and 'phase=migration' in line for line in logs),
                       {'task_id': result['task']['id'], 'runtime_operation_id': initial_operation['operation']['operation_id'], 'logs': logs})
            self.stopped()
        gate_marker = 'gate-' + self.identity['run_id'] + ('-resumed2' if self.gated_resume_path else '')
        self.exec_mode('write', {'web/.business-build-gate': gate_marker})
        gated = self.task('restart')
        gate = self.wait('actual gated build entry', self.probe,
                         lambda value: (value.get('gate') or {}).get('gate') == gate_marker and value['gate_process_alive'])
        gate_task = self.snapshot(gated)
        require(gate_task['status'] == 'running' and gate_task.get('stage') == 'building', 'Stop fault window is not the original running build')
        stopped = self.stopped();cancelled = self.terminal(gated, 'cancelled')
        stopped = self.wait('original gated build physical exit', self.probe,
                            lambda value: gate_exit_confirmed(gate['gate'], value), cap=30)
        self.exec_mode('write', {'web/.business-build-release': gate_marker})
        for index in range(8):
            observed = self.probe();record = self.snapshot(gated)
            require(record['status'] == 'cancelled' and observed['business'].get('status') is None and observed['starts'] == gate['starts']
                    and observed['runtime_status'].get('body', {}).get('data', {}).get('desired') == 'stopped', 'cancelled build or late callback relaunched business')
            time.sleep(.5)
        self.check('Stop during actual build cancels original task without late startup', self.exec_mode('operation', {'task_id': gated['id']}) == [],
                   {'original_task': cancelled['task'], 'actual_build': gate['gate'], 'stop': stopped})
        files = fixture_files(self.app, self.identity['run_id'], 'b')
        self.exec_mode('write', {name: files[name] for name in ('web/main.py', 'web/project.manifest.toml')})
        self.started('start', self.app + '-b')
        captured = self.exec_mode('owner', {'pvc_name': self.builder['pvc']['name']})
        self.report['captured_owner_before_kill'] = captured;self.persist()
        self.check('actual unique owner holds the stable kernel lock', captured['owner']['owner_lock']['holder_pid'] == captured['owner']['pid'], captured)
        signal = self.exec_mode('kill', captured)
        self.check('precise captured app-cli SIGKILL confirms physical exits', signal['all_captured_exit_confirmed'], signal)
        files = fixture_files(self.app, self.identity['run_id'], 'c')
        self.exec_mode('write', {name: files[name] for name in ('web/main.py', 'web/project.manifest.toml')})
        _, resumed, recovered_operation = self.started('restart', self.app + '-c')
        replacement = self.exec_mode('owner', {'pvc_name': self.builder['pvc']['name']})
        same_physical(self.builder, self.physical(True))
        self.check('original restart入口 restores a new owner within the same actual container and volume',
                   replacement['owner']['runtime_instance_id'] != captured['owner']['runtime_instance_id']
                   and replacement['owner']['owner_lock']['file_id'] == captured['owner']['owner_lock']['file_id'] and resumed['sentinel_matches'],
                   {'old': captured['owner'], 'new': replacement['owner'], 'original_source_operation': recovered_operation, 'physical': self.physical(True)})
        final = self.stopped()
        self.check('successful business Stop keeps management and workspace data', final['sentinel_matches'], final)
        after = self.input_proof();after['business_harness_sha256'] = digest(__file__)
        self.check('source host binary and business harness remain frozen', before == after, {'before': before, 'after': after})
        self.report.update(success=True, inputs_after=after, final_business='stopped', compute='running; Root may separately Stop computation', volume_retained=True)
        self.persist()

    def failure_evidence(self, error):
        self.report.update(success=False, error=safe(str(error)), cleanup='none; original tasks/operations, namespace and all PVCs retained')
        if self.builder is not None:
            try:self.report['failure_business_probe'] = self.probe()
            except Exception as failure:self.report['failure_business_probe'] = {'capture_error': safe(str(failure))}
        for captured in self.report['tasks']:
            try:captured['failure_get'] = self.snapshot(captured)
            except Exception as failure:captured['failure_get'] = {'capture_error': safe(str(failure))}
            # Capture bounded real SSE even for a still-running original task.
            # A truncated diagnostic stream is explicitly not a passing terminal.
            try:
                timeout = self.remaining(5)
                transfer = subprocess.run(['curl', '--silent', '--show-error', '--noproxy', '*', '--max-time', str(timeout),
                    '--header', 'x-app-id: ' + self.app, '--header', 'Accept-Language: en-US', '--write-out', '\n%{http_code}',
                    self.url + captured['sse_route']], capture_output=True, timeout=timeout + 1)
                raw, status = transfer.stdout.rsplit(b'\n', 1)
                captured['failure_sse'] = {'http_status': status.decode(errors='replace'), 'curl_exit': transfer.returncode,
                    'transfer_complete': transfer.returncode == 0, 'text': safe(raw.decode(errors='replace')),
                    'actual_bytes_sha256': hashlib.sha256(raw).hexdigest(), 'cause': safe(transfer.stderr.decode(errors='replace'))}
            except Exception as failure:captured['failure_sse'] = {'capture_error': safe(str(failure))}
            if self.builder is not None and captured['kind'] != 'build':
                try:captured['failure_original_runtime'] = self.exec_mode('operation', {'task_id': captured['id']})
                except Exception as failure:captured['failure_original_runtime'] = {'capture_error': safe(str(failure))}
        self.persist()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, type=Path);parser.add_argument('--url', required=True);parser.add_argument('--app', required=True)
    parser.add_argument('--budget-seconds', type=int, default=1800)
    parser.add_argument('--resume-failed-build-report', type=Path, help='仅续跑首次 duplicate-manifest pure-build 失败，另写 resumed 报告')
    parser.add_argument('--resume-namespace-proof', type=Path, help='本 runDir 早于原失败报告的真实 namespace UID 证据')
    parser.add_argument('--resume-after-gated-stop-report', type=Path, help='保留已完成 Source 阶段，核验原取消构建后新建独立 Stop 窗口')
    parser.add_argument('--resume-stop-proof', type=Path, help='同一原构建的实际 Running→DevStop→Cancelled 取证')
    args = parser.parse_args();require(600 <= args.budget_seconds <= 3600, 'business stages need an explicit bounded parent budget')
    harness = None
    try:
        harness = BusinessHarness(args.root, args.url, args.app, args.budget_seconds,
                                  args.resume_failed_build_report, args.resume_namespace_proof,
                                  args.resume_after_gated_stop_report, args.resume_stop_proof);harness.run_business()
    except Exception as error:
        if harness:harness.failure_evidence(error)
        print(json.dumps({'success': False, 'error': safe(str(error)), 'report': str(harness.report_path) if harness else None}), flush=True);return 1
    print(json.dumps({'success': True, 'checks': len(harness.report['checks']), 'report': str(harness.report_path)}), flush=True);return 0


if __name__ == '__main__':
    raise SystemExit(main())
