#!/usr/bin/env python3
"""Real CLI recovery contract, without containers, Node, PG or an LLM.

Uses psutil only for fault injection against the exact worker created by this
case. Production supervisors never use the diagnostic PID as kill authority.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

from native_worker_smoke import alive, poll


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app-cli', type=Path, required=True)
    parser.add_argument('--proxy', type=Path, required=True)
    parser.add_argument('--pingap', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    args = parser.parse_args()
    import psutil  # Explicit prerequisite; no silent skip of suspension on Windows.
    def executable(value):
        resolved = value if value.is_file() else shutil.which(str(value))
        if resolved is None:
            raise RuntimeError(f'executable not found: {value}')
        return Path(resolved).resolve()
    app, proxy, pingap = [executable(p) for p in (args.app_cli, args.proxy, args.pingap)]
    root = args.root.resolve() / ('recovery' + uuid.uuid4().hex[:12])
    workspace = root / 'workspace space'
    (workspace / 'web/dist').mkdir(parents=True)
    (workspace / 'worker').mkdir()
    marker = workspace / 'keep.txt'
    marker.write_text('preserve workspace data', encoding='utf-8')
    (workspace / 'web/dist/index.html').write_text('native recovery contract', encoding='utf-8')
    (workspace / 'worker/main.py').write_text(
        'import os,json,pathlib,time,uuid\n'
        'pathlib.Path("execution.json").write_text(json.dumps({"pid":os.getpid(),"id":uuid.uuid4().hex}))\n'
        'while True: time.sleep(1)\n', encoding='utf-8')
    ports = []
    for _ in range(4):
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            ports.append(sock.getsockname()[1])
    if len(set(ports + [9080, 3018])) != 6:
        raise RuntimeError('port allocation collision; use a new case')
    for port in ports + [9080, 3018]:
        with socket.socket() as sock:
            if os.name != 'nt':
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(('127.0.0.1', port))
    admin, proxy_port, web_port, worker_port = ports
    env = {k: v for k, v in os.environ.items() if not k.startswith(
        ('APP_DEPLOY_', 'APP_CLI_', 'FILE_SERVER_', 'RCODER_SUPERVISOR_', 'RCODER_COMMAND_'))}
    token = secrets.token_hex(24)
    env.update(PROJECT_ID=root.name, APP_CLI_STATE_ROOT=str(root / 'app-state'),
               APP_CLI_SKIP_PG_WAIT='1', APP_CLI_PINGAP_BIN=str(pingap),
               APP_CLI_SUPERVISOR_SOCKET=str(root / 'unused-supervisor.sock'),
               FILE_SERVER_PROXY_HOST='127.0.0.1', FILE_SERVER_PROXY_TOKEN=token,
               FILE_SERVER_PROXY_STATE_DIR=str(root / 'proxy-state'),
               FILE_SERVER_LOG_DIR=str(root / 'file-logs'),
               LOG_BASE_DIR=str(root / 'project-logs'),
               COMPUTER_LOG_DIR=str(root / 'computer-logs'),
               FILE_SERVER_APP_CLI_BIN=str(app),
               FILE_SERVER_APP_CLI_ADMIN_PROBE_ADDR=f'127.0.0.1:{admin}',
               USERAPP_WORKSPACE_DIR=str(workspace), USERAPP_SINGLE_APP_ID=root.name,
               PROJECT_SOURCE_DIR=str(root / 'projects'),
               COMPUTER_WORKSPACE_DIR=str(root / 'computer'))
    result = {'case': str(root), 'platform': sys.platform, 'passed': False,
              'binaries': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in (app, proxy, pingap)},
              'checks': []}
    processes, logs = [], []
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def check(name, condition):
        result['checks'].append({'name': name, 'passed': bool(condition)})
        if not condition:
            raise RuntimeError(name)

    def launch(binary, argv, name):
        log = (root / (name + '.log')).open('wb')
        logs.append(log)
        process = subprocess.Popen([str(binary), *argv], env=env, stdout=log, stderr=log)
        processes.append(process)
        return process

    def json_file(path):
        return json.loads(path.read_text(encoding='utf-8'))

    def http(port, path, body=None):
        headers = {'Content-Type': 'application/json'}
        if port == proxy_port:
            headers['X-Proxy-Token'] = token
        else:
            headers['X-Deploy-Token'] = (root / 'app-state/token').read_text().strip()
        request = urllib.request.Request(f'http://127.0.0.1:{port}' + path,
            data=None if body is None else json.dumps(body).encode(), headers=headers)
        try:
            with opener.open(request, timeout=120) as response:
                value = json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f'HTTP {error.code}: {error.read().decode()}') from error
        if value.get('success') is False:
            raise RuntimeError(f'HTTP application error: {value}')
        return value.get('data', value)

    def available(port, path):
        try:
            return http(port, path)
        except (OSError, RuntimeError):
            return None

    def owner_status():
        value = json_file(root / 'app-state/supervisor.json')['snapshot']
        return value if value['phase'] == 'ready' else None

    def operation(kind):
        identity = http(admin, '/v1/runtime/identity')
        status = http(admin, '/v1/runtime/status')
        operation_id = uuid.uuid4().hex
        http(admin, '/v1/runtime/operations', {
            'operation_id': operation_id, 'expected_runtime_instance_id': identity['runtime_instance_id'],
            'expected_revision': status['revision'], 'workspace_id': identity['workspace_id'],
            'kind': kind, 'profile': {'profile': 'source', 'input': {'workspace_id': identity['workspace_id']}}})
        def terminal():
            value = http(admin, '/v1/runtime/operations/' + operation_id)
            return value if value['state'] not in ('accepted', 'running', 'cancelling') else None
        value = poll(terminal, 60)
        if value['state'] != 'succeeded':
            result['failed_operation'] = {key: value.get(key) for key in
                ('operation_id', 'state', 'error', 'error_code', 'recovery')}
        check(kind + ' succeeded', value['state'] == 'succeeded')
        return value

    def stop_through_file_server():
        return http(proxy_port, '/api/v1/userapp/dev/stop', {'app_id': root.name})

    proxy_args = ['--embed', '--policy', 'all_rust', '--port', str(proxy_port)]
    try:
        owner = launch(app, ['serve', '--workspace', str(workspace), '--log-dir', str(root / 'logs'),
                            '--admin-addr', f'127.0.0.1:{admin}'], 'app')
        poll(lambda: available(admin, '/v1/runtime/recovery'))
        poll(owner_status)
        (workspace / 'release.lock.toml').write_text(f'''schema_version = 1
release_id = "recovery"
workspace_name = "recovery"
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
port = {web_port}
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
port = {worker_port}
logs = []
env = {{}}
[services.run]
command = [{json.dumps(sys.executable)}, "main.py"]
shutdown_timeout_seconds = 2
[services.health]
startup_probe = "process"
startup_timeout_seconds = 20
''', encoding='utf-8')
        operation('start')
        first = json_file(workspace / 'worker/execution.json')
        transport = launch(proxy, proxy_args, 'proxy')
        poll(lambda: available(proxy_port, '/api/computer/fs/roots'))
        stop_through_file_server()  # Creates real durable transport history.
        check('file server stops agent-launched owner', not alive(first['pid']))
        operation('start')
        before = poll(owner_status)
        business = json_file(workspace / 'worker/execution.json')
        execution = json_file(root / 'app-state/work' / before['generation'] / 'generation.json')
        process = psutil.Process(execution['worker_pid'])
        check('suspend only captured CLI worker', Path(process.exe()).resolve() == app)
        process.suspend()
        stop_through_file_server()
        after = poll(owner_status)
        check('hung owner replaced by stopped management', after['generation'] != before['generation'])
        check('old business reaped after forced stop', not alive(business['pid']))
        check('file API remains available', bool(http(proxy_port, '/api/computer/fs/roots')))
        operation('start')
        operation('stop')
        owner.kill()
        owner.wait(timeout=10)
        # Recovery from durable transport history must bootstrap an independent
        # app-cli. That CLI must survive subsequent proxy termination.
        stop_through_file_server()
        recovered = poll(owner_status)
        check('file server bootstraps new supervisor', recovered['supervisor_id'] != before['supervisor_id'])
        operation('start')
        identity = http(admin, '/v1/runtime/identity')
        transport.kill()
        transport.wait(timeout=10)
        check('proxy crash preserves app-cli identity', http(admin, '/v1/runtime/identity') == identity)
        transport = launch(proxy, proxy_args, 'proxy-recovered')
        poll(lambda: available(proxy_port, '/api/computer/fs/roots'))
        completed = subprocess.run([str(proxy), 'stop', '--port', str(proxy_port)], env=env,
                                   capture_output=True, text=True, timeout=70)
        check('proxy clean stop completed', completed.returncode == 0)
        check('proxy stop preserves independently owned business', http(admin, '/v1/runtime/identity') == identity)
        operation('stop')
        check('workspace retained', marker.read_text() == 'preserve workspace data')
        result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
    finally:
        # These control requests address only directories/ports created by us.
        for binary, argv in [(proxy, ['stop', '--port', str(proxy_port)]),
                             (app, ['owner', 'shutdown', '--workspace', str(workspace)])]:
            try:
                cleanup = subprocess.run([str(binary), *argv], env=env, capture_output=True, text=True, timeout=75)
                if cleanup.returncode:
                    result.setdefault('cleanup_errors', []).append(cleanup.stderr[-1500:])
            except (OSError, subprocess.TimeoutExpired) as error:
                # A failed cleanup must remain visible in the report and must
                # not skip cleanup of the other independently owned CLI.
                result.setdefault('cleanup_errors', []).append(str(error))
        for process in processes:
            try:
                process.wait(timeout=40)
            except subprocess.TimeoutExpired:
                result.setdefault('cleanup_errors', []).append('CLI parent required forced exit')
                try:
                    process.kill()
                    process.wait(timeout=10)
                except (OSError, subprocess.TimeoutExpired) as error:
                    result['cleanup_errors'].append(str(error))
        for log in logs:
            log.close()
        if result.get('cleanup_errors'):
            result['passed'] = False
        (root / 'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
        print(json.dumps(result, indent=2))
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
