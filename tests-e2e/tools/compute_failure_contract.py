#!/usr/bin/env python3
"""Real HTTP compute failures and original-operation recovery, private Compose.

The proxy injects a failed Docker GET only after reading the same operation's
persisted Stopped phase. Docker stop/confirmation and all successful responses
are real. stop_reply_timeout instead forwards Stop, withholds its completed
response until the unchanged client times out, and reads its private checkpoint
only with the formal offline same-engine observer after stopping the controller.
This does not claim coverage of the Starting storage preflight: that
Docker check uses host paths, and needs separate storage/runtime fixtures.
Requires E2E_COMPUTE_BUILD_RECEIPT with source_identity plus immutable rcoder /
builder images and build-time binary hashes; never rebuilds or updates services.
"""
import json
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from idle_owner_recovery import idle_config
from prod_readiness_contract import source_identity
from turso_runtime_contract import REPO, command, isolated_config, service_config, validate_owned, wait_ready

MARKER = 'owned-compute-stopped-inspect-fault'
STEPS = {
    'compute failure frozen inputs verified',
    'compute failure isolated controller and real builder prepared',
    'compute failure original source serves HTTP',
    'compute failure exact Stopped phase fault observed',
    'compute failure diagnostic belongs to original operation',
    'compute failure stop acknowledged without any start dispatch',
    'compute failure revoked fault resumes original operation',
    'compute failure one builder retains original data mounts',
    'compute failure corrected source rebuild serves HTTP',
    'compute failure source stayed frozen',
    'compute failure owned cleanup retains volumes',
}
STOP_TIMEOUT_STEPS = {
    'compute stop timeout ' + name.removeprefix('compute failure ') for name in STEPS
    if name not in {
        'compute failure exact Stopped phase fault observed',
        'compute failure diagnostic belongs to original operation',
        'compute failure stop acknowledged without any start dispatch',
        'compute failure revoked fault resumes original operation',
    }
} | {
    'compute stop timeout real Stop completed before held reply',
    'compute stop timeout original operation remains Stopping while reply unknown',
    'compute stop timeout expired reply preserves original recovery diagnostic',
    'compute stop timeout unknown period has no Start or Create',
    'compute stop timeout durable checkpoint retains original physical intent',
    'compute stop timeout released reply resumes original operation',
}
FAULT_MODES = {'stopped_read': 'compute_stopped', 'stop_reply_timeout': 'compute_stop_timeout'}
DOCKER_CLIENT_TIMEOUT_SECONDS = 120  # unchanged DockerManager socket client

TASK_STATUSES = frozenset({'pending', 'running', 'completed', 'failed', 'cancelled'})
COMPUTE_STATES = frozenset({'pending', 'running', 'recovery_required', 'succeeded', 'failed', 'superseded'})


class PhaseDeadline:
    def __init__(self, seconds, label):
        self.expires = time.monotonic() + seconds
        self.label = label

    def remaining(self):
        remaining = self.expires - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(self.label + ' exceeded its total deadline')
        return remaining

    def request(self, api, path, body=None):
        result = api(path, body, timeout=self.remaining())
        self.remaining()  # A late successful response cannot complete the phase.
        return result

    def pause(self, seconds):
        time.sleep(min(seconds, self.remaining()))
        self.remaining()


def checked_state(result, field, known, label):
    if not isinstance(result, dict):
        raise ValueError(label + ' response must be an object')
    value = result.get(field)
    if not isinstance(value, str) or value not in known:
        raise ValueError(f'{label} returned unsupported {field}: {value!r}')
    return value


def poll_state(api, path, deadline, *, field, expected, waiting, rejected, known, interval):
    if (waiting & rejected or expected in waiting or expected in rejected
            or waiting | rejected | {expected} != known):
        raise ValueError(deadline.label + ' needs an explicit decision for every known state')
    while True:
        result = deadline.request(api, path)
        value = checked_state(result, field, known, deadline.label)
        if value == expected:
            return result
        if value in rejected:
            raise RuntimeError(f'{deadline.label} ended as {value} before {expected}')
        if value in waiting:
            deadline.pause(interval)
            continue
        raise ValueError(deadline.label + ' has no reviewed decision for ' + value)


def required_app_entry_port(environment):
    raw = environment.get('E2E_COMPUTE_APP_ENTRY_PORT')
    if not isinstance(raw, str) or not re.fullmatch(r'[0-9]+', raw):
        raise ValueError('E2E_COMPUTE_APP_ENTRY_PORT must come from the Rust strict entrypoint')
    port = int(raw)
    if not 1 <= port <= 65535:
        raise ValueError('E2E_COMPUTE_APP_ENTRY_PORT must be a valid TCP port')
    return port


def failure_mode(environment):
    value = environment.get('E2E_COMPUTE_FAILURE_MODE', 'stopped_read')
    if value not in FAULT_MODES:
        raise ValueError('unsupported E2E_COMPUTE_FAILURE_MODE')
    return value


def required_checkpoint_observer(build, mode):
    if mode != 'stop_reply_timeout':
        return None
    value = (build.get('rcoder') or {}).get('observer_sha256')
    if not isinstance(value, str) or not re.fullmatch(r'[0-9a-f]{64}', value):
        raise ValueError('Stop timeout build receipt requires rcoder.observer_sha256')
    return value


def wait_control_json(path, deadline):
    while not path.exists():
        deadline.pause(.05)
    value = json.loads(path.read_text())
    deadline.remaining()
    return value


def compute_write_counts(writes):
    stops, starts = [], []
    for row in writes:
        path = urllib.parse.urlsplit(row['path']).path
        if row['method'] == 'POST' and '/containers/' in path:
            if path.endswith('/stop'):
                stops.append(row)
            if any(path.endswith(suffix) for suffix in ('/start', '/restart', '/containers/create')):
                starts.append(row)
    return len(stops), len(starts)


def validate_timeout_write_window(writes, results, *, old_id, new_id, app, lifecycle):
    """Audit attempts through recovered-source HTTP, not just successful writes.

    Retries are deliberately counted as additional dispatches. There is no
    generic SDK retry allowance and no inferred acknowledgement of a lost Stop.
    """
    if any(not re.fullmatch(r'[0-9a-f]{64}', value) for value in (old_id, new_id)):
        raise ValueError('write audit requires captured full Docker IDs')
    by_id = {}
    for result in results:
        key = result.get('request_id')
        if not key or key in by_id:
            raise ValueError('write audit has duplicate or missing result identity')
        by_id[key] = result
    counts = {'stop': 0, 'create': 0, 'start': 0, 'remove': 0, 'exec_create': 0, 'exec_start': 0}
    execs, attempts = {}, set()
    for row in writes:
        key = row.get('request_id')
        if not key or key in attempts:
            raise ValueError('write audit has duplicate or missing attempt identity')
        attempts.add(key)
        result = by_id.get(key)
        if not result or result.get('outcome') != 'upstream':
            raise ValueError('write audit contains a denied or unknown mutation result')
        method, status = row['method'], result.get('status')
        parts = urllib.parse.urlsplit(row['path']).path.strip('/').split('/')
        if parts and parts[0].startswith('v1.'):
            parts = parts[1:]
        query = row.get('query') or {}
        if method == 'POST' and parts == ['containers', 'create']:
            counts['create'] += 1
            if (status != 201 or old_id == new_id or result.get('created_container_id') != new_id
                    or query.get('name') != ['rcoder-app-builder-' + app]
                    or result.get('creation') != {'rcoder.io/application-id': app,
                        'rcoder.io/lifecycle-id': lifecycle, 'service-type': 'user-app-builder'}):
                raise ValueError('write audit creation is not the captured replacement')
        elif len(parts) == 3 and parts[0] == 'containers' and parts[2] == 'stop' and method == 'POST':
            counts['stop'] += 1
            if parts[1] != old_id or result.get('container_id') != old_id or status not in (204, 304):
                raise ValueError('write audit Stop does not target the original full ID')
        elif len(parts) == 3 and parts[0] == 'containers' and parts[2] == 'start' and method == 'POST':
            counts['start'] += 1
            if (result.get('container_id') != new_id or parts[1] != new_id
                    or status not in (204, 304)):
                raise ValueError('write audit Start does not target the captured recovered full ID')
        elif len(parts) == 2 and parts[0] == 'containers' and method == 'DELETE':
            counts['remove'] += 1
            if (parts[1] != old_id or result.get('container_id') != old_id
                    or query.get('v') != ['false'] or status not in (204, 404) or new_id == old_id):
                raise ValueError('write audit remove fence must retain volumes and target only the old full ID')
        elif len(parts) == 3 and parts[0] == 'containers' and parts[2] == 'exec' and method == 'POST':
            target, execution = result.get('container_id'), result.get('created_exec_id')
            if target not in (old_id, new_id) or status != 201 or not execution or execution in execs:
                raise ValueError('write audit exec does not bind a captured container')
            execs[execution] = target
            counts['exec_create'] += 1
        elif len(parts) == 3 and parts[0] == 'exec' and parts[2] == 'start' and method == 'POST':
            target = execs.pop(parts[1], None)
            if target is None or result.get('container_id') != target or status not in (101, 200):
                raise ValueError('write audit exec start has no exact owned execution receipt')
            counts['exec_start'] += 1
        else:
            raise ValueError('write audit contains an unexpected mutation target or action')
        if counts['stop'] > 1 or any(counts[k] > 1 for k in ('create', 'start', 'remove')):
            raise ValueError('write audit found replayed Stop or duplicate Create/Start/remove dispatch')
    if counts['stop'] != 1 or counts['start'] != 1 or counts['create'] != int(old_id != new_id) or execs:
        raise ValueError('write audit is missing a bounded completed recovery sequence')
    return {'full_window': True, 'counts': counts, 'sdk_retry_allowances': [],
            'old_container_id': old_id, 'new_container_id': new_id,
            'old_cid_remove_fence': counts['remove'] == 1,
            'all_targets_captured': True, 'all_upstream_results_observed': True}


def validate_timeout_checkpoint(snapshot, *, app, lifecycle, operation, request, captured_id):
    """Private offline-reader contract, never a SQLite read of a live Turso DB."""
    if (not isinstance(snapshot, dict) or snapshot.get('version') != 1 or snapshot.get('read_only') is not True
            or snapshot.get('controller_stopped') is not True):
        raise ValueError('checkpoint reader must prove offline read-only observation')
    record = snapshot.get('record') or {}
    expected = {'app_id': app, 'lifecycle_id': lifecycle, 'operation_id': operation,
                'request_id': request, 'scope': 'Dev', 'action': 'restart',
                'state': 'recovery_required', 'stage': 'stopping'}
    if not isinstance(record, dict) or any(record.get(key) != value for key, value in expected.items()):
        raise ValueError('checkpoint reader returned another operation or durable phase')
    checkpoint = record.get('checkpoint')
    if not isinstance(checkpoint, dict):
        raise ValueError('checkpoint reader returned no structured checkpoint')
    context = checkpoint.get('context') or {}
    workload = checkpoint.get('workload') or {}
    lease = record.get('lease') or {}
    if (not all(isinstance(value, dict) for value in (context, workload, lease))
            or any(context.get(key) != value for key, value in {
            'app_id': app, 'lifecycle_id': lifecycle, 'operation_id': operation}.items())
            or workload.get('uid') != captured_id
            or not record.get('executor_id') or context.get('executor_id') != record['executor_id']
            or not record.get('request_fingerprint')
            or context.get('request_fingerprint') != record['request_fingerprint']
            or lease.get('runtime') != 'docker' or lease.get('token') != record['executor_id']
            or type(lease.get('inode')) is not int or lease['inode'] <= 0):
        raise ValueError('checkpoint lost the original captured target, executor or lease')
    if not record.get('error_code') or not record.get('error_message'):
        raise ValueError('checkpoint reader lost the original failure diagnostic')
    return record


def settle_stop_reply(control, action, deadline):
    held = control / 'compute-stop-held.json'
    if not held.exists():
        return None
    release = control / 'compute-stop-release.json'
    if not release.exists():
        publish_fault_config(release, {'action': action})
    return wait_control_json(control / 'compute-stop-settled.json', deadline)


def validate_build(build, source):
    if build.get('version') != 1 or build.get('source') != source:
        raise ValueError('compute build receipt does not match frozen source')
    for role, hashes in (('rcoder', ('binary_sha256',)),
                         ('builder', ('agent_runner_sha256', 'app_cli_sha256'))):
        row = build.get(role) or {}
        if not re.fullmatch(r'sha256:[0-9a-f]{64}', str(row.get('image_id', ''))):
            raise ValueError('compute receipt needs immutable ' + role + ' image')
        for key in hashes:
            if not re.fullmatch(r'[0-9a-f]{64}', str(row.get(key, ''))):
                raise ValueError('compute receipt missing build-time ' + role + ' ' + key)
    return build


def atomic_json(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2))
    temporary.chmod(0o600)
    temporary.replace(path)


def publish_fault_config(path, value):
    """Publish once from this fixture's single writer; never rewrite a target.

    Docker Desktop bind readers can combine stale sizes with rewritten content.
    Separate immutable filenames avoid exposing a second version at that path.
    The existence check assumes one writer; it is not a multi-writer CAS.
    """
    if path.exists():
        raise ValueError('fault configuration was already published')
    descriptor, name = tempfile.mkstemp(prefix=path.name + '.', suffix='.tmp', dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, 'w') as output:
            json.dump(value, output, indent=2)
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def builder_rows(receipt):
    ids = command('docker', 'ps', '-aq', '--no-trunc', '--filter',
                  'label=rcoder.io/application-id=' + receipt['app_id']).split()
    rows = json.loads(command('docker', 'inspect', *ids)) if ids else []
    for row in rows:
        labels = row['Config'].get('Labels') or {}
        source = next((m for m in row['Mounts']
                       if m['Destination'] == '/home/user/' + receipt['app_id']), {})
        if (labels.get('service-type') != 'user-app-builder'
                or not labels.get('rcoder.io/lifecycle-id')
                or (receipt.get('lifecycle_id') and labels['rcoder.io/lifecycle-id'] != receipt['lifecycle_id'])
                or row['Image'] != receipt['builder_image'] or source.get('Type') != 'bind'
                or not Path(source.get('Source', '/')).resolve().is_relative_to(Path(receipt['root']) / 'userapp-workspace')):
            raise ValueError('compute fixture builder ownership changed')
    return rows


def validate_compose_row(row, receipt):
    labels = row['Config'].get('Labels') or {}
    service = labels.get('com.docker.compose.service')
    if (labels.get('com.docker.compose.project') != receipt['project']
            or labels.get('rcoder.e2e.run') != receipt['run_id']
            or labels.get('rcoder.e2e.case') != receipt['case_id']
            or service not in ('rcoder', 'docker-proxy')
            or row.get('Name') != '/' + receipt['project'] + '-' + service + '-1'):
        raise ValueError('compute fixture Compose physical ownership mismatch')


def cleanup(root, run_id, case_id, existing_ids=()):
    root = Path(root).resolve()
    receipt = json.loads((root / 'ownership.json').read_text())
    if (receipt['root'] != str(root) or receipt['run_id'] != run_id or receipt['case_id'] != case_id
            or not re.fullmatch(r'rcoder-cfailure-[0-9a-f]{16}', receipt['project'])
            or not re.fullmatch(r'cf' + re.escape(case_id[:10]) + r'[0-9a-f]{10}', receipt['app_id'])):
        raise ValueError('compute cleanup ownership does not match')
    if not receipt.get('creation_started'):
        return {'ok': True, 'volumes_removed': False, 'captured_containers_removed': True}
    compose = ['docker', 'compose', '-p', receipt['project'], '-f', str(root / 'compose.json')]
    ids = command('docker', 'ps', '-aq', '--no-trunc', '--filter',
                  'label=com.docker.compose.project=' + receipt['project']).split()
    rows = json.loads(command('docker', 'inspect', *ids)) if ids else []
    for row in rows:
        validate_compose_row(row, receipt)
        if row['Id'] in existing_ids:
            raise ValueError('compute cleanup cannot stop a preexisting container')
    # Stop the real executor before administrative fixture cleanup. Docker
    # Stop itself has no delayed write: the timeout variant holds its completed
    # reply. The old variant injects only failed scoped reads.
    command(*compose, 'stop', 'rcoder', timeout=90)
    if receipt.get('fault_mode') == 'stop_reply_timeout':
        # Stop the only executor first. The held Stop already completed at Docker;
        # discard/drain its transport before proxy shutdown or builder cleanup.
        for row in rows:
            if row['Config']['Labels'].get('com.docker.compose.service') == 'rcoder':
                actual = json.loads(command('docker', 'inspect', row['Id']))[0]
                validate_compose_row(actual, receipt)
                if actual['Id'] != row['Id'] or actual['State']['Running']:
                    raise RuntimeError('cannot release a held reply while its original controller runs')
        settle_stop_reply(root / 'control', 'discard', PhaseDeadline(20, 'cleanup held Stop reply'))
    for row in builder_rows(receipt):
        if row['Id'] in existing_ids:
            raise ValueError('compute cleanup cannot remove a preexisting builder')
        command('docker', 'rm', '-f', row['Id'])  # never -v
    command(*compose, 'down', '--remove-orphans', timeout=90)  # retain named volumes
    if builder_rows(receipt):
        raise RuntimeError('compute fixture builder remains after cleanup')
    retained = []
    for name in receipt.get('volume_names', []):
        labels = json.loads(command('docker', 'volume', 'inspect', name))[0].get('Labels') or {}
        if labels.get('rcoder.e2e.run') != run_id or labels.get('rcoder.e2e.case') != case_id:
            raise RuntimeError('retained compute volume ownership changed')
        retained.append(name)
    return {'ok': True, 'volumes_removed': False, 'captured_containers_removed': True,
            'retained_volumes': retained, 'retained_data': str(root)}


def main():
    mode = failure_mode(os.environ)
    app_entry_port = required_app_entry_port(os.environ)  # before Docker or fixture creation
    report_dir = Path(os.environ['E2E_REPORT_DIR']).resolve()
    build_path = Path(os.environ['E2E_COMPUTE_BUILD_RECEIPT'])
    source_root = Path(os.environ.get('E2E_SOURCE_ROOT', REPO)).resolve()
    source = source_identity(source_root)
    build = validate_build(json.loads(build_path.read_text()), source)  # before Docker/directory mutation
    observer_sha256 = required_checkpoint_observer(build, mode)
    reader = source_root / 'tests-e2e/tools/compute_checkpoint_snapshot.py'
    if observer_sha256 and not reader.is_file():
        raise ValueError('Stop timeout requires the frozen formal compute checkpoint helper')
    run_id, case_id = os.environ['E2E_RUN_ID'], os.environ['E2E_CASE_ID']
    root = report_dir / 'compute-failure-contract'
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    app = 'cf' + case_id[:10] + uuid.uuid4().hex[:10]
    project = 'rcoder-cfailure-' + uuid.uuid4().hex[:16]
    workspace = '/home/user/' + app
    receipt = {'root': str(root), 'run_id': run_id, 'case_id': case_id, 'project': project,
               'app_id': app, 'builder_image': build['builder']['image_id'], 'creation_started': False,
               'fault_mode': mode}
    evidence = {'checks': [], 'source': source, 'build': build,
                'scope': ('real_http_stop_reply_timeout_and_original_recovery' if mode == 'stop_reply_timeout'
                          else 'real_http_stopped_failure_and_recovery'),
                'starting_preflight_covered': False, 'private_lease_checkpoint_covered': False,
                'operations': [], 'cleanup': {}}
    evidence['fault_mode'] = mode
    if observer_sha256:
        evidence.update(checkpoint_reader={'path': str(reader), 'sha256': hashlib.sha256(reader.read_bytes()).hexdigest(),
                                           'observer_sha256': observer_sha256, 'required': True},
                        private_lease_checkpoint_covered=False)
    compose = ['docker', 'compose', '-p', project, '-f', str(root / 'compose.json')]
    controller = cid = base = None

    def save():
        atomic_json(root / 'ownership.json', receipt)
        atomic_json(root / 'result.json', evidence)

    def check(name, ok, detail=None):
        if mode == 'stop_reply_timeout':
            name = name.replace('compute failure ', 'compute stop timeout ', 1)
        evidence['checks'].append({'name': name, 'ok': bool(ok), 'detail': detail})
        save()
        print(name, 'PASS' if ok else 'FAIL', flush=True)
        if not ok:
            raise RuntimeError(name)

    def api(path, body=None, timeout=180):
        request = urllib.request.Request(base + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={'Content-Type': 'application/json', 'X-App-Id': app})
        with urllib.request.urlopen(request, timeout=timeout) as response:
            envelope = json.load(response)
            if response.status not in (200, 202) or envelope.get('code') != '0000':
                raise RuntimeError('compute fixture HTTP contract failed at ' + path)
        return envelope['data']

    def execute(*args, timeout=90):
        return command('docker', 'exec', cid, *args, timeout=timeout)

    def write(files):
        script = ('import json,pathlib,sys; root=pathlib.Path(sys.argv[1]); '
                  '[( (root/p).parent.mkdir(parents=True,exist_ok=True), '
                  '(root/p).write_text(v)) for p,v in json.loads(sys.argv[2]).items()]')
        execute('python3', '-c', script, workspace, json.dumps(files))

    def capture():
        nonlocal cid
        rows = builder_rows(receipt)
        if len(rows) != 1 or not rows[0]['State']['Running']:
            raise RuntimeError('expected exactly one running compute fixture builder')
        row = rows[0]
        cid = row['Id']
        receipt.setdefault('captured_builder_ids', [])
        if cid not in receipt['captured_builder_ids']:
            receipt['captured_builder_ids'].append(cid)
        save()
        return {'id': cid, 'mounts': sorted((m['Type'], m['Source'], m['Destination'])
                                          for m in row['Mounts'] if m['Destination'].startswith('/home/user'))}

    def task_start(version, count):
        # Fixture input editing is preparation with the existing command budget.
        # The business phase starts before POST and includes its HTTP evidence.
        write({'web/version.txt': version})
        deadline = PhaseDeadline(180, 'compute fixture real build')
        admitted = deadline.request(api, '/api/v1/userapp/dev/start', {'app_id': app})
        if checked_state(admitted, 'status', TASK_STATUSES, deadline.label) in {'failed', 'cancelled'}:
            raise RuntimeError('compute fixture build admission is terminally unsuccessful')
        task = admitted['task_id']
        poll_state(api, '/api/v1/userapp/tasks/' + task + '?app_id=' + app, deadline,
                   field='status', expected='completed', waiting={'pending', 'running'},
                   rejected={'failed', 'cancelled'},
                   known=TASK_STATUSES, interval=1)
        content = execute('curl', '-fsS', '--max-time', str(min(5, deadline.remaining())),
                          f'http://127.0.0.1:{app_entry_port}/health', timeout=deadline.remaining())
        deadline.remaining()
        actual = int(execute('cat', workspace + '/web/build-count', timeout=deadline.remaining()))
        deadline.remaining()
        return content == version and actual == count, {'task': task, 'build_count': actual, 'content': content}

    try:
        check('compute failure frozen inputs verified', True)
        # The current private config uses the same supported builder/state layout
        # as idle recovery, but both recyclers remain disabled for this scenario.
        config_text = idle_config((source_root / 'docker/config.yml').read_text(),
                                  build['builder']['image_id'], app, workspace)
        config_path = root / 'config.yml'
        config_path.write_text(isolated_config(config_text))
        config_path.chmod(0o600)
        config = service_config(root, build['rcoder']['image_id'], run_id, case_id,
                                '/var/run/docker.sock', config_path)
        service = config['services']['rcoder']
        service['environment'].update(DOCKER_SOCKET_PATH='/proxy/docker.sock',
                                      COMPOSE_PROJECT_NAME=project, DOCKER_NETWORK_BASE_NAME='agent-network')
        service['volumes'] = [m for m in service['volumes'] if m['target'] not in ('/var/run/docker.sock', '/app/data')]
        service['volumes'] += [{'type': 'volume', 'source': 'fault-socket', 'target': '/proxy'},
                               {'type': 'volume', 'source': 'compute-data', 'target': '/app/data'}]
        service['depends_on'] = {'docker-proxy': {'condition': 'service_healthy'}}
        service['networks'] = ['agent-network']
        for mount in service['volumes']:
            if mount['type'] == 'bind' and not mount.get('read_only'):
                Path(mount['source']).mkdir(parents=True, exist_ok=True)
        labels = {'rcoder.e2e.run': run_id, 'rcoder.e2e.case': case_id}
        config['volumes'] = {name: {'labels': labels} for name in ('fault-socket', 'compute-data')}
        receipt['volume_names'] = [project + '_' + name for name in config['volumes']]
        config['networks'] = {'agent-network': {'labels': labels}}
        control = root / 'control'
        control.mkdir(mode=0o700)
        script = root / 'docker_fault_proxy.py'
        script.write_text((source_root / 'tests-e2e/tools/docker_fault_proxy.py').read_text())
        config['services']['docker-proxy'] = {
            'image': build['rcoder']['image_id'], 'pull_policy': 'never',
            'entrypoint': ['python3', '/fault.py'], 'restart': 'no', 'labels': labels,
            'networks': ['agent-network'],
            'environment': {'FAULT_APP_ID': app, 'FAULT_USER_ID': 'compute-e2e',
                            'FAULT_PROJECT': project, 'FAULT_MODE': FAULT_MODES[mode]},
            'volumes': [{'type': 'volume', 'source': 'fault-socket', 'target': '/proxy'},
                        {'type': 'bind', 'source': os.environ.get('DOCKER_SOCKET_PATH', '/var/run/docker.sock'), 'target': '/var/run/docker.sock'},
                        {'type': 'bind', 'source': str(script), 'target': '/fault.py', 'read_only': True},
                        {'type': 'bind', 'source': str(control), 'target': '/control'}],
            'healthcheck': {'test': ['CMD', 'python3', '-c', 'import socket;s=socket.socket(socket.AF_UNIX);s.connect("/proxy/docker.sock");s.close()'],
                            'interval': '1s', 'timeout': '2s', 'retries': 30},
        }
        atomic_json(root / 'compose.json', config)
        if command('docker', 'ps', '-aq', '--filter', 'label=rcoder.io/application-id=' + app):
            raise RuntimeError('compute fixture application already has resources')
        receipt['creation_started'] = True
        save()
        command(*compose, 'up', '-d', '--no-build', '--pull', 'never', timeout=120)
        controller = command(*compose, 'ps', '-q', 'rcoder')
        controller_row = json.loads(command('docker', 'inspect', controller))[0]
        validate_owned(controller_row, receipt)
        receipt['controller_id'] = controller
        receipt['controller_data_mounts'] = [
            {key: mount.get(key) for key in ('Type', 'Name', 'Source', 'Destination', 'RW')}
            for mount in controller_row['Mounts'] if mount['Destination'] == '/app/data']
        if mode == 'stop_reply_timeout' and (len(receipt['controller_data_mounts']) != 1
                or receipt['controller_data_mounts'][0]['Type'] != 'volume'
                or receipt['controller_data_mounts'][0]['Name'] != project + '_compute-data'):
            raise RuntimeError('checkpoint observer requires the exact owned controller data volume')
        save()
        base = 'http://' + command(*compose, 'port', 'rcoder', '8090')
        wait_ready(base)
        evidence['actual_binaries'] = {'rcoder': command('docker', 'exec', controller, 'sha256sum', '/app/bin/rcoder').split()[0]}
        if evidence['actual_binaries']['rcoder'] != build['rcoder']['binary_sha256']:
            raise RuntimeError('actual compute RCoder binary does not match build receipt')
        api('/api/v1/userapp/workspace', {'app_id': app})
        receipt['lifecycle_id'] = api('/api/v1/userapp/' + app + '/lifecycle')['lifecycle_id']
        before = capture()
        for path, key in (('/usr/local/bin/agent_runner', 'agent_runner_sha256'),
                          ('/usr/local/bin/app-cli', 'app_cli_sha256')):
            actual = execute('sha256sum', path).split()[0]
            evidence['actual_binaries'][key] = actual
            if actual != build['builder'][key]:
                raise RuntimeError('actual compute builder binary does not match ' + key)
        check('compute failure isolated controller and real builder prepared', True, {'controller_id': controller, 'builder_id': cid})
        # Real embedded PostgreSQL is a prerequisite, no AI and no bypass.
        pg_deadline = PhaseDeadline(180, 'compute fixture PostgreSQL prerequisite')
        while True:
            pg = subprocess.run(['docker', 'exec', cid, 'sh', '-ec',
                'PGPASSWORD="$POSTGRES_PASSWORD" PGCONNECT_TIMEOUT=2 psql -X -w -h 127.0.0.1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -qAt -c "SELECT 1"'],
                capture_output=True, text=True, timeout=min(10, pg_deadline.remaining()))
            pg_deadline.remaining()
            if pg.returncode == 0 and pg.stdout.strip() == '1':
                break
            pg_deadline.pause(2)
        write({'workspace.manifest.toml': 'schema_version=1\n[workspace]\nname="compute-failure"\n',
               'web/project.manifest.toml': 'schema_version=1\n[project]\nservice_id="web"\nname="Compute fixture"\ntype="python"\n[build]\ncommand=["python3","build.py"]\nartifact="artifact.zip"\n[run]\ncommand=["python3","main.py"]\n[health]\nreadiness_path="/health"\n[proxy]\npath="/"\nstrip_prefix=false\n',
               'web/build.py': 'from pathlib import Path\nimport zipfile\np=Path("build-count")\np.write_text(str((int(p.read_text()) if p.exists() else 0)+1))\nwith zipfile.ZipFile("artifact.zip","w") as z:\n for n in ("main.py","version.txt"): z.write(n)\n',
               'web/main.py': 'import os\nfrom pathlib import Path\nfrom http.server import BaseHTTPRequestHandler,HTTPServer\nclass H(BaseHTTPRequestHandler):\n def do_GET(self):\n  self.send_response(200)\n  self.end_headers()\n  self.wfile.write(Path("version.txt").read_bytes())\nHTTPServer(("0.0.0.0",int(os.environ["PORT"])),H).serve_forever()\n',
               'sentinel': app})
        command('docker', 'exec', '-d', cid, 'sh', '-ec',
                'exec app-cli serve --control-only --workspace "$1" > /home/user/logs/compute-fixture-owner.log 2>&1', '--', workspace)
        ok, detail = task_start('before-compute-failure', 1)
        check('compute failure original source serves HTTP', ok, detail)
        arm = {'container_id': cid, 'lifecycle_id': receipt['lifecycle_id']}
        publish_fault_config(control / 'compute-arm.json', arm)
        write_offset = len((control / 'compute-writes.jsonl').read_text().splitlines())
        body = {'app_id': app, 'app_stage': 'dev', 'service_type': 'userapp',
                'lifecycle_id': receipt['lifecycle_id'], 'request_id': 'cf-' + uuid.uuid4().hex}
        deadline = PhaseDeadline(210 if mode == 'stop_reply_timeout' else 120,
                                 'real Stop reply timeout' if mode == 'stop_reply_timeout' else 'real Stopped fault')
        fault_started_at = time.monotonic()
        original = deadline.request(api, '/computer/pod/restart', body)
        if checked_state(original, 'state', COMPUTE_STATES, deadline.label) in {'failed', 'succeeded', 'superseded'}:
            raise RuntimeError('compute admission ended before the intended ' + mode + ' fault')
        operation_id = original['operation_id']
        if not isinstance(operation_id, str) or not operation_id:
            raise RuntimeError('compute admission has no original operation identity')
        receipt.update(compute_operation_id=operation_id, compute_request_id=body['request_id'])
        evidence['original_compute_identity'] = {'app_id': app, 'lifecycle_id': receipt['lifecycle_id'],
            'operation_id': operation_id, 'request_id': body['request_id'], 'captured_container_id': before['id']}
        save()
        publish_fault_config(control / 'compute-operation.json', {'operation_id': operation_id})
        operation_path = '/computer/pod/operations/' + app + '/' + operation_id
        if mode == 'stopped_read':
            failed = poll_state(api, operation_path, deadline,
                                field='state', expected='recovery_required',
                                waiting={'pending', 'running'},
                                rejected={'failed', 'succeeded', 'superseded'},
                                known=COMPUTE_STATES, interval=.2)
            events = [json.loads(line) for line in (control / 'compute-faults.jsonl').read_text().splitlines()]
            evidence['operations'] += [original, failed]
            check('compute failure exact Stopped phase fault observed', bool(events) and all(
                e['operation']['operation_id'] == operation_id and e['operation']['stage'] == 'stopped'
                and e['container_id'] == before['id'] and e['lifecycle_id'] == receipt['lifecycle_id'] for e in events),
                {'fault_count': len(events), 'physical_status': [e['physical_status'] for e in events]})
            check('compute failure diagnostic belongs to original operation',
                  failed['operation_id'] == operation_id and failed['lifecycle_id'] == receipt['lifecycle_id']
                  and failed['stage'] == 'stopped' and failed.get('error_code') is not None
                  and MARKER in (failed.get('error_message') or ''), failed)
            writes_before = [json.loads(line) for line in (control / 'compute-writes.jsonl').read_text().splitlines()]
            # No successful start dispatch may occur after the durable stop fault.
            pending_writes = writes_before[write_offset:]
            evidence['writes_at_failure'] = pending_writes
            stops = [row for row in pending_writes
                     if row['method'] == 'POST' and row['path'].split('?')[0].endswith('/stop')]
            starts = [row for row in pending_writes if row['method'] == 'POST' and '/containers/' in row['path'] and any(
                row['path'].split('?')[0].endswith(suffix) for suffix in ('/start', '/restart', '/containers/create'))]
            check('compute failure stop acknowledged without any start dispatch',
                  len(stops) == 1 and not starts and not any(row['State']['Running'] for row in builder_rows(receipt)),
                  {'stop_dispatches': len(stops), 'start_dispatches': len(starts)})
            (control / 'compute-arm.json').unlink()  # revoke only the test-side failed read
            # Same request retries are the public original entrypoint; no new intent.
            deadline = PhaseDeadline(180, 'original compute recovery')
            replay = deadline.request(api, '/computer/pod/restart', body)
            if checked_state(replay, 'state', COMPUTE_STATES, deadline.label) in {'failed', 'superseded'}:
                raise RuntimeError('compute recovery admission is terminally unsuccessful')
            if replay.get('operation_id') != operation_id:
                raise RuntimeError('retry allocated a different compute operation')
            restored = poll_state(api, operation_path, deadline,
                                  field='state', expected='succeeded',
                                  waiting={'pending', 'running', 'recovery_required'},
                                  rejected={'failed', 'superseded'},
                                  known=COMPUTE_STATES, interval=.5)
            evidence['operations'] += [replay, restored]
            check('compute failure revoked fault resumes original operation',
                  restored['operation_id'] == operation_id and restored['stage'] == 'completed'
                  and restored['lifecycle_id'] == receipt['lifecycle_id'], restored)
        else:
            held = wait_control_json(control / 'compute-stop-held.json', deadline)
            check('compute stop timeout real Stop completed before held reply',
                  held.get('upstream_completed') is True and held.get('upstream_status') in (204, 304)
                  and held.get('response_released') is False and held.get('container_id') == before['id']
                  and held.get('lifecycle_id') == receipt['lifecycle_id'], held)
            waiting = deadline.request(api, operation_path)
            check('compute stop timeout original operation remains Stopping while reply unknown',
                  waiting.get('operation_id') == operation_id and waiting.get('lifecycle_id') == receipt['lifecycle_id']
                  and waiting.get('state') == 'running' and waiting.get('stage') == 'stopping'
                  and held['operation']['operation_id'] == operation_id, waiting)
            failed = poll_state(api, operation_path, deadline,
                                field='state', expected='recovery_required',
                                waiting={'pending', 'running'}, rejected={'failed', 'succeeded', 'superseded'},
                                known=COMPUTE_STATES, interval=.5)
            elapsed = time.monotonic() - fault_started_at
            evidence['operations'] += [original, waiting, failed]
            evidence['timeout_observation'] = {'elapsed_seconds': elapsed,
                'unchanged_docker_client_timeout_seconds': DOCKER_CLIENT_TIMEOUT_SECONDS,
                'held_real_stop_reply': held}
            check('compute stop timeout expired reply preserves original recovery diagnostic',
                  failed.get('operation_id') == operation_id and failed.get('lifecycle_id') == receipt['lifecycle_id']
                  and failed.get('stage') == 'stopping' and bool(failed.get('error_code'))
                  and bool(re.search(r'(?i)timeout|timed out|deadline', failed.get('error_message') or ''))
                  and elapsed >= DOCKER_CLIENT_TIMEOUT_SECONDS - 10, failed)
            writes = [json.loads(line) for line in (control / 'compute-writes.jsonl').read_text().splitlines()]
            pending_writes = writes[write_offset:]
            stops, starts = compute_write_counts(pending_writes)
            evidence['writes_at_failure'] = pending_writes
            evidence['sdk_stop_dispatches_before_recovery'] = stops
            check('compute stop timeout unknown period has no Start or Create',
                  stops == 1 and starts == 0 and not any(row['State']['Running'] for row in builder_rows(receipt)),
                  {'stop_dispatches': stops, 'start_create_restart_dispatches': starts})
            settled = settle_stop_reply(control, 'continue', PhaseDeadline(20, 'release completed Stop response'))
            if settled.get('response_released') is not True or settled.get('container_id') != before['id']:
                raise RuntimeError('held real Stop response was not explicitly released')
            (control / 'compute-arm.json').unlink()
            # The formal observer holds the original volume's directory lock.
            # Never open live Turso from sqlite3 or add a public diagnostics API.
            command(*compose, 'stop', 'rcoder', timeout=90)
            stopped_controller = json.loads(command('docker', 'inspect', controller))[0]
            validate_owned(stopped_controller, receipt)
            if stopped_controller['State']['Running']:
                raise RuntimeError('checkpoint reader cannot observe a running controller')
            reader_report = root / 'timeout-checkpoint-offline.json'
            if hashlib.sha256(reader.read_bytes()).hexdigest() != evidence['checkpoint_reader']['sha256']:
                raise RuntimeError('checkpoint reader changed during the fixture')
            reader_argv = [sys.executable, str(reader), '--build-receipt', str(build_path),
                '--run-id', run_id, '--case-id', case_id, '--controller', controller,
                '--data-volume', project + '_compute-data',
                '--app-id', app, '--lifecycle-id', receipt['lifecycle_id'],
                '--operation-id', operation_id, '--report', str(reader_report)]
            evidence['checkpoint_reader']['argv'] = reader_argv
            save()
            command(*reader_argv, timeout=90)
            reader_report.chmod(0o600)
            snapshot = json.loads(reader_report.read_text())
            persisted = validate_timeout_checkpoint(snapshot, app=app, lifecycle=receipt['lifecycle_id'],
                operation=operation_id, request=body['request_id'], captured_id=before['id'])
            evidence['private_lease_checkpoint_covered'] = True
            evidence['checkpoint_reader']['report_sha256'] = hashlib.sha256(reader_report.read_bytes()).hexdigest()
            evidence['checkpoint_reader']['checkpoint_sha256'] = hashlib.sha256(
                json.dumps(persisted['checkpoint'], sort_keys=True, separators=(',', ':')).encode()).hexdigest()
            check('compute stop timeout durable checkpoint retains original physical intent', True,
                  {'operation_id': operation_id, 'report': str(reader_report),
                   'checkpoint_sha256': evidence['checkpoint_reader']['checkpoint_sha256']})
            command(*compose, 'start', 'rcoder', timeout=90)
            base = 'http://' + command(*compose, 'port', 'rcoder', '8090')
            wait_ready(base)
            deadline = PhaseDeadline(180, 'original Stop timeout recovery')
            replay = deadline.request(api, '/computer/pod/restart', body)
            if replay.get('operation_id') != operation_id:
                raise RuntimeError('Stop timeout retry allocated another operation')
            restored = poll_state(api, operation_path, deadline,
                                 field='state', expected='succeeded',
                                 waiting={'pending', 'running', 'recovery_required'},
                                 rejected={'failed', 'superseded'}, known=COMPUTE_STATES, interval=.5)
            evidence['operations'] += [replay, restored]
            # Its final assertion also audits every write through the new-source
            # HTTP check below, so a late replay cannot hide behind success.
        after = capture()
        check('compute failure one builder retains original data mounts',
              after['mounts'] == before['mounts'] and execute('cat', workspace + '/sentinel') == app,
              {'before': before, 'after': after})
        ok, detail = task_start('after-compute-recovery', 2)
        check('compute failure corrected source rebuild serves HTTP', ok, detail)
        check('compute failure source stayed frozen', source_identity(source_root) == source)
        if mode == 'stop_reply_timeout':
            writes = [json.loads(line) for line in (control / 'compute-writes.jsonl').read_text().splitlines()]
            results = [json.loads(line) for line in (control / 'compute-write-results.jsonl').read_text().splitlines()]
            evidence['writes_full_window'] = writes[write_offset:]
            evidence['write_results_full_window'] = [result for result in results
                if result['request_id'] in {row['request_id'] for row in evidence['writes_full_window']}]
            save()  # Preserve failing raw attempts/results before validating.
            audit = validate_timeout_write_window(evidence['writes_full_window'], evidence['write_results_full_window'],
                old_id=before['id'], new_id=after['id'], app=app, lifecycle=receipt['lifecycle_id'])
            evidence['full_window_write_audit'] = audit
            check('compute stop timeout released reply resumes original operation',
                  restored.get('operation_id') == operation_id and restored.get('stage') == 'completed'
                  and restored.get('lifecycle_id') == receipt['lifecycle_id'],
                  {'operation': restored, 'write_audit': audit})
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        evidence['error'] = type(error).__name__ + ': ' + str(error)
        # The strict reporter gets only the error class; private diagnostics stay here.
        evidence['checks'].append({'name': 'compute failure execution', 'ok': False, 'detail': type(error).__name__})
        save()
    finally:
        if controller:
            try:
                output = subprocess.run(['docker', 'logs', '--tail', '200', controller],
                                        capture_output=True, text=True, timeout=20)
                path = root / 'controller-stdout.log'
                path.write_text(output.stdout + output.stderr)
                path.chmod(0o600)
                if output.returncode:
                    raise RuntimeError('compute controller diagnostic collection failed')
            except (OSError, subprocess.SubprocessError, RuntimeError) as error:
                evidence['checks'].append({'name': 'compute failure diagnostics collected',
                                           'ok': False, 'detail': type(error).__name__})
        try:
            evidence['cleanup'] = cleanup(root, run_id, case_id)
            check('compute failure owned cleanup retains volumes', evidence['cleanup']['ok']
                  and evidence['cleanup']['volumes_removed'] is False)
        except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
            evidence['checks'].append({'name': 'compute failure owned cleanup retains volumes', 'ok': False, 'detail': type(error).__name__})
        names = {c['name'] for c in evidence['checks'] if c['ok']}
        required = STOP_TIMEOUT_STEPS if mode == 'stop_reply_timeout' else STEPS
        evidence['success'] = required <= names and all(c['ok'] for c in evidence['checks'])
        save()
    return int(not evidence['success'])


if __name__ == '__main__':
    raise SystemExit(main())
