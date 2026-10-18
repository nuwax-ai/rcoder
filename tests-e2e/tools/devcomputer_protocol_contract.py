#!/usr/bin/env python3
"""Main DevComputer HTTP -> real Runner/ACP process, not a model/AI fixture.

Requires an explicitly owned isolated Compose controller and the strict runner.
No command is retried after a possible write. Existing services/volumes are never
removed. httpx-sse decodes the wire; normal delivery is held until the original
fresh controlled ACP witness proves a live consumer. Fast completion before first subscribe is
a separate, deliberately unclaimed retention contract.
"""
import argparse
import asyncio
import base64
import contextvars
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.parse
import uuid

OBSERVATION_DEADLINE = contextvars.ContextVar('original_turn_deadline', default=None)

STEPS = {
    'devcomputer frozen controller and owned Runner identity',
    'devcomputer explicit Main entry and default reload propagation',
    'devcomputer first real SID and sequenced original terminal',
    'devcomputer pending hint never reaches ACP Load',
    'devcomputer shared source projects have independent real owners',
    'devcomputer default reload reaps original and reads changed source',
    'devcomputer reload A preserves B actual owner',
    'devcomputer real SID Stop Load preserves data and Git',
    'devcomputer each original accepted prompt dispatched exactly once',
    'devcomputer preparation state never becomes a public session',
    'devcomputer default English invalid ACP cause and real cleanup',
    'devcomputer fixture source stayed unchanged',
    'devcomputer captured Runner cleanup retains volumes',
    'devcomputer preexisting containers preserved',
}


def require(value, message):
    if not value:
        raise AssertionError(message)


def positive_frame(event, sid):
    """Known empty keepalives are not application events or sequence witnesses."""
    if event.event == 'message' and event.data == '':
        return None
    value = json.loads(event.data)
    require(isinstance(value, dict) and value.get('sessionId') == sid, 'SSE session identity differs')
    if event.event == 'ping' or value.get('messageType') == 'heartbeat':
        return None
    require(event.id is not None and re.fullmatch(r'[0-9]+', event.id) is not None,
            'Main application event has no real sequence')
    sequence = int(event.id)
    require(sequence > 0, 'Synthetic sequence zero is not an accepted task terminal')
    return {'seq': sequence, 'event': event.event, 'data': value}


def successful_terminal(frame, sid, rid):
    value = frame['data']
    return (frame['event'] == 'end_turn' and frame['seq'] > 0
            and value.get('sessionId') == sid and value.get('messageType') == 'sessionPromptEnd'
            and value.get('subType') == 'end_turn' and value.get('data', {}).get('request_id') == rid
            and value['data'].get('reason') == 'EndTurn' and value['data'].get('error_message') is None)


def container_witness(row):
    """Container identity and execution/mount state, excluding changing counters."""
    return {'id': row['Id'], 'name': row['Name'], 'image': row['Image'],
            'labels': row['Config'].get('Labels') or {},
            'running': row['State']['Running'], 'started_at': row['State']['StartedAt'],
            'restart_count': row['RestartCount'],
            'mounts': sorted(row['Mounts'], key=lambda mount: json.dumps(mount, sort_keys=True))}


class Contract:
    def __init__(self, args):
        self.args = args
        self.root = Path(os.environ['E2E_REPORT_DIR']) / 'devcomputer-protocol'
        self.root.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.source = Path(os.environ.get('E2E_SOURCE_ROOT', Path(__file__).resolve().parents[2]))
        self.fixture = self.source / 'crates/agent_runner/tests/fixtures/controlled_acp.py'
        self.fixture_bytes = self.fixture.read_bytes()
        self.fixture_sha = hashlib.sha256(self.fixture_bytes).hexdigest()
        self.case = os.environ['E2E_CASE_ID']
        require(re.fullmatch(r'[0-9a-f]{32}', self.case) is not None, 'Strict case identity is required')
        self.user = 'dv' + self.case
        self.projects = [self.user + '-' + letter for letter in 'abc']
        self.remote = '/home/user/' + self.user
        self.cwd = self.remote + '/source'
        self.state = self.remote + '/fixture'
        self.script = self.state + '/controlled_acp.py'
        self.cid = None
        self.captured = None
        self.pending_http = []
        self.pending_turns = []
        self.secrets = [os.environ.get('RCODER_API_KEY', '')]
        self.report = {'success': False, 'checks': [], 'requests': [], 'intents': {}, 'turns': [],
            'case_id': self.case, 'user_id': self.user, 'projects': self.projects,
            'evidence_level': 'real Main HTTP, real Rust Runner, controlled ACP child; no AI/model calls',
            'fixture_sha256': self.fixture_sha, 'fast_before_first_subscribe_covered': False,
            'normal_delivery_barrier': 'fresh positive-sequence controlled subscriber witness', 'cleanup': {}}

    def save(self, name, value):
        encoded = json.dumps(value, ensure_ascii=False, indent=2)
        for secret in filter(None, self.secrets):
            encoded = encoded.replace(secret, '<credential>')
        (self.root / name).write_text(encoded + '\n')

    def persist(self):
        self.save('result.json', self.report)
        self.save('assertions.json', self.report['checks'])

    def check(self, name, ok, detail=None):
        self.report['checks'].append({'name': name, 'ok': bool(ok), 'detail': detail})
        self.persist()
        require(ok, name)

    def docker(self, *args, timeout=20):
        result = subprocess.run(['docker', *args], capture_output=True, text=True,
                                timeout=self.observation_timeout(timeout))
        require(result.returncode == 0, 'Docker observation/control failed: ' + result.stderr[-1200:])
        return result.stdout

    def observation_timeout(self, maximum):
        deadline = OBSERVATION_DEADLINE.get()
        if deadline is None:
            return maximum
        remaining = deadline - time.monotonic()
        require(remaining > 0, 'Original turn deadline expired before Docker observation')
        return min(maximum, remaining)

    def inspect(self, cid):
        rows = json.loads(self.docker('inspect', cid))
        require(len(rows) == 1 and rows[0]['Id'] == cid, 'Exact Docker identity changed')
        return rows[0]

    def container_snapshot(self, ids):
        if not ids:
            return {}
        rows = json.loads(self.docker('inspect', *sorted(ids)))
        require({row['Id'] for row in rows} == set(ids), 'Container observation is incomplete')
        return {row['Id']: container_witness(row) for row in rows}

    def fence(self, running=True):
        row = self.inspect(self.cid)
        labels = row['Config'].get('Labels') or {}
        require((not running or row['State']['Running']) and labels.get('service-type') == 'computer-agent-runner'
                and labels.get('identifier') == self.user and row['Image'] == self.captured['image'],
                'Owned Runner physical identity differs')

    def remote_python(self, program, payload=None):
        self.fence()
        result = subprocess.run(['docker', 'exec', '-i', self.cid, 'python3', '-c', program],
            input=json.dumps(payload), capture_output=True, text=True, timeout=self.observation_timeout(20))
        require(result.returncode == 0, 'Owned fixture observation failed: ' + result.stderr[-1200:])
        return json.loads(result.stdout)

    def file(self, name, text=None, *, remove=False):
        require(name in ('source/source.txt', 'source/sentinel.txt') or
                re.fullmatch(r'fixture/[a-zA-Z0-9_.-]+', name) is not None, 'Fixture path outside owned root')
        return self.remote_python('''import json,pathlib,sys
p=json.load(sys.stdin);path=pathlib.Path(p['root'])/p['name']
if p['remove']:path.unlink(missing_ok=True);value=None
elif p['text'] is not None:
 path.parent.mkdir(parents=True,exist_ok=True);path.write_text(p['text']);value=p['text']
else:value=path.read_text()
print(json.dumps(value))''', {'root': self.remote, 'name': name, 'text': text, 'remove': remove})

    def journal(self):
        return self.remote_python('''import json,pathlib,sys
p=json.load(sys.stdin);f=pathlib.Path(p['root'])/'protocol.jsonl'
print(json.dumps([json.loads(x)for x in f.read_text().splitlines(keepends=True)
 if x.endswith('\\n')]if f.exists()else []))''',
            {'root': self.state})

    def processes(self, project):
        rows = [r for r in self.journal() if r['event'] == 'spawn' and r['project'] == project]
        return self.remote_python('''import json,pathlib,sys
p=json.load(sys.stdin);result=[]
for pid in sorted(set(p['pids'])):
 d=pathlib.Path('/proc')/str(pid)
 try:stat=(d/'stat').read_text();argv=(d/'cmdline').read_bytes().split(b'\\0');environment=(d/'environ').read_bytes().split(b'\\0')
 except FileNotFoundError:continue
 tail=stat[stat.rfind(')')+2:].split()
 if tail[0]=='Z':raise RuntimeError('owned ACP child is a zombie')
 if p['script'].encode()not in argv:continue
 if ('AGENT_PROJECT_ID='+p['project']).encode()not in environment:continue
 result.append({'pid':pid,'start':tail[19]})
print(json.dumps(result))''', {'pids': [r['pid'] for r in rows], 'script': self.script,'project':project})

    def original_absent(self, captured):
        return self.remote_python('''import json,pathlib,sys
p=json.load(sys.stdin)
try:s=(pathlib.Path('/proc')/str(p['pid'])/'stat').read_text()
except FileNotFoundError:print('true')
else:print(json.dumps(s[s.rfind(')')+2:].split()[19]!=p['start']))''', captured)

    def git(self, seed=None):
        return self.remote_python('''import json,pathlib,subprocess,sys
p=json.load(sys.stdin);root=pathlib.Path(p['root'])
def git(*args):
 r=subprocess.run(['git','-C',str(root),*args],capture_output=True,text=True,check=True)
 return r.stdout.strip()
if p['seed'] is not None:
 (root/'sentinel.txt').write_text(p['seed']);git('init','-q');git('add','source.txt','sentinel.txt')
 git('-c','user.name=E2E','-c','user.email=e2e@example.invalid','-c','commit.gpgsign=false','commit','-qm','owned source witness')
print(json.dumps({'commit':git('rev-parse','HEAD'),'blob':git('show','HEAD:sentinel.txt'),
 'sentinel':(root/'sentinel.txt').read_text(),'source':(root/'source.txt').read_text()}))''',
            {'root':self.cwd,'seed':seed})

    async def wait(self, predicate, deadline, label):
        while True:
            require(time.monotonic() < deadline, label + ' deadline expired')
            value = predicate()
            if value:
                require(time.monotonic() < deadline, label + ' observation arrived late')
                return value
            await asyncio.sleep(min(.05, max(0, deadline-time.monotonic())))

    async def api(self, path, body=None, timeout=60):
        result = await self.http.request('GET' if body is None else 'POST', self.base + path,
                                        json=body, timeout=timeout)
        value = result.json()
        self.report['requests'].append({'path': path, 'method': 'GET' if body is None else 'POST',
            'status': result.status_code, 'response': value})
        self.persist()
        return result.status_code, value

    def payload(self, project, rid, hint):
        encoded = base64.b64encode(self.fixture_bytes).decode()
        bootstrap = ("import base64,os,pathlib,sys;root=pathlib.Path(" + repr(self.state) +
            ");root.mkdir(parents=True,exist_ok=True);p=root/'controlled_acp.py';"
            "tmp=root/('fixture-'+str(os.getpid())+'.tmp');tmp.write_bytes(base64.b64decode(" +
            repr(encoded) + "));os.replace(tmp,p);source=pathlib.Path(" + repr(self.cwd) +
            ");source.mkdir(parents=True,exist_ok=True);f=source/'source.txt';"
            "f.exists()or f.write_text(os.environ['ACP_FIXTURE_INITIAL_NONCE']);"
            "os.execv(sys.executable,[sys.executable,'-u',str(p)])")
        return {'user_id': self.user, 'project_id': project, 'service_type': 'computer-agent-runner',
            'agent_work_dir': self.cwd, 'request_id': rid, 'session_id': hint,
            'prompt': 'Read the controlled source fixture once.',
            'agent_config': {'agent_server': {'agent_id': 'controlled-devcomputer-protocol',
                'command': 'python3', 'args': ['-u', '-c', bootstrap], 'env': {
                    'ACP_FIXTURE_DIR': self.state, 'ACP_FIXTURE_HTTP_CONTRACT': '1',
                    'ACP_FIXTURE_SESSION_PREFIX': self.user + '-sid-',
                    'ACP_FIXTURE_SOURCE_FILE': self.cwd + '/source.txt',
                    'ACP_FIXTURE_INITIAL_NONCE': self.initial_nonce, 'LOG_DIR': '{PREFIX_WORKSPACE_DIR}'}}}}

    async def turn(self, phase, project, hint=None, expected=None, *, ready=None, release=None):
        # B deliberately remains Active while the other project's bounded
        # matrix runs. This is one original held protocol prompt, not repeated
        # keepalive chats, nor an extension of any existing LLM/E2E budget.
        seconds = 300 if release is not None else 60
        deadline = time.monotonic() + seconds
        token = OBSERVATION_DEADLINE.set(deadline)
        try:
            async with asyncio.timeout_at(asyncio.get_running_loop().time() + seconds):
                return await self.turn_until(phase, project, hint, expected, ready=ready,
                                             release=release, deadline=deadline, seconds=seconds)
        finally:
            OBSERVATION_DEADLINE.reset(token)

    async def turn_until(self, phase, project, hint, expected, *, ready, release, deadline, seconds):
        from httpx_sse import aconnect_sse
        rid = self.user + '-' + phase + '-' + uuid.uuid4().hex
        payload = self.payload(project, rid, hint)
        require(rid not in self.report['intents'], 'Original request already submitted')
        self.report['intents'][rid] = {'project_id': project, 'hint': hint,
            'controlled_budget_seconds':seconds,
            'request_sha256': hashlib.sha256(json.dumps(payload, sort_keys=True).encode()).hexdigest()}
        self.persist()  # before the one potentially accepted POST
        status, response = await self.api(self.args.chat_path, payload, timeout=deadline-time.monotonic())
        sid = (response.get('data') or {}).get('session_id')
        require(status == 200 and response.get('success') is True and isinstance(sid, str)
                and sid not in ('', 'pending'), 'DevComputer did not accept a real session: ' + str(response))
        frames, terminal, released = [], None, False
        probe = 'subscriber-' + uuid.uuid4().hex
        url = self.base + self.args.progress_prefix + '/' + urllib.parse.quote(sid, safe='')
        try:
            async with asyncio.timeout_at(asyncio.get_running_loop().time() + max(0, deadline-time.monotonic())):
                async with aconnect_sse(self.http, 'GET', url, headers={'Accept': 'text/event-stream'}) as stream:
                    stream.response.raise_for_status()
                    # A served same-SID connection is legitimately live-only.
                    # This future witness proves real subscription without
                    # requiring replay of a possibly earlier prompt_start.
                    self.file('fixture/'+rid+'.subscriber-probe', probe)
                    async for event in stream.aiter_sse():
                        require(time.monotonic() < deadline, 'Original turn observation arrived late')
                        frame = positive_frame(event, sid)
                        if frame is None:
                            continue
                        require(not frames or frame['seq'] > frames[-1]['seq'], 'SSE sequence is not increasing')
                        frames.append(frame)
                        self.save(phase+'-wire.json', frames)
                        witness = frame['data'].get('data', {}).get('availableCommands', [])
                        if not released and frame['event'] == 'available_commands_update' and any(
                                command.get('name') == probe for command in witness):
                            if ready is not None and not ready.done():
                                owners=self.processes(project)
                                require(len(owners)==1,'Held B has no unique actual owner')
                                ready.set_result((sid,owners[0]))
                            if release is None or release.is_set():
                                self.file('fixture/'+rid+'.release', 'release')
                                released = True
                        if successful_terminal(frame, sid, rid):
                            terminal = frame
                            break
                        require(frame['event'] not in ('error', 'cancelled', 'refusal'), 'Original prompt failed')
        finally:
            self.save(phase+'-wire.json', frames)
        require(released and terminal is not None, 'Original sequenced terminal was not observed')
        text = ''.join(f['data'].get('data', {}).get('content', {}).get('text', '')
                       for f in frames if f['event'] == 'agent_message_chunk')
        require(text == (expected or self.initial_nonce), 'Controlled child did not read the real source nonce')
        await self.wait(lambda: any(r['event'] == 'prompt_end' and r.get('request_id') == rid
                                  for r in self.journal()), deadline, 'Actual ACP prompt completion')
        owners = self.processes(project)
        require(len(owners) == 1, 'Expected exactly one real ACP owner')
        self.report['turns'].append({'phase': phase, 'project_id': project, 'session_id': sid,
            'request_id': rid, 'terminal_seq': terminal['seq'], 'owner': owners[0], 'text': text})
        self.persist()
        return sid, owners[0]

    async def stop(self, project, sid, owner):
        status, value = await self.api('/devcomputer/agent/stop', {'user_id': self.user,
            'project_id': project, 'session_id': sid, 'force': True})
        require(status == 200 and value.get('success') is True
                and (value.get('data') or {}).get('success') is True, 'Original Stop was not acknowledged')
        await self.wait(lambda: self.original_absent(owner), time.monotonic()+10, 'Exact old ACP exit')

    async def prepare(self):
        import httpx
        self.base = os.environ['RCODER_URL'].rstrip('/')
        parsed = urllib.parse.urlsplit(self.base)
        require(parsed.scheme == 'http' and parsed.hostname in ('127.0.0.1', 'localhost'),
                'Use the explicitly isolated loopback controller')
        controller = os.environ['E2E_OWNER_CONTROLLER_ID']
        require(re.fullmatch(r'[0-9a-f]{64}', controller) is not None, 'Exact isolated controller ID required')
        main = self.inspect(controller)
        labels = main['Config'].get('Labels') or {}
        require(labels.get('rcoder.e2e.run') and main['State']['Running'], 'Controller is not an owned E2E fixture')
        ports = main['NetworkSettings'].get('Ports') or {}
        require(any(int(row['HostPort']) == parsed.port for row in ports.get('8090/tcp') or []),
                'RCODER_URL does not point to the captured controller')
        build = json.loads(Path(os.environ['E2E_COMPUTE_BUILD_RECEIPT']).read_text())
        require(main['Image']==build['rcoder']['image_id'], 'Controller differs from the frozen build receipt')
        binary=self.docker('exec',controller,'sha256sum','/app/bin/rcoder').split()[0]
        require(binary==build['rcoder']['binary_sha256'], 'Actual controller binary differs from the build')
        self.report['build_source']=build['source']
        self.report['controller_binary_sha256']=binary
        self.expected_runner_image=build['builder']['image_id']
        self.before_ids = set(self.docker('ps', '-aq', '--no-trunc').split())
        self.before_containers = self.container_snapshot(self.before_ids)
        self.save('preexisting-containers.json', self.before_containers)
        self.initial_nonce = 'DEV_SOURCE_' + uuid.uuid4().hex
        self.http = httpx.AsyncClient(trust_env=False, follow_redirects=False,
            headers=({'X-API-Key':self.secrets[0]}if self.secrets[0]else{}))
        self.report['ensure_intent'] = {'user_id': self.user, 'project_id': self.projects[0]}
        self.persist()
        status, response = await self.api('/computer/pod/ensure', {'user_id':self.user,
            'project_id':self.projects[0], 'service_type':'computer-agent-runner'})
        cid = (response.get('data') or {}).get('container_info', {}).get('container_id')
        require(status in (200, 202) and response.get('success') is True
                and isinstance(cid, str) and re.fullmatch(r'[0-9a-f]{64}',cid), 'Ensure lacks an exact Docker acknowledgement')
        require(cid not in self.before_ids, 'Ensure returned a preexisting container')
        self.cid = cid
        row = self.inspect(cid)
        self.save('captured-runner-raw.json', row)
        self.captured = {'id':cid, 'image':row['Image'], 'name':row['Name'].lstrip('/'),
            'case_id':self.case,'state':row['State'],'service_type':(row['Config'].get('Labels')or{}).get('service-type')}
        require(row['Image']==build['builder']['image_id'], 'Actual Runner differs from the paired build')
        self.fence()
        resources = Path(os.environ['E2E_REPORT_DIR'])/'resources'
        resources.mkdir(exist_ok=True)
        (resources/(self.captured['name']+'-ownership.json')).write_text(json.dumps(self.captured)+'\n')
        self.report['captured'] = self.captured
        self.report['runner_binary_sha256']=self.docker('exec',cid,'sha256sum','/usr/local/bin/agent_runner').split()[0]
        require(self.report['runner_binary_sha256'] == build['builder']['agent_runner_sha256'],
                'Actual Runner binary differs from the frozen paired build')
        self.check('devcomputer frozen controller and owned Runner identity', True,
                   {'controller':controller,'runner':self.captured})

    async def matrix(self):
        a,b,c = self.projects
        sid_a, first = await self.turn('first', a)
        self.check('devcomputer first real SID and sequenced original terminal', True)
        original_git=self.git('KEEP_'+uuid.uuid4().hex)
        self.save('original-git.json',original_git)
        changed = 'CHANGED_SOURCE_' + uuid.uuid4().hex
        b_ready=asyncio.get_running_loop().create_future()
        b_release=asyncio.Event()
        b_turn=asyncio.create_task(self.turn('pending-new',b,'pending',changed,
                                          ready=b_ready,release=b_release))
        self.pending_turns.append(b_turn)
        done,_=await asyncio.wait([b_ready,b_turn],timeout=60,return_when=asyncio.FIRST_COMPLETED)
        require(done,'Held B original request did not establish its real subscriber')
        if b_turn in done:
            await b_turn
            raise AssertionError('Held B ended before its release')
        sid_b,owner_b=b_ready.result()
        self.check('devcomputer shared source projects have independent real owners',
                   sid_a != sid_b and first != owner_b)
        spawn = next(r for r in self.journal()if r['event']=='spawn'and r['project']==a)
        self.check('devcomputer explicit Main entry and default reload propagation',
                   spawn['cwd']==self.cwd and spawn['launch_environment']['LOG_DIR']=='/home/user')
        self.file('source/source.txt', changed)
        require(self.processes(a)==[first], 'Original A owner is not live before default reload')
        again, second = await self.turn('default-reload', a, sid_a, changed)
        self.check('devcomputer default reload reaps original and reads changed source',
                   again==sid_a and second!=first and self.original_absent(first))
        require(self.processes(b)==[owner_b], 'Default reload changed B owner')
        require(self.processes(a)==[second], 'Original A owner is not live before legacy-hint reload')
        legacy, third = await self.turn('pending-existing', a, 'pending', changed)
        require(legacy==sid_a and third!=second and self.original_absent(second), 'Pending hint lost the actual owned session')
        require(self.processes(b)==[owner_b], 'Legacy hint reload changed B owner')
        self.check('devcomputer pending hint never reaches ACP Load',
                   not any(r['event']=='load'and r['params']['sessionId']=='pending'for r in self.journal()))
        require(self.processes(a)==[third], 'Original A owner is not live before controlled Stop')
        await self.stop(a,sid_a,third)
        require(self.processes(b)==[owner_b], 'Stopping A changed B owner')
        recovered, fourth = await self.turn('real-history',a,sid_a,changed)
        recovered_git=self.git()
        self.check('devcomputer real SID Stop Load preserves data and Git',
                   recovered==sid_a and fourth!=third and self.original_absent(third)
                   and self.file('source/source.txt')==changed
                   and recovered_git['commit']==original_git['commit']
                   and recovered_git['blob']==recovered_git['sentinel']==original_git['sentinel']
                   and any(r['event']=='load'and r['params']['sessionId']==sid_a for r in self.journal()))
        self.check('devcomputer reload A preserves B actual owner', self.processes(b)==[owner_b])
        self.file('fixture/'+c+'.new-session-id','pending')
        self.file('fixture/'+c+'.exit-barrier','hold')
        rid=self.user+'-invalid-'+uuid.uuid4().hex
        self.report['intents'][rid]={'project_id':c,'invalid_response_test':True}
        self.persist()
        request=asyncio.create_task(self.api(self.args.chat_path,self.payload(c,rid,None)))
        self.pending_http.append(request)
        await self.wait(lambda:any(r['event']=='exit_started'and r['project']==c for r in self.journal()),
                        time.monotonic()+10,'Invalid SID real exit barrier')
        invalid_owners=self.processes(c)
        require(len(invalid_owners)==1 and not request.done(), 'Invalid SID replied before physical exit')
        self.file('fixture/'+c+'.exit-release','release')
        status,error=await request
        self.check('devcomputer default English invalid ACP cause and real cleanup',
                   error.get('success')is False and 'ACP session response contained a preparation state instead of a session ID'
                   in error.get('message','') and self.original_absent(invalid_owners[0]),
                   {'status':status,'response':error})
        require(self.processes(b)==[owner_b], 'Invalid C attempt changed B owner')
        b_release.set()
        completed_b,final_b=await b_turn
        require(completed_b==sid_b and final_b==owner_b,'Held B was replaced while A changed')
        rows=self.journal()
        accepted={r['request_id']for r in self.report['turns']}
        actual=[r['request_id']for r in rows if r['event']=='prompt']
        self.check('devcomputer each original accepted prompt dispatched exactly once',
                   len(actual)==len(accepted)and set(actual)==accepted and rid not in actual
                   and all(row.get('received_sid') == row['session'] for row in rows if row['event']=='prompt')
                   and not any(row['event']=='prompt_wrong_session' for row in rows))
        self.check('devcomputer preparation state never becomes a public session',
                   all(r['session_id']not in ('','pending')for r in self.report['turns']))
        self.check('devcomputer fixture source stayed unchanged',
                   hashlib.sha256(self.fixture.read_bytes()).hexdigest()==self.fixture_sha
                   and self.remote_python('''import hashlib,json,pathlib,sys
p=json.load(sys.stdin);print(json.dumps(hashlib.sha256(pathlib.Path(p['path']).read_bytes()).hexdigest()))''',
                       {'path':self.script})==self.fixture_sha)
        self.report['success']=True

    async def cleanup(self):
        if self.cid is not None:
            if self.captured is None:
                # The acknowledged create is not replayed. Diagnose its exact
                # identity once; only independently confirmed own resources may
                # be removed, even when the first inspection failed.
                row = self.inspect(self.cid)
                self.save('captured-runner-cleanup-raw.json', row)
                labels = row['Config'].get('Labels') or {}
                require(self.cid not in self.before_ids
                        and labels.get('service-type')=='computer-agent-runner'
                        and labels.get('identifier')==self.user
                        and row['Image']==self.expected_runner_image,
                        'Incomplete capture cannot authorize deletion of an unconfirmed Runner')
                self.captured={'id':self.cid,'image':row['Image'],'name':row['Name'].lstrip('/')}
            self.fence(running=False)
            try:
                self.save('protocol-journal.json',self.journal())
            except Exception as error:
                self.report['success']=False
                self.report['journal_capture_error']=str(error)
            try:
                (self.root/'runner.log').write_text(self.docker('logs',self.cid))
            except Exception as error:
                self.report['success']=False
                self.report['log_capture_error']=str(error)
            self.fence(running=False)
            self.docker('rm','-f',self.cid)  # captured test Runner only; never -v
            self.check('devcomputer captured Runner cleanup retains volumes',
                       self.cid not in set(self.docker('ps','-aq','--no-trunc').split()))
            self.report['cleanup']={'captured_container_removed':True,'volumes_removed':False}
        if hasattr(self,'before_ids'):
            now=set(self.docker('ps','-aq','--no-trunc').split())
            self.check('devcomputer preexisting containers preserved',self.before_ids<=now
                       and self.container_snapshot(self.before_ids)==self.before_containers)
        if hasattr(self,'http'):
            for task in self.pending_turns:
                if not task.done():task.cancel()
            await asyncio.gather(*self.pending_turns,return_exceptions=True)
            for request in self.pending_http:
                if not request.done():request.cancel()
            await asyncio.gather(*self.pending_http,return_exceptions=True)
            await self.http.aclose()


async def execute(args):
    contract=Contract(args)
    try:
        require(sys.version_info >= (3,11), 'Use Python 3.11+ with requirements-devcomputer.txt')
        await contract.prepare()
        await contract.matrix()
    except BaseException as error:
        contract.report['success']=False
        contract.report['error']=type(error).__name__+': '+str(error)
    finally:
        try:
            await contract.cleanup()
        except BaseException as error:
            contract.report['success']=False
            contract.report['cleanup_error']=type(error).__name__+': '+str(error)
        contract.persist()
    require(contract.report['success']and STEPS<={r['name']for r in contract.report['checks']if r['ok']},
            'DevComputer contract failed; preserve original evidence')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--chat-path',required=True,choices=('/computer/chat','/devcomputer/chat'))
    parser.add_argument('--progress-prefix',required=True,choices=('/computer/progress','/devcomputer/progress'))
    args=parser.parse_args()
    require(args.chat_path=='/devcomputer/chat'and args.progress_prefix=='/devcomputer/progress',
            'The formal case must exercise the explicit DevComputer entry')
    asyncio.run(execute(args))


if __name__=='__main__':main()
