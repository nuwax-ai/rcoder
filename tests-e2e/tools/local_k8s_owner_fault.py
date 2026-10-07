#!/usr/bin/env python3
"""续跑已完成 Source/构建 Stop 的真实 K8s owner 故障验证，保留全部旧报告和卷。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import threading
import time

import local_k8s_business as business
from local_k8s_core import digest, require, safe, validate_inputs


CANONICAL_OWNER_CODE = r'''
def canonical_owner_argv(raw,executable_id,trusted_executable_id):
 require(executable_id==trusted_executable_id,'owner native executable is not the trusted immutable app-cli',
  observed_executable_id=executable_id,expected_executable_id=trusted_executable_id)
 if len(raw)>3 and raw[:3]==['/usr/local/bin/app-cli','app-cli','serve']:
  return [raw[0],*raw[2:]]
 require(len(raw)>2 and raw[0] in ('/usr/local/bin/app-cli','app-cli') and raw[1]=='serve',
  'owner argv is not the exact native or observed wrapped serve command',argv=raw)
 return list(raw)
'''


PROCESS_ENUMERATION_CODE = r'''
import http.client,socket,xmlrpc.client
def registered_executions(owner):
 root=Path(owner['authority']['state_root'])
 receipt=json.loads((root/'work'/owner['generation']/'supervisord-engine.json').read_text())
 require(receipt.get('generation')==owner['generation'] and receipt.get('supervisor_id')==owner['supervisor_id']
  and receipt.get('socket')=='/var/run/supervisor.sock','registered service engine differs from captured native generation')
 class Connection(http.client.HTTPConnection):
  def connect(self):
   self.sock=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);self.sock.settimeout(2);self.sock.connect(receipt['socket'])
 class Transport(xmlrpc.client.Transport):
  def make_connection(self,host):return Connection(host)
 with xmlrpc.client.ServerProxy('http://localhost/RPC2',transport=Transport()) as client:
  supervisor_pid=client.supervisor.getPID();require(isinstance(supervisor_pid,int) and supervisor_pid>0,'supervisord identity absent')
  observations=[]
  for name in ('app-svc-web','app-pingap'):
   info=client.supervisor.getProcessInfo(name)
   require(info.get('name')==info.get('group')==name and info.get('state')==20 and info.get('statename')=='RUNNING'
    and isinstance(info.get('pid'),int) and info['pid']>1,'registered application execution is not confirmed live',program=name)
   observations.append({key:info[key] for key in ('name','group','state','statename','pid','start')})
  require(client.supervisor.getPID()==supervisor_pid,'supervisord instance changed during registered identity observation')
 return observations
def capture_cli_processes(owner,registered,proc=Path('/proc'),binary=Path('/usr/local/bin/app-cli')):
 trusted=binary.stat();trusted_id=[trusted.st_dev,trusted.st_ino]
 required={owner['pid'],*(item['pid'] for item in registered)}
 candidates=sorted(required)+sorted(int(item.name) for item in proc.iterdir() if item.name.isdigit() and int(item.name) not in required)
 processes=[];inaccessible=[];observations=[]
 for pid in candidates:
  matched_cli=False
  try:
   path=proc/str(pid);exe=(path/'exe').stat();executable_id=[exe.st_dev,exe.st_ino];matched_cli=executable_id==trusted_id
   fields=(path/'stat').read_text().rsplit(')',1)[1].split()
   if pid in required:
    # Unix run-service execs Python/Pingap in place. These are observed, never
    # mistaken for additional app-cli targets or confirmed cleanup.
    require(fields[0]!='Z','registered application execution became a zombie',pid=pid)
    proof=owned_process(pid,owner['authority'],proc)
    observations.append({'process':proof,'native_app_cli':matched_cli})
   elif matched_cli:
    if fields[0]=='Z':continue
    proof=owned_process(pid,owner['authority'],proc)
   else:continue
   if matched_cli:processes.append(proof)
  except PermissionError:
   require(pid not in required and not matched_cli,'registered or native app-cli execution identity unreadable; no signal authorized',pid=pid)
   inaccessible.append({'pid':pid,'observation':'permission_denied_unknown','exit_confirmed':False})
  except FileNotFoundError:
   require(pid not in required,'registered execution disappeared before identity confirmation; no signal authorized',pid=pid)
   inaccessible.append({'pid':pid,'observation':'disappeared_unknown','exit_confirmed':False})
 require(processes and owner['pid'] in [item['pid'] for item in processes],'actual native lock holder absent from captured app-cli targets')
 validate_captured_owner(owner,proc)
 return {'processes':processes,'registered_observations':observations,'unknown_inaccessible_candidates':inaccessible,
  'scope':'verified same-application native app-cli targets only; unknown candidates are not signalled or declared exited'}
'''


def owner_code(original):
    """Keep the original raw argv in the existing lock/PID revalidation contract."""
    replacements = {
        "    argv=[v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]\n":
        "    raw_argv=[v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]\n"
        "    observed_exe=(p/'exe').stat();trusted_exe=Path('/usr/local/bin/app-cli').stat()\n"
        "    argv=canonical_owner_argv(raw_argv,[observed_exe.st_dev,observed_exe.st_ino],[trusted_exe.st_dev,trusted_exe.st_ino])\n",
        "    result={'pid':pid,'start_time':fields[19],'argv':argv,\n":
        "    result={'pid':pid,'start_time':fields[19],'argv':argv,'raw_argv':raw_argv,\n",
        "and [v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]==argv,":
        "and [v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]==raw_argv,",
    }
    for old, new in replacements.items():
        require(original.count(old) == 1, 'shared owner identity source changed; re-review before adapting its wrapper contract')
        original = original.replace(old, new, 1)
    return CANONICAL_OWNER_CODE + original


require(business.POD_PROGRAM.startswith(business.OWNER_IDENTITY_CODE), 'owner identity must remain the authoritative program prefix')
_OLD_ENUMERATION = '''  binary=Path('/usr/local/bin/app-cli').stat();processes=[]
  for item in Path('/proc').iterdir():
   if not item.name.isdigit():continue
   try:
    exe=(item/'exe').stat()
    if [exe.st_dev,exe.st_ino]!=[binary.st_dev,binary.st_ino]:continue
    if signature(int(item.name))['state']=='Z':continue
    processes.append(owned_process(int(item.name),authority))
   except FileNotFoundError:continue
  require(processes and owner['pid'] in [entry['pid'] for entry in processes],'verified owner absent from controlled process set')
  result={'owner':owner,'processes':processes}'''
_NEW_ENUMERATION = '''  registered=registered_executions(owner)
  candidates=capture_cli_processes(owner,registered)
  # Re-read each authoritative program binding before publishing target IDs.
  require(registered_executions(owner)==registered,'registered application execution changed during capture; no signal authorized')
  result={'owner':owner,**candidates}'''
require(business.POD_PROGRAM.count(_OLD_ENUMERATION) == 1, 'shared enumeration source changed; review identity boundaries before adapting it')
POD_PROGRAM = owner_code(business.OWNER_IDENTITY_CODE) + PROCESS_ENUMERATION_CODE + business.POD_PROGRAM[len(business.OWNER_IDENTITY_CODE):].replace(_OLD_ENUMERATION, _NEW_ENUMERATION, 1)


def origin_identity(old, app, run_id, namespace):
    require(old.get('success') is False and old.get('app_id') == app and old.get('run_id') == run_id
            and old.get('namespace') == namespace and old.get('namespace_uid'), 'owner continuation belongs to another successful/unknown execution')
    guards = []
    for line in old.get('error', '').splitlines():
        if line.startswith('RuntimeError: '):
            guards.append(json.loads(line[len('RuntimeError: '):]))
    require(len(guards) == 1 and guards[0].get('owner_guard') == 'captured process is not a live serve owner'
            and guards[0].get('argv') == ['/usr/local/bin/app-cli', 'app-cli', 'serve', '--workspace', '/home/user/' + app]
            and isinstance(guards[0].get('pid'), int) and guards[0]['pid'] > 1,
            'continue only the exact observed wrapper guard failure, never another cleanup or runtime failure')
    require('captured_owner_before_kill' not in old, 'a process fault was already captured/dispatched; this continuation may not replay it')
    required = {'Stop during actual build cancels original task without late startup', 'start actually builds current Source and serves HTTP'}
    require(required <= {item.get('name') for item in old.get('checks', []) if item.get('passed') is True}
            and all(item.get('passed') is True for item in old.get('checks', [])), 'the prior Stop/Source B stage has not passed')
    tasks = old.get('tasks', [])
    require(len(tasks) == 2 and [item.get('kind') for item in tasks] == ['dev_restart', 'dev_start']
            and len({item.get('id') for item in tasks}) == 2, 'original Stop build and Source B tasks are missing or ambiguous')
    for item in tasks:
        require(re.fullmatch(r'[A-Za-z0-9_-]+', item.get('id', '')) and item.get('route') ==
                '/api/v1/userapp/tasks/' + item['id'] + '?app_id=' + app and item.get('sse_route') ==
                '/api/v1/userapp/tasks/' + item['id'] + '/logs/stream?app_id=' + app + '&from_seq=0', 'original task route changed app scope')
    physical = old.get('resume_gate_origin', {}).get('physical')
    require(isinstance(physical, dict) and physical.get('pod', {}).get('uid'), 'captured physical application is absent')
    return tasks, physical


class OwnerFaultHarness(business.BusinessHarness):
    def __init__(self, root, url, app, report, budget=900, report_suffix=None):
        self.root, self.identity, self.kubeconfig, self.url = validate_inputs(root, url, app)
        self.app, self.namespace = app, self.identity['namespace']
        self.origin_path = Path(report).resolve()
        require(self.origin_path.is_file() and self.origin_path.parent == self.root
                and self.origin_path.name == 'local-k8s-business-' + app + '-resumed2.json', 'explicit original same-run report required')
        self.old = json.loads(self.origin_path.read_text())
        self.old_tasks, self.original = origin_identity(self.old, app, self.identity['run_id'], self.namespace)
        self.builder = self.original;self.namespace_uid = self.old['namespace_uid']
        self.deadline = time.monotonic() + budget;self.lifecycle_id = None
        self.last_operation, self.pending_compute_request = None, None
        self.lock = threading.Lock()
        require(report_suffix in (None, 'second'), 'only the explicit second attempt suffix is supported; no automatic fault replay')
        self.prior_attempt_path = self.root / ('local-k8s-owner-fault-' + app + '.json') if report_suffix else None
        self.prior_attempt = None
        if self.prior_attempt_path:
            require(self.prior_attempt_path.is_file(), 'preserve and verify the first failed attempt before second dispatch')
            previous = json.loads(self.prior_attempt_path.read_text())
            require(previous.get('success') is False and previous.get('run_id') == self.identity['run_id'] and previous.get('app_id') == app
                    and previous.get('namespace_uid') == self.namespace_uid and previous.get('namespace') == self.namespace
                    and 'fault_dispatch' not in previous and 'captured_owner_before_kill' not in previous and previous.get('tasks') == []
                    and 'PermissionError:' in previous.get('error', '') and '/proc/' in previous.get('error', '')
                    and previous.get('origin', {}).get('path') == str(self.origin_path)
                    and previous['origin'].get('sha256') == digest(self.origin_path), 'prior fault is unknown/dispatched or not this permission-only pre-signal attempt')
            self.prior_attempt = previous
        self.report_path = self.root / ('local-k8s-owner-fault-' + app + ('-' + report_suffix if report_suffix else '') + '.json')
        require(not self.report_path.exists(), 'never overwrite any previous owner fault report')
        prior_ids = [item['id'] for item in self.old_tasks] + self.old.get('resume_gate_origin', {}).get('prior_task_ids', [])
        self.report = {'success': False, 'app_id': app, 'namespace': self.namespace, 'namespace_uid': self.namespace_uid,
            'run_id': self.identity['run_id'], 'scope': 'real K8s precise app-cli SIGKILL and original Source restart after validated prior stages',
            'checks': [], 'timeline': [], 'tasks': [], 'pending_business_request': None,
            'resume_gate_origin': {'prior_task_ids': prior_ids},
            'policy': 'no workload/PVC creation or deletion, no shared resources, no fault replay, failure keeps all original evidence'}
        self.persist()

    def exec_mode(self, mode, payload):
        current = self.physical(True);business.same_physical(self.builder, current)
        raw = self.kube(['exec', self.builder['pod']['name'], '-c', 'agent', '--', 'python3', '-c', POD_PROGRAM,
                         self.app, self.builder['pod']['uid'], mode, json.dumps(payload)], json_output=False)
        return json.loads(raw)

    def proof(self):
        result = self.input_proof()
        paths = [Path(__file__), Path(business.__file__), Path(business.ProdHarness.request.__code__.co_filename)]
        result['continuation_files_sha256'] = {str(path.resolve()): digest(path) for path in paths}
        return result

    def run_fault(self):
        self.ownership()
        server = self.kube(['config', 'view', '--minify', '-o', 'jsonpath={.clusters[0].cluster.server}'], json_output=False)
        require(server == self.identity['cluster_api'], 'local private API binding differs')
        before = self.proof();self.report['inputs_before'] = before
        for key in ('source', 'binary', 'binary_sha256', 'host_pid', 'host_process_start_and_executable', 'source_bound_to_build', 'harness_sha256'):
            require(self.old.get('inputs_before', {}).get(key) == before.get(key), 'original host source/build/process changed: ' + key)
            if self.prior_attempt:
                require(self.prior_attempt.get('inputs_before', {}).get(key) == before.get(key), 'first pre-signal attempt source/process changed: ' + key)
        business.same_physical(self.builder, self.physical(True))
        live = self.probe();expected = {name: hashlib.sha256(text.encode()).hexdigest() for name, text in business.fixture_files(self.app, self.identity['run_id'], 'b').items()}
        require(live['source_files_sha256'] == expected and live['sentinel_matches'] and live['business'] == {'status': 200, 'body': self.app + '-b'}
                and all(live[key] == self.old['failure_business_probe'][key] for key in ('builds', 'starts', 'migrations'))
                and live['runtime_identity'] == self.old['failure_business_probe']['runtime_identity'],
                'Source B, original owner, data or build inputs changed before continuation')
        if self.prior_attempt:
            prior = self.prior_attempt['failure_business_probe']
            require(all(live[key] == prior[key] for key in ('source_files_sha256','builds','starts','migrations','runtime_identity')),
                    'Source/input/owner changed after first pre-signal attempt')
            self.report['prior_attempt'] = {'path': str(self.prior_attempt_path), 'sha256': digest(self.prior_attempt_path),
                'no_signal_dispatch': True, 'original_error': self.prior_attempt['error']}
        snapshots = []
        for captured, expected_status in zip(self.old_tasks, ('cancelled', 'completed')):
            snapshot = self.snapshot(captured)
            events = business.task_sse(self.request('GET', captured['sse_route'], raw=True), snapshot, expected_status)
            snapshots.append({'task': snapshot, 'sse': events})
        original_operation = self.exec_mode('operation', {'task_id': self.old_tasks[1]['id']})
        expected_operation = next(check['evidence']['runtime'][0]['operation']['operation_id'] for check in self.old['checks']
                                  if check['name'] == 'start actually builds current Source and serves HTTP')
        require(len(original_operation) == 1 and original_operation[0]['operation']['operation_id'] == expected_operation
                and original_operation[0]['operation']['state'] == 'succeeded', 'Source B query substituted the original succeeded operation')
        self.report['origin'] = {'path': str(self.origin_path), 'sha256': digest(self.origin_path), 'prior_checks': self.old['checks'],
            'prior_source_checks': self.old.get('prior_stage_checks', []), 'validated_tasks': snapshots,
            'original_source_operation': original_operation, 'physical': self.builder, 'source_b': live}
        self.check('prior real Stop and Source B retain original task operation physical and source identities', True, self.report['origin'])
        captured = self.exec_mode('owner', {'pvc_name': self.builder['pvc']['name']})
        self.report['captured_owner_before_kill'] = captured;self.persist()
        self.check('verified native owner holds the exact stable kernel flock', captured['owner']['owner_lock']['holder_pid'] == captured['owner']['pid'], captured)
        # Persist dispatch intent before exec. A lost result must never be replayed.
        self.report['fault_dispatch'] = {'pod_uid': self.builder['pod']['uid'], 'owner_pid': captured['owner']['pid'],
            'owner_start_time': captured['owner']['start_time'], 'signal': 9, 'result_unknown': True};self.persist()
        result = self.exec_mode('kill', captured)
        require(result['all_captured_exit_confirmed'] is True, 'signals have no confirmed physical process exits')
        self.report['fault_dispatch'].update(result_unknown=False, result=result)
        self.check('only captured own app-cli processes receive SIGKILL and physically exit', True, result)
        files = business.fixture_files(self.app, self.identity['run_id'], 'c')
        self.exec_mode('write', {name: files[name] for name in ('web/main.py', 'web/project.manifest.toml')})
        _, resumed, operation = self.started('restart', self.app + '-c')
        replacement = self.exec_mode('owner', {'pvc_name': self.builder['pvc']['name']})
        business.same_physical(self.builder, self.physical(True))
        self.check('original Source restart restores a new owner in the same Pod container PVC and stable lock inode',
            replacement['owner']['runtime_instance_id'] != captured['owner']['runtime_instance_id']
            and replacement['owner']['owner_lock']['file_id'] == captured['owner']['owner_lock']['file_id'] and resumed['sentinel_matches'],
            {'old': captured['owner'], 'new': replacement['owner'], 'original_new_source_operation': operation, 'physical': self.physical(True)})
        final = self.stopped();self.check('actual final business Stop keeps management and workspace data', final['sentinel_matches'], final)
        after = self.proof();self.check('host binary source and continuation code stay frozen', before == after, {'before': before, 'after': after})
        self.report.update(success=True, inputs_after=after, final_business='stopped', compute='running; Root separately stops computation', volume_retained=True)
        self.persist()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, type=Path);parser.add_argument('--url', required=True);parser.add_argument('--app', required=True)
    parser.add_argument('--prior-report', required=True, type=Path);parser.add_argument('--budget-seconds', type=int, default=900)
    parser.add_argument('--report-suffix', choices=['second'], help='核验首轮仅 PermissionError 且未派发信号，另写 second 报告')
    args = parser.parse_args();require(300 <= args.budget_seconds <= 1800, 'owner fault parent stages require an explicit bounded budget')
    harness = None
    try:
        harness = OwnerFaultHarness(args.root, args.url, args.app, args.prior_report, args.budget_seconds, args.report_suffix);harness.run_fault()
    except Exception as error:
        if harness:harness.failure_evidence(error)
        print(json.dumps({'success': False, 'error': safe(str(error)), 'report': str(harness.report_path) if harness else None}), flush=True);return 1
    print(json.dumps({'success': True, 'checks': len(harness.report['checks']), 'report': str(harness.report_path)}), flush=True);return 0


if __name__ == '__main__':
    raise SystemExit(main())
