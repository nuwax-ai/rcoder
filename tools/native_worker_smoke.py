#!/usr/bin/env python3
"""Bounded native process-worker lifecycle; no Docker, database or Node required.

Uses a real static web service and foreground Python worker with the runtime
owner API. Only processes started by this invocation are stopped. Existing
listeners cause a failure, never process-name cleanup.
"""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid


def poll(read, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = read()
        if value:
            return value
        time.sleep(.15)
    raise RuntimeError('native worker observation deadline exceeded')


def alive(pid):
    if os.name == 'nt':
        result = subprocess.run(['tasklist', '/FI', f'PID eq {pid}', '/FO', 'CSV', '/NH'],
                                text=True, capture_output=True, timeout=10, check=True)
        return any(len(row) > 1 and row[1] == str(pid) for row in csv.reader(result.stdout.splitlines()))
    result = subprocess.run(['ps', '-p', str(pid), '-o', 'stat='],
                            text=True, capture_output=True, timeout=5)
    if result.returncode not in (0, 1):
        raise RuntimeError('cannot observe the owned worker PID')
    return bool(result.stdout.strip()) and not result.stdout.strip().startswith('Z')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--pingap', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    args = parser.parse_args()
    binary, pingap = args.binary.resolve(), args.pingap.resolve()
    case = args.root.resolve() / ('worker' + uuid.uuid4().hex[:12])
    workspace = case / 'workspace'
    (workspace / 'web/dist').mkdir(parents=True)
    (workspace / 'worker').mkdir()
    (workspace / 'web/dist/index.html').write_text('<html>native-worker-contract</html>')
    (workspace / 'worker/main.py').write_text('''import json,os,pathlib,time,uuid
if pathlib.Path('exit-next').exists(): raise SystemExit(0)
pathlib.Path('execution.json').write_text(json.dumps({'pid':os.getpid(),'execution':uuid.uuid4().hex}))
while True: time.sleep(1)
''')
    result = {'case': case.name, 'platform': sys.platform, 'passed': False,
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'checks': []}
    owner, identity, base, token = None, None, None, None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def check(name, condition):
        result['checks'].append({'name': name, 'passed': bool(condition)})
        if not condition:
            raise RuntimeError(name)

    def request(path, body=None):
        req = urllib.request.Request(base + path, headers={'X-Deploy-Token': token,
            'Content-Type': 'application/json'},
            data=None if body is None else json.dumps(body).encode())
        with opener.open(req, timeout=4) as response:
            return json.load(response)['data']

    def operation(kind, wanted='succeeded'):
        op = kind + uuid.uuid4().hex
        status = request('/v1/runtime/status')
        request('/v1/runtime/operations', {
            'operation_id': op, 'expected_runtime_instance_id': identity['runtime_instance_id'],
            'expected_revision': status['revision'], 'workspace_id': identity['workspace_id'],
            'kind': kind, 'profile': {'profile': 'source', 'input': {
                'workspace_id': identity['workspace_id']}}})

        def terminal():
            record = request('/v1/runtime/operations/' + op)
            return record if record['state'] in ('succeeded', 'failed', 'cancelled', 'recovery_required') else None

        record = poll(terminal)
        (case / (op + '.json')).write_text(json.dumps(record, indent=2))
        check(kind + ' operation identity', record['operation_id'] == op
              and record['runtime_instance_id'] == identity['runtime_instance_id'])
        check(kind + ' ' + wanted, record['state'] == wanted)
        return record

    def execution():
        record = json.loads((workspace / 'worker/execution.json').read_text())
        check('owned worker is alive', alive(record['pid']))
        return record

    def web_ready():
        with opener.open('http://127.0.0.1:9080/', timeout=4) as response:
            check('static web HTTP', response.status == 200
                  and b'native-worker-contract' in response.read())

    try:
        ports = [3018, 9080]
        for _ in range(2):
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                ports.append(probe.getsockname()[1])
        if len(set(ports)) != len(ports):
            raise RuntimeError('test port allocation collided; retry with a fresh case')
        for port in ports:
            with socket.socket() as probe:
                probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                probe.bind(('127.0.0.1', port))
        environment = {k: v for k, v in os.environ.items()
                       if not k.startswith(('APP_DEPLOY_', 'APP_CLI_'))}
        # A native host may already run supervisord for unrelated programs. Keep
        # this fixture on the builtin engine instead of attaching to that daemon.
        environment.update(PROJECT_ID=case.name, APP_CLI_STATE_ROOT=str(case / 'state'),
                           APP_CLI_SKIP_PG_WAIT='1',
                           APP_CLI_SUPERVISOR_SOCKET=str(case / 'unused-supervisor.sock'))
        with (case / 'owner.log').open('w') as log:
            owner = subprocess.Popen([str(binary), 'serve', '--workspace', str(workspace),
                '--log-dir', str(case / 'logs'), '--admin-addr', '127.0.0.1:0',
                '--pingap-bin', str(pingap)], env=environment, stdout=log, stderr=log)

            def initialized():
                nonlocal base, token
                if owner.poll() is not None:
                    raise RuntimeError('owner exited during initialization')
                try:
                    endpoint = json.loads((case / 'state/endpoint.json').read_text())
                    base = 'http://' + endpoint['address']
                    token = (case / 'state/token').read_text().strip()
                except FileNotFoundError:
                    return None
                try:
                    identity = request('/v1/runtime/identity')
                    request('/v1/runtime/recovery')
                    return identity
                except urllib.error.HTTPError as error:
                    body = json.load(error)
                    if error.code == 503 and body.get('code') == 'ERR_INVALID_STATE':
                        return None
                    raise
                except urllib.error.URLError:
                    return None

            identity = poll(initialized)
            check('startup capability', 'startup-probe-v1' in identity['capabilities'])
            lock = f'''schema_version = 1
release_id = "nativeworker"
workspace_name = "nativeworker"
minimum_app_cli_version = "0.3.9"
runtime_image_digest = ""
[pingap]
mode = "managed"
version = "0.14.3"
commit = "cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8"
[[services]]
service_id = "web"
name = "web"
dir = "web"
type = "static"
kind = "web"
enabled = true
port = {ports[2]}
static_content_dir = "dist"
logs = []
env = {{}}
[services.run]
command = []
[services.health]
[services.proxy]
path = "/"
strip_prefix = true
[[services]]
service_id = "worker"
name = "worker"
dir = "worker"
type = "python"
kind = "worker"
enabled = true
port = {ports[3]}
logs = []
env = {{}}
[services.run]
command = [{json.dumps(sys.executable)}, "main.py"]
shutdown_timeout_seconds = 2
[services.health]
startup_probe = "process"
startup_timeout_seconds = 20
'''
            (workspace / 'release.lock.toml').write_text(lock)
            operation('start')
            first = execution()
            web_ready()
            with socket.socket() as probe:
                probe.settimeout(1)
                check('worker has no listener', probe.connect_ex(('127.0.0.1', ports[3])) != 0)
            operation('stop')
            check('stop reaps worker', poll(lambda: not alive(first['pid'])))
            operation('start')
            second = execution()
            check('start creates new execution', second['execution'] != first['execution'])
            operation('restart')
            third = execution()
            check('restart replaces execution', third['execution'] != second['execution'])
            check('restart reaps old worker', not alive(second['pid']))
            web_ready()
            operation('stop')
            check('final stop reaps worker', poll(lambda: not alive(third['pid'])))
            (workspace / 'worker/exit-next').touch()
            failed = operation('start', 'failed')
            check('early exit diagnostic names worker', 'worker' in failed.get('error_message', ''))
            operation('stop')
            check('stop retains owner identity', request('/v1/runtime/identity')['runtime_instance_id']
                  == identity['runtime_instance_id'] and owner.poll() is None)
    except Exception as error:
        result['error'] = str(error)
    finally:
        if owner and owner.poll() is None:
            if identity:
                try:
                    operation('stop')
                except Exception as error:
                    result['cleanup_error'] = str(error)
            # Windows Popen.terminate uses TerminateProcess (exit 1). Business
            # cleanup is proved by the owner Stop operation above, not this exit.
            owner.terminate()
            try:
                result['owner_exit'] = owner.wait(timeout=30)
            except subprocess.TimeoutExpired:
                owner.kill()
                owner.wait(timeout=5)
                result['cleanup_error'] = 'owner required forced termination'
        result['passed'] = bool(result['checks']) and not any(k.endswith('error') for k in result)
        (case / 'result.json').write_text(json.dumps(result, indent=2))
        print(json.dumps(result, indent=2))
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
