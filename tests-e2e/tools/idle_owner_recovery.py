#!/usr/bin/env python3
"""Real idle reaper -> retained workspace -> UserApp rebuild/control regression.

Starts a private Compose control plane. Only its cleaner uses short deadlines.
No LLM, database edits, receipt deletion, or simulated owner replies.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

from turso_runtime_contract import service_config, validate_owned, wait_ready

REPO = Path(__file__).resolve().parents[2]
IDLE_SECONDS = 60
SCAN_SECONDS = 5

# Runs inside the captured Linux builder. In the unified Session topology the
# generation's worker_pid is the owner itself, not a child of a root guardian.
# Authenticate the control channel and pin the process with a pidfd before any
# signal. Never signal its parent (which can be agent_runner).
OWNER_FAULT_SCRIPT = r'''
import fcntl, http.client, ipaddress, json, os, pathlib, re, select, signal, socket, sys, uuid, xmlrpc.client
workspace, action = pathlib.Path(sys.argv[1]).resolve(), sys.argv[2]
scope, app_id = pathlib.Path(sys.argv[3]).resolve(), sys.argv[4]
assert re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]*', app_id), 'invalid application ID'
assert workspace == pathlib.Path('/home/user')/app_id, 'cross-application source root'
assert scope == workspace/'state'/app_id and scope.is_relative_to(workspace), 'foreign managed state root'
assert action in ('freeze', 'kill'), 'unknown fault injection action'
discovery = json.loads((scope/'supervisor.json').read_text())
snapshot = discovery['snapshot']
binding = {'component': 'app-cli', 'resource': str(workspace)}
assert discovery['version'] == 2 and snapshot['binding'] == binding, 'owner binding differs'
assert discovery['instance'] == snapshot['supervisor_id'], 'discovery instance differs'
assert str(uuid.UUID(snapshot['generation'])) == snapshot['generation'], 'invalid generation identity'
work = (scope/'work'/snapshot['generation']).resolve()
assert work.parent == scope/'work', 'generation scope escapes the managed state root'
generation = json.loads((work/'generation.json').read_text())
assert generation['id'] == work.name and generation['supervisor'] == discovery['instance'], 'generation differs'
assert generation['phase'] == 'Running', 'no running owner generation'
pid = generation['worker_pid']
assert isinstance(pid, int) and pid > 1, 'no managed owner PID'
pidfd = os.pidfd_open(pid)
try:
    proc = pathlib.Path('/proc', str(pid))
    cmd = [c.decode() for c in (proc/'cmdline').read_bytes().split(b'\0') if c]
    assert (proc/'exe').resolve().name == 'app-cli', 'captured PID is not app-cli'
    assert str(workspace) in cmd or str(workspace/'.run') in cmd, 'owner command workspace differs'
    start_ticks = (proc/'stat').read_text().rsplit(')', 1)[1].split()[19]
    for lock in (scope/'owner.lock', work/'generation.lock'):
        identity = lock.stat()
        held = False
        for fd in (proc/'fd').iterdir():
            try:
                value = fd.stat()
                held |= (value.st_dev, value.st_ino) == (identity.st_dev, identity.st_ino)
            except FileNotFoundError:
                pass
        assert held, 'owner does not hold captured lock FD: ' + str(lock)
        with lock.open('rb') as handle:
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise AssertionError('captured owner lock is not held: ' + str(lock))
    host, port = discovery['address'].rsplit(':', 1)
    assert ipaddress.ip_address(host).is_loopback, 'control address is not loopback'
    with socket.create_connection((host, int(port)), timeout=3) as channel:
        frame = {'version': 2, 'instance': discovery['instance'], 'token': discovery['token'],
                 'request': {'request_id': str(uuid.uuid4()), 'action': 'status', 'expected_generation': generation['id']}}
        channel.sendall(json.dumps(frame).encode() + b'\n')
        with channel.makefile('rb') as stream:
            raw = stream.readline(32769)
        assert raw.endswith(b'\n') and len(raw) <= 32768, 'invalid control frame'
        reply = json.loads(raw)
        assert reply['error'] is None and reply['instance'] == discovery['instance'], 'owner control rejected'
        assert reply['snapshot']['binding'] == binding and reply['snapshot']['generation'] == generation['id'], 'live owner differs'
    # Killing an owner must leave a real managed execution range to recover,
    # rather than proving only an empty management process can be restarted.
    commands = []
    for path in (work/'guardians').glob('*/receipt.json'):
        receipt = json.loads(path.read_text())
        if receipt['phase'] != 'Running' or receipt.get('diagnostic_pid') is None:
            continue
        assert receipt['instance_id'] == generation['id'], 'foreign command guardian'
        assert receipt['id'] == path.parent.name, 'guardian directory identity differs'
        # Long-lived builtin services can carry a guardian without a separate
        # command journal. The generation-bound guardian is still real evidence.
        if receipt['command_record'] is not None:
            command_path = pathlib.Path(receipt['command_record'])
            assert command_path.parent == work/'commands', 'foreign command record'
            command = json.loads(command_path.read_text())
            if command['phase'] != 'Running' or command.get('diagnostic_pid') != receipt['diagnostic_pid']:
                continue
        command_proc = pathlib.Path('/proc', str(receipt['diagnostic_pid']))
        if command_proc.exists() and command_proc.joinpath('stat').read_text().rsplit(')', 1)[1].split()[0] != 'Z':
            commands.append({'guardian': path.parent.name, 'command_pid': receipt['diagnostic_pid'],
                             'command_record': receipt['command_record']})
    engine_programs = []
    engine_path = work/'supervisord-engine.json'
    if engine_path.exists():
        engine = json.loads(engine_path.read_text())
        assert engine['generation'] == generation['id'] and engine['supervisor_id'] == discovery['instance'], 'foreign engine receipt'
        assert pathlib.Path(engine['socket']).is_absolute(), 'engine socket is not absolute'
        class UnixConnection(http.client.HTTPConnection):
            def connect(self):
                self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.sock.settimeout(3)
                self.sock.connect(engine['socket'])
        class UnixTransport(xmlrpc.client.Transport):
            def make_connection(self, host):
                return UnixConnection(host)
        with xmlrpc.client.ServerProxy('http://localhost/RPC2', transport=UnixTransport()) as client:
            for program in client.supervisor.getAllProcessInfo():
                if (program['name'].startswith('app-svc-') or program['name'] == 'app-pingap') and program['state'] == 20:
                    assert program['pid'] > 1 and pathlib.Path('/proc', str(program['pid'])).exists(), 'engine reports unavailable process'
                    engine_programs.append({'name': program['name'], 'pid': program['pid']})
    assert commands or engine_programs, 'running owner has no live managed execution range'
    latest = json.loads((scope/'supervisor.json').read_text())
    assert latest['instance'] == discovery['instance'] and latest['snapshot']['generation'] == generation['id'], 'owner changed before injection'
    assert json.loads((work/'generation.json').read_text()) == generation, 'generation changed before injection'
    assert (proc/'stat').read_text().rsplit(')', 1)[1].split()[19] == start_ticks, 'PID changed before injection'
    if action == 'freeze':
        assert generation['physical_domain'] is not None, 'idle proof requires physical domain'
    signal.pidfd_send_signal(pidfd, signal.SIGSTOP if action == 'freeze' else signal.SIGKILL)
    if action == 'kill':
        assert select.select([pidfd], [], [], 10)[0], 'captured owner did not exit'
        assert json.loads((work/'generation.json').read_text())['phase'] in ('Running', 'Draining'), 'fault did not leave an unfinished generation'
    print(json.dumps({'generation': generation['id'], 'owner_pid': pid,
                      'supervisor_id': generation['supervisor'], 'domain': generation['physical_domain'],
                      'binding': binding, 'mode': 'unified_session', 'commands': commands,
                      'engine_programs': engine_programs}))
finally:
    os.close(pidfd)
'''


def command(*args, check=True, timeout=180):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f'{args[0]} {args[1]} failed ({result.returncode}): '
                           f'{result.stderr[-2000:]} {result.stdout[-2000:]}')
    return result


def docker(*args, **kwargs):
    return command('docker', *args, **kwargs)


def inspect(cid):
    # A failed inspect could be a daemon outage, not absence. A successful list
    # distinguishes that from a positively absent captured container.
    ids = docker('ps', '-aq', '--no-trunc', '--filter', 'id=' + cid).stdout.split()
    if not ids:
        return None
    return json.loads(docker('inspect', cid).stdout)[0]


def fixture_layout(app_id, source_root):
    """固定本实验的平台源码根，不接受 code/ 或跨应用路径作为别名。"""
    if not isinstance(app_id, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]*', app_id):
        raise ValueError('idle fixture application ID must be one normal path component')
    expected = '/home/user/' + app_id
    if str(source_root) != expected:
        raise ValueError('idle fixture source root does not match its application')
    root = PurePosixPath(expected)
    return {'source_root': str(root), 'state_root': str(root / 'state' / app_id)}


def retained_state_root(mounts, app_id, source_root, owned_root):
    layout = fixture_layout(app_id, source_root)
    selected = [m for m in mounts if m['destination'] == layout['source_root']]
    if len(selected) != 1 or selected[0]['type'] != 'bind' or selected[0].get('read_only'):
        raise ValueError('managed state needs the exact writable retained source mount')
    physical_root = Path(owned_root).resolve()
    physical_source = Path(selected[0]['source']).resolve()
    if not physical_source.is_relative_to(physical_root) or physical_source == physical_root:
        raise ValueError('source mount is outside this fixture retained workspace')
    physical_scope = physical_source / 'state' / app_id
    if physical_scope.resolve() != physical_scope or not physical_scope.is_relative_to(physical_source):
        raise ValueError('managed state mount path crosses an unexpected symlink')
    return physical_scope


def verify_builder_layout(environment, mounts, app_id, source_root, owned_root):
    layout = fixture_layout(app_id, source_root)
    expected = {'PROJECT_ID': app_id, 'USERAPP_WORKSPACE_DIR': '/home/user',
                'APP_CLI_RUNTIME_WORKSPACE': layout['source_root'],
                'APP_CLI_STATE_ROOT': layout['state_root']}
    for key, value in expected.items():
        actual = [entry.partition('=')[2] for entry in environment
                  if entry.partition('=')[0] == key]
        if actual != [value]:
            raise ValueError(f'builder managed directory declaration differs: {key}')
    retained = retained_state_root(mounts, app_id, source_root, owned_root)
    return dict(layout, retained_state_root=str(retained))


def generation_file(state_root, app_id, source_root, generation, filename):
    layout = fixture_layout(app_id, source_root)
    if str(state_root) != layout['state_root']:
        raise ValueError('generation belongs to another managed state root')
    try:
        canonical = str(uuid.UUID(generation))
    except (ValueError, TypeError, AttributeError) as error:
        raise ValueError('invalid captured generation identity') from error
    if canonical != generation or filename not in ('generation.json', 'physical-exit.json'):
        raise ValueError('invalid captured generation path')
    return str(PurePosixPath(state_root) / 'work' / generation / filename)


def physical_exit_matches(proof, captured, app_id, source_root):
    layout = fixture_layout(app_id, source_root)
    binding = {'component': 'app-cli', 'resource': layout['source_root']}
    return (isinstance(proof, dict) and isinstance(captured, dict)
            and isinstance(captured.get('generation'), str) and bool(captured['generation'])
            and isinstance(captured.get('supervisor_id'), str) and bool(captured['supervisor_id'])
            and isinstance(captured.get('domain'), dict) and bool(captured['domain'])
            and captured.get('binding') == binding
            and proof.get('generation') == captured['generation']
            and proof.get('supervisor_id') == captured['supervisor_id']
            and proof.get('domain') == captured['domain'] and proof.get('binding') == binding)


def idle_config(source, builder_image, app_id, source_root):
    layout = fixture_layout(app_id, source_root)
    cleanup = f'''cleanup_config:
  enabled: true
  idle_timeout_seconds: {IDLE_SECONDS}
  long_idle_timeout_seconds: {IDLE_SECONDS}
  cleanup_interval_seconds: {SCAN_SECONDS}
  container_protection_seconds: 15
  docker_stop_timeout_seconds: 3

'''
    result, count = re.subn(r'(?ms)^cleanup_config:\n.*?(?=^[^ \t#\n]|\Z)', cleanup, source)
    if count != 1 or 'dev-rcoder-agent-runner:latest' not in result:
        raise ValueError('Compose fixture configuration changed; update idle fixture explicitly')
    result = result.replace('dev-rcoder-agent-runner:latest', builder_image)
    # 源码目录本身就是按应用保留的 bind。状态必须置于其原生 state/app 布局；
    # /home/user/logs 中另设 state 根会被现代 managed 身份校验拒绝。
    declarations = ''.join('          ' + key + ': ' + json.dumps(value) + '\n'
                           for key, value in (
                               ('APP_CLI_STATE_ROOT', layout['state_root']),
                               ('APP_CLI_RUNTIME_WORKSPACE', layout['source_root'])))
    section = re.search(r'(?ms)^      user-app-builder:\n.*?(?=^      \S|\Z)', result)
    if section is None or re.search(r'(?m)^          APP_CLI_(STATE_ROOT|RUNTIME_WORKSPACE):',
                                    section.group()):
        raise ValueError('builder fixture already declares managed directory authority')
    result, count = re.subn(r'(?m)^(      user-app-builder:\n(?:.*\n)*?        environment:\n)',
                            lambda match: match.group(1) + declarations, result, count=1)
    if count != 1:
        raise ValueError('expected one UserApp builder environment section')
    return result


def cleanup(root, run_id, case_id, existing_ids=()):
    """Also used by the strict launcher's parent after an interrupted fixture."""
    root = Path(root).resolve()
    receipt = json.loads((root / 'ownership.json').read_text())
    if (receipt.get('run_id') != run_id or receipt.get('case_id') != case_id
            or receipt.get('root') != str(root)
            or not re.fullmatch(r'rcoder-idle-[0-9a-f]{16}', receipt['project'])
            or not re.fullmatch(r'idle[0-9a-f]{16}', receipt['app_id'])):
        raise ValueError('idle fixture cleanup ownership mismatch')
    compose = ['docker', 'compose', '-p', receipt['project'], '-f', str(root / 'compose.json')]
    if not receipt.get('creation_started'):
        return {'ok': True, 'removed': [], 'retained_data': str(root)}
    ids = docker('ps', '-aq', '--no-trunc', '--filter',
                 'label=com.docker.compose.project=' + receipt['project']).stdout.split()
    for target in ids:
        row = json.loads(docker('inspect', target).stdout)[0]
        validate_owned(row, receipt)
        if target in existing_ids:
            raise ValueError('refusing to clean a preexisting controller')
        # Stop the isolated writer before inspecting builder ownership. This is
        # cleanup only, never the action which simulates idle reclamation.
        docker('stop', '-t', '30', target)
    builders = docker('ps', '-aq', '--no-trunc', '--filter',
                      'label=rcoder.io/application-id=' + receipt['app_id'], '--filter',
                      'label=service-type=user-app-builder').stdout.split()
    removed = []
    for target in builders:
        row = json.loads(docker('inspect', target).stdout)[0]
        labels = row['Config'].get('Labels') or {}
        workspace = next((m for m in row['Mounts'] if m['Destination'] == '/home/user/' + receipt['app_id']), {})
        physical_root = root / 'userapp-workspace'
        if (target in existing_ids or row['Image'] != receipt['builder_image']
                or not labels.get('rcoder.io/lifecycle-id')
                or (receipt.get('lifecycle_id') and labels['rcoder.io/lifecycle-id'] != receipt['lifecycle_id'])
                or workspace.get('Type') != 'bind'
                or not Path(workspace.get('Source', '/')).resolve().is_relative_to(physical_root)):
            raise ValueError('cleanup builder physical ownership unconfirmed')
        log = docker('logs', '--tail', '200', target, check=False)
        (root / (target + '-builder.log')).write_text(log.stdout + log.stderr)
        docker('rm', '-f', target)  # no -v; no workspace/PVC deletion
        removed.append(target)
    command(*compose, 'down', '--remove-orphans')
    remaining = docker('ps', '-aq', '--filter', 'label=rcoder.io/application-id=' + receipt['app_id']).stdout.strip()
    if remaining:
        raise RuntimeError('idle fixture builder remains after cleanup')
    return {'ok': True, 'removed': removed, 'retained_data': str(root)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--rcoder-image', default=os.environ.get('E2E_IDLE_RCODER_IMAGE', 'dev-master-rcoder:latest'))
    parser.add_argument('--builder-image', default=os.environ.get('E2E_IDLE_BUILDER_IMAGE', 'dev-rcoder-agent-runner:latest'))
    args = parser.parse_args()
    root = args.report.resolve().parent / 'idle-owner-runtime'
    root.mkdir(parents=True, exist_ok=False)
    app = 'idle' + uuid.uuid4().hex[:16]
    project = 'rcoder-idle-' + uuid.uuid4().hex[:16]
    run_id, case_id = os.environ.get('E2E_RUN_ID', project), os.environ.get('E2E_CASE_ID', app)
    evidence = {'app_id': app, 'checks': [], 'containers': [], 'tasks': [],
                'cleanup': [], 'idle_seconds': IDLE_SECONDS, 'scan_seconds': SCAN_SECONDS,
                'scope': 'isolated_compose_real_idle_reaper', 'retained_data': str(root)}
    receipt = {'project': project, 'run_id': run_id, 'case_id': case_id,
               'root': str(root), 'app_id': app}
    compose = ['docker', 'compose', '-p', project, '-f', str(root / 'compose.json')]
    cid = controller = base = life = None
    private_config = None
    workspace = '/home/user/' + app
    state_root = fixture_layout(app, workspace)['state_root']
    evidence.update(source_root=workspace, state_root=state_root)
    dev_key = 'userapp:' + app

    def save():
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(evidence, ensure_ascii=False, indent=2))
        (root / 'ownership.json').write_text(json.dumps(receipt, indent=2))

    def check(name, ok, detail=None):
        evidence['checks'].append({'name': name, 'ok': bool(ok), 'detail': detail})
        save()
        print(name, 'PASS' if ok else 'FAIL', flush=True)
        if not ok:
            raise RuntimeError(name)

    def http(path, body=None, timeout=150):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(base + path, data=data,
                                         headers={'Content-Type': 'application/json', 'X-App-Id': app})
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                result = json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f'{path}: HTTP {error.code}: {error.read().decode(errors="replace")[-4000:]}') from error
        if not (result.get('success') is True or result.get('code') == '0000'):
            raise RuntimeError(f'{path}: {result}')
        return result

    def execute(*args, check=True):
        return docker('exec', cid, *args, check=check)

    def write(files):
        code = ('import json,pathlib,sys; root=pathlib.Path(sys.argv[1]); '
                '[( (root/p).parent.mkdir(parents=True,exist_ok=True), '
                '(root/p).write_text(text)) for p,text in json.loads(sys.argv[2]).items()]')
        execute('python3', '-c', code, workspace, json.dumps(files))

    def read_json(path):
        return json.loads(execute('cat', path).stdout)

    def owner():
        return json.loads(execute('curl', '-fsS', '--max-time', '5',
                                 'http://127.0.0.1:3010/v1/runtime/identity').stdout)['data']['runtime_instance_id']

    def wait_owner():
        deadline = time.monotonic() + 30
        while True:
            try:
                return owner()
            except (RuntimeError, ValueError, KeyError):
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.5)

    def content():
        result = execute('curl', '-fsS', '--max-time', '2', 'http://127.0.0.1:9080/health', check=False)
        return result.stdout if result.returncode == 0 else None

    def build_count():
        return int(execute('cat', workspace + '/web/build-count').stdout)

    def keepalive():
        result = http('/computer/pod/keepalive', {'app_id': app, 'app_stage': 'dev'})['data']
        if result.get('created') or result['container_info']['container_id'] != cid:
            raise RuntimeError('keepalive unexpectedly replaced the captured container')

    def start(action, version, previous_count):
        keepalive()
        data = http('/api/v1/userapp/dev/' + action, {'app_id': app})['data']
        task = data['task_id']
        if task in [record['id'] for record in evidence['tasks']]:
            raise RuntimeError('new build reused an old task identity')
        row = {'id': task, 'action': action, 'version': version}
        evidence['tasks'].append(row)
        deadline = time.monotonic() + 150
        while True:
            row['result'] = http('/api/v1/userapp/tasks/' + task + '?app_id=' + app)['data']
            state = row['result'].get('status')
            save()
            if state == 'completed':
                break
            if state in ('failed', 'cancelled') or time.monotonic() >= deadline:
                raise RuntimeError(f'{action} task failed/timed out: {row}')
            time.sleep(1)
        keepalive()
        while content() != version and time.monotonic() < deadline:
            time.sleep(0.5)
        count = build_count()
        check(version + ': fresh build and HTTP content', count == previous_count + 1 and content() == version,
              {'build_count': count, 'http': content(), 'task': task})
        return count

    def capture():
        nonlocal cid
        ids = docker('ps', '-aq', '--no-trunc', '--filter', 'label=rcoder.io/application-id=' + app,
                     '--filter', 'label=service-type=user-app-builder').stdout.split()
        if len(ids) != 1:
            raise RuntimeError(f'expected one builder, got {ids}')
        row = json.loads(docker('inspect', ids[0]).stdout)[0]
        labels = row['Config'].get('Labels') or {}
        if labels.get('rcoder.io/lifecycle-id') != life or row['Image'] != evidence['builder_image']:
            raise RuntimeError('builder lifecycle or image identity mismatch')
        cid = row['Id']
        snapshot = {'id': cid, 'image': row['Image'], 'lifecycle': life,
                    'mounts': sorted(({'type': m['Type'], 'source': m['Source'], 'destination': m['Destination'],
                                      'read_only': not m.get('RW', False)}
                                      for m in row['Mounts'] if m['Destination'].startswith('/home/user')),
                                     key=lambda m: m['destination']),
                    'binaries': execute('sha256sum', '/usr/local/bin/agent_runner', '/usr/local/bin/app-cli').stdout}
        layout = verify_builder_layout(row['Config'].get('Env') or [], snapshot['mounts'],
                                       app, workspace, root / 'userapp-workspace')
        snapshot.update(layout)
        if not evidence['containers']:
            check('builder source and state roots match managed scope', True, layout)
        evidence['containers'].append(snapshot)
        save()
        return snapshot

    def control_stop(label):
        result = http('/api/v1/userapp/dev/stop', {'app_id': app})
        check(label, (result.get('data') or {}).get('message') == 'Stopped' and content() is None, result)

    def controller_logs():
        # RCoder file logs are authoritative even when stdout is empty.
        stdout = docker('logs', controller).stdout
        files = '\n'.join(p.read_text(errors='replace') for p in (root / 'logs').glob('*') if p.is_file())
        return stdout + '\n' + files

    def recycle_and_ensure(before, original_owner, cycle):
        nonlocal cid
        old_id = cid
        # No RCoder requests while idle: file/status traffic may touch activity.
        # Capture earlier lines so a later cycle cannot reuse earlier evidence
        # (stdout and file logs may grow independently or rotate).
        previous_lines = set(controller_logs().splitlines())
        deadline = time.monotonic() + IDLE_SECONDS + SCAN_SECONDS * 4 + 90
        while inspect(old_id) is not None and time.monotonic() < deadline:
            time.sleep(2)
        lines = [line for line in controller_logs().splitlines() if line not in previous_lines and app in line and
                 ('idle confirmed (2nd consecutive scan)' in line or 'Starting container destruction' in line)]
        check(cycle + ': real idle reaper removed captured builder', inspect(old_id) is None and len(lines) >= 2, lines)
        cid = None
        log_mount = next(m for m in before['mounts'] if m['destination'] == '/home/user/logs')
        retained = Path(log_mount['source']) / 'dev-server-external.json'
        persisted = json.loads(retained.read_text())
        coordinator = retained_state_root(before['mounts'], app, workspace,
                                           root / 'userapp-workspace') / '.deploy-coordinator.json'
        check(cycle + ': stale owner registration retained', persisted['owners'][dev_key]['owner']['runtime_instance_id'] == original_owner and coordinator.is_file(),
              {'state_sha256': hashlib.sha256(retained.read_bytes()).hexdigest(),
               'coordinator_retained': coordinator.is_file()})
        http('/api/v1/userapp/workspace', {'app_id': app}, timeout=180)
        after = capture()
        check(cycle + ': replacement preserves lifecycle and workspace mounts', after['id'] != old_id and after['mounts'] == before['mounts'] and
              execute('cat', workspace + '/sentinel').stdout == app and
              http('/api/v1/userapp/' + app + '/lifecycle')['data']['lifecycle_id'] == life,
              {'old_id': old_id, 'new_id': cid})
        return after

    try:
        evidence['rcoder_image'] = docker('image', 'inspect', '--format', '{{.Id}}', args.rcoder_image).stdout.strip()
        evidence['builder_image'] = docker('image', 'inspect', '--format', '{{.Id}}', args.builder_image).stdout.strip()
        receipt['builder_image'] = evidence['builder_image']
        private_config = Path(tempfile.mkdtemp(prefix=project + '-')) / 'config.yml'
        private_config.write_text(idle_config((REPO / 'docker/config.yml').read_text(),
                                             evidence['builder_image'], app, workspace))
        private_config.chmod(0o600)
        config = service_config(root, evidence['rcoder_image'], run_id, case_id,
                                os.environ.get('DOCKER_SOCKET_PATH', '/var/run/docker.sock'), private_config)
        config['services']['rcoder']['environment'].pop('RCODER_AUTO_CLEANUP')
        for mount in config['services']['rcoder']['volumes']:
            if not mount.get('read_only') and mount['source'].startswith(str(root) + '/'):
                Path(mount['source']).mkdir(parents=True, exist_ok=True)
        (root / 'compose.json').write_text(json.dumps(config, indent=2))
        # This project, DB, network and all writable roots are separate from the
        # user's regular Compose stack. No configuration reload of that stack.
        receipt['creation_started'] = True
        save()
        command(*compose, 'up', '-d', '--no-build', '--pull', 'never')
        controller = command(*compose, 'ps', '-q', 'rcoder').stdout.strip()
        validate_owned(json.loads(docker('inspect', controller).stdout)[0], receipt)
        evidence['rcoder_binary_sha256'] = docker('exec', controller, 'sha256sum', '/app/bin/rcoder').stdout.split()[0]
        base = 'http://' + command(*compose, 'port', 'rcoder', '8090').stdout.strip()
        wait_ready(base)
        check('isolated idle reaper ready', True, {'controller': controller, 'base': base})
        http('/api/v1/userapp/workspace', {'app_id': app}, timeout=180)
        life = http('/api/v1/userapp/' + app + '/lifecycle')['data']['lifecycle_id']
        receipt['lifecycle_id'] = life
        save()
        before = capture()
        # First initdb on a macOS shared mount can exceed the app-cli PG wait.
        # Prepare the real prerequisite (no bypass/PGDATA edits) before testing
        # owner recovery; keepalive is used only during this setup phase.
        pg_deadline = time.monotonic() + 180
        while True:
            keepalive()
            pg = execute('sh', '-ec',
                         'PGPASSWORD="$POSTGRES_PASSWORD" PGCONNECT_TIMEOUT=2 '
                         'psql -X -w -h 127.0.0.1 -U "$POSTGRES_USER" '
                         '-d "$POSTGRES_DB" -qAt -c "SELECT 1"', check=False)
            if pg.returncode == 0 and pg.stdout.strip() == '1':
                break
            if time.monotonic() >= pg_deadline:
                raise RuntimeError('fixture PostgreSQL initialization/login did not complete')
            time.sleep(2)
        check('real PostgreSQL prerequisite ready', True)
        write({
            'workspace.manifest.toml': 'schema_version=1\n[workspace]\nname="idle-recovery"\n',
            'web/project.manifest.toml': '''schema_version=1
[project]
service_id="web"
name="Idle recovery fixture"
type="python"
[build]
command=["python3","build.py"]
artifact="artifact.zip"
[run]
command=["python3","main.py"]
[health]
readiness_path="/health"
[proxy]
path="/"
strip_prefix=false
''',
            'web/build.py': 'from pathlib import Path\nimport zipfile,time\n'
            'p=Path("build-count")\n'
            f'if not p.exists(): time.sleep({IDLE_SECONDS + SCAN_SECONDS * 3})\n'
            'p.write_text(str((int(p.read_text()) if p.exists() else 0)+1))\n'
            'with zipfile.ZipFile("artifact.zip","w") as z:\n'
            ' for name in ("main.py","version.txt"): z.write(name)\n',
            'web/main.py': 'import os\nfrom pathlib import Path\nfrom http.server import BaseHTTPRequestHandler,HTTPServer\n'
            'class H(BaseHTTPRequestHandler):\n def do_GET(self):\n  self.send_response(200)\n'
            '  self.end_headers()\n  self.wfile.write(Path("version.txt").read_bytes())\n'
            'HTTPServer(("0.0.0.0",int(os.environ["PORT"])),H).serve_forever()\n',
            'web/version.txt': 'before-recycle', 'sentinel': app,
        })
        # Agent/manual CLI starts the management owner; RCoder then adopts it
        # and persists a real registration. A fresh legacy `run` alone does not
        # exercise the incident's external-owner registration recovery path.
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli serve --control-only --workspace "$1" '
               '> /home/user/logs/idle-fixture-owner.log 2>&1', '--', workspace)
        wait_owner()
        count = start('start', 'before-recycle', 0)
        check('running build survives idle scans without keepalive', inspect(cid) is not None
              and cid == before['id'])
        original_owner = owner()
        old_state = read_json('/home/user/logs/dev-server-external.json')
        check('live owner registration persisted', old_state.get('owners', {}).get(dev_key, {}).get('owner', {}).get('runtime_instance_id') == original_owner,
              {'owner': original_owner})
        kernel_before = execute('find', state_root, '-type', 'f').stdout.splitlines()
        check('authoritative owner state resides on retained volume',
              any(path.endswith('/.deploy-coordinator.json') for path in kernel_before), kernel_before)
        evidence['owner_before'] = original_owner
        # Freeze the verified unified owner, not its agent_runner parent.
        # Real idle retirement must then consume physical exit evidence, since
        # this process cannot publish its own graceful cleanup receipt.
        frozen = execute('python3', '-c', OWNER_FAULT_SCRIPT, workspace, 'freeze', state_root, app)
        evidence['frozen_owner'] = json.loads(frozen.stdout)
        after = recycle_and_ensure(before, original_owner, 'first recycle')
        # No preparatory Stop: Start itself must repair the unavailable owner.
        write({'web/version.txt': 'after-recycle'})
        count = start('start', 'after-recycle', count)
        frozen_owner = evidence['frozen_owner']
        retained_generation = read_json(generation_file(state_root, app, workspace,
                                       frozen_owner['generation'], 'generation.json'))
        check('first recycle: retained generation automatically becomes quiescent',
              retained_generation['phase'] == 'Quiescent'
              and retained_generation['id'] == frozen_owner['generation']
              and retained_generation['supervisor'] == frozen_owner['supervisor_id']
              and retained_generation['physical_domain'] == frozen_owner['domain'],
              {'generation': retained_generation['id'], 'phase': retained_generation['phase'],
               'supervisor': retained_generation['supervisor']})
        physical_exit = read_json(generation_file(state_root, app, workspace,
                                 frozen_owner['generation'], 'physical-exit.json'))
        check('first recycle: physical exit matches captured owner',
              physical_exit_matches(physical_exit, frozen_owner, app, workspace), physical_exit)
        evidence['physical_exit'] = physical_exit
        recovered_owner = owner()
        check('new owner replaces retired registration', recovered_owner != original_owner and
              original_owner in json.dumps(read_json('/home/user/logs/dev-server-external.json').get('retired', {})),
              {'old_owner': original_owner, 'new_owner': recovered_owner})
        control_stop('recovered application stops')
        control_stop('independent duplicate stop succeeds')
        write({'web/version.txt': 'after-restart'})
        count = start('restart', 'after-restart', count)
        check('controls retain new owner and physical container', owner() == recovered_owner and inspect(cid) is not None)
        # Kill only the authenticated owner while managed execution is live.
        # Pipe guardians or the recorded external engine must be settled by
        # the next owner without restarting the container.
        same_container = cid
        orphan = execute('python3', '-c', OWNER_FAULT_SCRIPT, workspace, 'kill', state_root, app)
        evidence['same_container_orphan'] = json.loads(orphan.stdout)
        write({'web/version.txt': 'after-orphan'})
        count = start('restart', 'after-orphan', count)
        old_generation = read_json(generation_file(state_root, app, workspace,
                                  evidence['same_container_orphan']['generation'], 'generation.json'))
        check('same-container orphan automatically becomes quiescent', old_generation['phase'] == 'Quiescent'
              and cid == same_container and inspect(cid)['RestartCount'] == 0,
              {'phase': old_generation['phase'], 'container_id': cid,
               'orphan_premise': evidence['same_container_orphan']})
        control_stop('stop after same-container recovery succeeds')
        write({'web/version.txt': 'after-orphan-start'})
        count = start('start', 'after-orphan-start', count)
        recovered_owner = owner()
        after = capture()
        # A second real recycle covers Stop as the first control call against
        # stale state. Otherwise a successful Start would mask Stop's own bug.
        recycle_and_ensure(after, recovered_owner, 'second recycle')
        control_stop('stop after recycle recovers unavailable owner')
        write({'web/version.txt': 'after-second-recycle'})
        start('restart', 'after-second-recycle', count)
        control_stop('final stop keeps workspace data')
        check('workspace marker survives all operations', execute('cat', workspace + '/sentinel').stdout == app)
        evidence['success'] = True
    except (Exception, KeyboardInterrupt) as error:
        evidence.update(success=False, error=str(error))
        print('idle owner recovery FAILED:', error, flush=True)
    finally:
        # Stop the private controller before cleanup so it cannot race a new
        # builder into existence. Preserve database, logs, receipts and workspace.
        try:
            if controller:
                (root / 'controller.log').write_text(controller_logs())
            if cid and inspect(cid) is not None:
                for name in ('pg.out.log', 'pg.err.log'):
                    log = execute('cat', '/app/logs/' + name, check=False)
                    (root / name).write_text(log.stdout + log.stderr)
        except Exception as error:
            evidence.update(diagnostic_error=str(error), success=False)
        try:
            save()
            evidence['cleanup'] = cleanup(root, run_id, case_id)
            evidence['cleanup_ok'] = evidence['cleanup']['ok']
        except Exception as error:
            evidence.update(cleanup_ok=False, cleanup_error=str(error), success=False)
        if private_config:
            private_config.unlink(missing_ok=True)
            private_config.parent.rmdir()
        save()
    return 0 if evidence.get('success') and evidence.get('cleanup_ok') else 1


if __name__ == '__main__':
    raise SystemExit(main())
