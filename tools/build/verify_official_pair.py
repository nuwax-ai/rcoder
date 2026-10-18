#!/usr/bin/env python3
"""Native release gate: real app-cli owner + official Pingap + Python HTTP app.

Runs no AI and publishes nothing. Each invocation owns one fresh workspace;
cleanup uses its app-cli owner channel, never a numeric PID or global kill.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import uuid


BUSINESS_SOURCE = '''import faulthandler, os, sys, time
def phase(name):
 print('PAIR_FIXTURE_PHASE', name, time.monotonic(), flush=True)
phase('python-entered')
print('PAIR_FIXTURE_PYTHON', sys.executable, sys.version, flush=True)
faulthandler.dump_traceback_later(8, repeat=True)
phase('http-import')
import http.server
class Handler(http.server.BaseHTTPRequestHandler):
 def do_GET(self):
  body=b'official-pair-business';self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
phase('http-bind')
server=http.server.HTTPServer(('127.0.0.1',int(os.environ['PORT'])),Handler)
faulthandler.cancel_dump_traceback_later()
phase('http-listening')
server.serve_forever()
'''


def verify_unused_business_port(port=9080):
    """Reuse TIME_WAIT after cleanup while still rejecting a live listener."""
    with socket.socket() as listener:
        if hasattr(socket, 'SO_EXCLUSIVEADDRUSE'):
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        else:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(('127.0.0.1', port))


class NativePair:
    def __init__(self, app, proxy, root):
        self.app, self.proxy, self.root = app.resolve(), proxy.resolve(), root.resolve()
        self.root.mkdir(parents=True, exist_ok=False)
        self.workspace = self.root / 'source'
        self.state, self.logs = self.root / 'state', self.root / 'logs'
        (self.workspace / 'web').mkdir(parents=True)
        self.ports = {9080}
        # Refuse an occupied business port before touching any owner/process.
        verify_unused_business_port()
        self.admin = self.port()
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(('APP_CLI_', 'APP_DEPLOY_', 'RCODER_SUPERVISOR_',
                        'RCODER_COMMAND_', 'RCODER_PINGAP_', 'USERAPP_', 'PINGAP_', 'PG'))
                    and key not in ('PROJECT_ID', 'SERVICE_TYPE', 'RCODER_EXECUTION_DOMAIN',
                                    'RCODER_PLATFORM_BINDING_DIR')}
        self.env.update(APP_CLI_STATE_ROOT=str(self.state), APP_CLI_SPEC_DIR=str(self.root / 'specs'),
                        APP_CLI_PINGAP_RUNTIME_DIR=str(self.root / 'pingap'),
                        APP_CLI_SUPERVISOR_SOCKET=str(self.root / 'absent-supervisor.sock'),
                        APP_CLI_PINGAP_ADMIN_PORT=str(self.port()))
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.owner, self.output = None, None
        self.first_startup_failure = None
        self.result = {'platform': sys.platform, 'passed': False, 'checks': [],
                       'binary_sha256': {name: hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()
                                         for name, path in [('app-cli', self.app), ('pingap', self.proxy)]}}

    def port(self):
        for _ in range(100):
            with socket.socket() as listener:
                listener.bind(('127.0.0.1', 0))
                value = listener.getsockname()[1]
            if value not in self.ports:
                self.ports.add(value)
                return value
        raise RuntimeError('could not reserve distinct loopback port values')

    def check(self, name, value):
        self.result['checks'].append({'name': name, 'passed': bool(value)})
        if not value:
            raise AssertionError(name)

    def run_cli(self, args, label, timeout=120):
        with (self.root / (label + '.log')).open('wb') as stream:
            return subprocess.run([str(self.app), *args], env=self.env, stdout=stream,
                                  stderr=subprocess.STDOUT, timeout=timeout).returncode

    def http(self, port, path, body=None, extra_headers=None, timeout=4):
        headers = {'Content-Type': 'application/json'}
        headers.update(extra_headers or {})
        if port == self.admin:
            token = self.state / 'token'
            if token.is_file():
                headers['X-Deploy-Token'] = token.read_text(encoding='utf-8').strip()
        request = urllib.request.Request(f'http://127.0.0.1:{port}{path}', headers=headers,
            data=None if body is None else json.dumps(body).encode())
        try:
            with self.opener.open(request, timeout=timeout) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            with error:
                return error.code, error.read()

    def data(self, path, body=None):
        status, raw = self.http(self.admin, path, body)
        value = json.loads(raw)
        if status >= 400 or value.get('success') is not True:
            raise RuntimeError(f'{path}: HTTP {status}: {value.get("code", "missing code")}')
        return value['data']

    def wait(self, predicate, budget=90):
        deadline = time.monotonic() + budget
        last = None
        while time.monotonic() < deadline:
            # Read only this fixture's local evidence. Diagnostics must neither
            # wait on another endpoint nor change the original gate verdict.
            try:
                self.observe_startup_failure()
            except Exception as error:
                self.result['diagnostic_error'] = f'first startup snapshot: {error}'
            try:
                last = predicate()
                if last:
                    return last
            except (OSError, ValueError, RuntimeError) as error:
                last = str(error)
            time.sleep(.1)
        raise RuntimeError(f'wait exceeded {budget}s: {last}')

    def operation(self, kind):
        identity = self.data('/v1/runtime/identity')
        status = self.data('/v1/runtime/status')
        request = dict(operation_id=str(uuid.uuid4()), kind=kind,
            expected_runtime_instance_id=identity['runtime_instance_id'],
            expected_revision=status['revision'], workspace_id=identity['workspace_id'])
        request['profile'] = {'profile': 'source', 'input': {'workspace_id': identity['workspace_id']}}
        self.data('/v1/runtime/operations', request)
        def terminal():
            operation = self.data('/v1/runtime/operations/' + request['operation_id'])
            if operation['operation_id'] != request['operation_id']:
                raise AssertionError('operation response belongs to another request')
            return operation if operation['state'] in ('succeeded', 'failed', 'cancelled', 'recovery_required') else None
        operation = self.wait(terminal)
        self.check('operation identity ' + kind, operation['operation_id'] == request['operation_id'])
        self.check(kind + ' authoritative success', operation['state'] == 'succeeded')

    def confirmed_proxy(self):
        proxy = self.data('/v1/proxy/status')
        confirmation = proxy.get('confirmation') or {}
        if (confirmation.get('contract') == 'official_restart_http_v1'
                and confirmation.get('confirmed') is True
                and confirmation.get('phase') == 'serving'):
            return proxy
        return None

    def settled_business(self):
        status = self.data('/v1/runtime/status')
        return (status if status.get('observed') == 'ready'
                and not status.get('active_operation_id')
                and status.get('recovery_protection') is False else None)

    def reload_proxy(self):
        request_id = str(uuid.uuid4())
        headers = {'X-Request-ID': request_id}
        responses = []
        for _ in range(2):
            status, raw = self.http(self.admin, '/v1/proxy/reload', {}, headers, timeout=60)
            envelope = json.loads(raw)
            self.check('reload HTTP success', status == 200 and envelope.get('success') is True)
            data = envelope['data']
            self.check('reload original identity', data.get('operation_id') == request_id)
            self.check('reload authoritative success', data.get('state') == 'succeeded'
                       and (data.get('confirmation') or {}).get('confirmed') is True)
            self.check('reload does not claim private Applied', data.get('verified') is False
                       and data.get('process_instance_id') == '')
            responses.append(data)
        self.check('reload retry replays one publication', responses[1].get('replayed') is True
                   and responses[0]['publication_id'] == responses[1]['publication_id'])
        status, raw = self.http(self.admin, '/v1/proxy/status', extra_headers=headers)
        operation = json.loads(raw).get('data', {}).get('operation') or {}
        self.check('reload terminal query', status == 200 and operation.get('operation_id') == request_id
                   and operation.get('state') == 'succeeded')

    def execute(self):
        (self.workspace / 'web/server.py').write_text(BUSINESS_SOURCE, encoding='utf-8')
        (self.workspace / 'sentinel').write_text('keep source and owner state', encoding='utf-8')
        (self.workspace / 'workspace.manifest.toml').write_text('schema_version=1\n[workspace]\nname="official-pair"\n', encoding='utf-8')
        command = json.dumps([sys.executable, 'server.py'])
        build = json.dumps([sys.executable, '-c', 'pass'])
        (self.workspace / 'web/project.manifest.toml').write_text(
            f'schema_version=1\n[project]\nservice_id="web"\nname="web"\ntype="python"\n'
            f'[build]\ncommand={build}\nartifact="unused.zip"\n[run]\ncommand={command}\n'
            'shutdown_timeout_seconds=3\n[health]\nstartup_path="/health"\nreadiness_path="/health"\n'
            'startup_timeout_seconds=10\n[proxy]\npath="/"\n', encoding='utf-8')
        self.check('configuration generated', self.run_cli(['gen-lock', '--workspace', str(self.workspace)], 'gen-lock') == 0)
        self.output = (self.root / 'owner.log').open('wb')
        self.owner = subprocess.Popen([str(self.app), 'serve', '--workspace', str(self.workspace),
            '--log-dir', str(self.logs), '--admin-addr', f'127.0.0.1:{self.admin}',
            '--pingap-bin', str(self.proxy)], env=self.env, stdout=self.output, stderr=subprocess.STDOUT)
        self.wait(lambda: self.http(9080, '/pair-business') == (200, b'official-pair-business'))
        # A listener may answer before the owner commits the complete graph.
        # Keep the gate on verified publication rather than treating that
        # legitimate startup window as either success or immediate failure.
        proxy = self.wait(self.confirmed_proxy)
        confirmation = proxy.get('confirmation') or {}
        self.check('official HTTP confirmation', confirmation.get('contract') == 'official_restart_http_v1'
                   and confirmation.get('confirmed') is True)
        self.check('official runtime does not claim private Applied', proxy.get('applied') is False)
        self.wait(self.settled_business)
        self.check('owner advertises official runtime contract', 'official-proxy-restart-v1' in self.data('/v1/runtime/identity')['capabilities'])
        self.operation('start')
        self.operation('restart')
        self.wait(lambda: self.http(9080, '/pair-business') == (200, b'official-pair-business'))
        self.wait(self.confirmed_proxy)
        self.reload_proxy()
        self.check('business survives proxy reload', self.http(9080, '/pair-business') == (200, b'official-pair-business'))
        self.operation('stop')
        self.wait(lambda: self.http(9080, '/pair-business')[0] == 503)
        self.check('management remains alive after Stop', self.http(self.admin, '/health')[0] == 200)
        self.operation('start')
        self.wait(lambda: self.http(9080, '/pair-business') == (200, b'official-pair-business'))
        self.check('sentinel preserved', (self.workspace / 'sentinel').read_text(encoding='utf-8') == 'keep source and owner state')
        self.result['passed'] = True

    def owned_business_evidence(self, owner_log):
        """Correlate log discovery with the current owned guardian incarnation.

        Receipt PIDs are diagnostic locators, never process-liveness proof.
        Export only selected fields, not launch tokens or inherited environment.
        """
        state = getattr(self, 'state', self.root / 'state')

        def read_owned(path):
            if not path.resolve().is_relative_to(state.resolve()):
                raise ValueError('diagnostic receipt is outside the fixture state')
            content = path.read_bytes()
            value = json.loads(content)
            if not isinstance(value, dict):
                raise ValueError('diagnostic receipt is not an object')
            return value, hashlib.sha256(content).hexdigest()

        def identity(value):
            if not isinstance(value, str) or str(uuid.UUID(value)) != value:
                raise ValueError('diagnostic incarnation identity is invalid')
            return value

        try:
            if (not state.resolve().is_relative_to(self.root.resolve())
                    or not self.workspace.resolve().is_relative_to(self.root.resolve())):
                raise ValueError('diagnostic scope is outside this fixture')
            supervisor, supervisor_hash = read_owned(state / 'supervisor.json')
            snapshot = supervisor['snapshot']
            binding = snapshot['binding']
            if (binding['component'] != 'app-cli'
                    or not Path(binding['resource']).samefile(self.workspace)):
                raise ValueError('supervisor diagnostic binding differs from fixture workspace')
            supervisor_id = identity(snapshot['supervisor_id'])
            if supervisor['instance'] != supervisor_id:
                raise ValueError('supervisor diagnostic incarnation differs')
            generation_id = identity(snapshot['generation'])
            work = state / 'work' / generation_id
            generation, generation_hash = read_owned(work / 'generation.json')
            if generation['id'] != generation_id or generation['supervisor'] != supervisor_id:
                raise ValueError('generation diagnostic incarnation differs')
            lock = tomllib.loads((self.workspace / 'release.lock.toml').read_text(encoding='utf-8'))
            web = next(service for service in lock['services'] if service['service_id'] == 'web')
            # The log is discovery only. A matching current generation and its
            # unique guardian receipt must independently name this same PID.
            started = list(re.finditer(r'\bstart web \(web\) on :(\d+) \(pid=(\d+)\)', owner_log))
            if not started or int(started[-1][1]) != web['port']:
                raise ValueError('fixture web launch has no matching port witness')
            pid = int(started[-1][2])
            commands = []
            for path in sorted((work / 'guardians').glob('*/receipt.json')):
                receipt, receipt_hash = read_owned(path)
                if receipt.get('diagnostic_pid') != pid:
                    continue
                guardian_id = identity(path.parent.name)
                if (receipt['id'] != guardian_id or receipt['instance_id'] != generation_id
                        or receipt.get('version') not in (1, 2)
                        or receipt.get('phase') not in ('Pending', 'Running', 'Quiescent', 'Revoked')
                        or (receipt.get('root_status') is not None
                            and type(receipt['root_status']) is not int)
                        or not re.fullmatch('[a-f0-9]{64}', receipt['command_digest'])):
                    raise ValueError('guardian diagnostic incarnation differs')
                commands.append({
                    'supervisor_id': supervisor_id, 'generation_id': generation_id,
                    'guardian_id': guardian_id, 'command_digest': receipt['command_digest'],
                    'diagnostic_pid': pid, 'phase': receipt['phase'],
                    'root_status': receipt.get('root_status'), 'receipt_sha256': receipt_hash,
                })
            if not commands:
                raise ValueError('web launch PID has no matching owned guardian receipt')
            _, current_hash = read_owned(state / 'supervisor.json')
            if current_hash != supervisor_hash:
                raise ValueError('supervisor changed while collecting diagnostic evidence')
            return {'binding_verified': True, 'pid_is_liveness_proof': False,
                    'supervisor_sha256': supervisor_hash, 'generation_sha256': generation_hash,
                    'commands': commands}
        except (OSError, ValueError, TypeError, KeyError, StopIteration) as error:
            return {'binding_verified': False, 'pid_is_liveness_proof': False,
                    'commands': [], 'read_error': str(error)}

    def write_diagnostics(self, diagnostic):
        # One complete file is visible to artifact upload and later diagnostics.
        with tempfile.NamedTemporaryFile('w', dir=self.root, encoding='utf-8', delete=False) as stream:
            json.dump(diagnostic, stream, indent=2)
            stream.write('\n')
            temporary = Path(stream.name)
        try:
            temporary.replace(self.root / 'failure-diagnostics.json')
        finally:
            temporary.unlink(missing_ok=True)

    def observe_startup_failure(self):
        if getattr(self, 'first_startup_failure', None) is not None:
            return
        path = self.root / 'owner.log'
        if not path.is_file():
            return
        owner_log = path.read_text(encoding='utf-8', errors='replace')
        for number, line in enumerate(owner_log.splitlines(), 1):
            if not line.startswith('APP-CLI-EVT '):
                continue
            try:
                event = json.loads(line[len('APP-CLI-EVT '):])
            except ValueError:
                continue  # The writer may still be publishing the final line.
            if (isinstance(event, dict) and event.get('event') == 'service_start_fail'
                    and event.get('service') == 'web'):
                first = {'captured_unix_ns': time.time_ns(), 'owner_log_line': number,
                         'event': event, 'business': self.owned_business_evidence(owner_log)}
                self.write_diagnostics({'first_startup_failure': first})
                self.first_startup_failure = first
                return

    def capture_failure_diagnostics(self):
        # Capture this fixture's inputs and owner before shutdown. Do not dump
        # inherited environment, token files, or unrelated system processes.
        diagnostic = {
            'python': {'executable': sys.executable, 'version': sys.version},
            'owner': {'pid': self.owner.pid, 'exit_code': self.owner.poll()}
                     if self.owner is not None else None,
            'inputs': {}, 'observations': {},
        }
        diagnostic['first_startup_failure'] = getattr(self, 'first_startup_failure', None)
        try:
            diagnostic['business'] = self.owned_business_evidence(
                (self.root / 'owner.log').read_text(encoding='utf-8', errors='replace'))
        except OSError as error:
            diagnostic['business'] = {'read_error': str(error)}
        for relative in ('workspace.manifest.toml', 'web/project.manifest.toml',
                         'release.lock.toml', 'web/server.py'):
            path = self.workspace / relative
            try:
                diagnostic['inputs'][relative] = path.read_text(encoding='utf-8')
            except OSError as error:
                diagnostic['inputs'][relative] = {'read_error': str(error)}
        if self.owner is not None and diagnostic['owner']['exit_code'] is None:
            command = (['tasklist', '/FI', f'PID eq {self.owner.pid}', '/FO', 'CSV', '/NH']
                       if sys.platform == 'win32' else
                       ['ps', '-p', str(self.owner.pid), '-o', 'pid=,ppid=,pgid=,state=,comm='])
            try:
                observed = subprocess.run(command, capture_output=True, text=True, timeout=2)
                diagnostic['owner']['os_observation'] = {
                    'argv': command, 'exit_code': observed.returncode,
                    'stdout': observed.stdout, 'stderr': observed.stderr,
                }
            except (OSError, subprocess.TimeoutExpired) as error:
                diagnostic['owner']['observation_error'] = str(error)
        ports = {9080, self.admin}
        try:
            lock = tomllib.loads(diagnostic['inputs']['release.lock.toml'])
            ports.update(service['port'] for service in lock.get('services', []))
        except (TypeError, KeyError, tomllib.TOMLDecodeError) as error:
            diagnostic['lock_observation_error'] = str(error)
        diagnostic['tcp_observations'] = {}
        for port in sorted(ports):
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=.25):
                    diagnostic['tcp_observations'][str(port)] = {'connected': True}
            except OSError as error:
                diagnostic['tcp_observations'][str(port)] = {'connected': False, 'error': str(error)}
        for route in ('/v1/runtime/identity', '/v1/runtime/status', '/v1/proxy/status'):
            try:
                status, raw = self.http(self.admin, route, timeout=2)
                diagnostic['observations'][route] = {'status': status, 'body': json.loads(raw)}
            except (OSError, ValueError) as error:
                diagnostic['observations'][route] = {'read_error': str(error)}
        self.write_diagnostics(diagnostic)

    def close(self):
        if self.owner is not None:
            try:
                self.check('captured owner Shutdown', self.run_cli(
                    ['owner', 'shutdown', '--workspace', str(self.workspace)], 'shutdown', 60) == 0)
                self.owner.wait(timeout=50)
                self.check('owner exited cleanly', self.owner.returncode == 0)
            except Exception as error:
                self.result.update(passed=False, cleanup_error=str(error))
        if self.output is not None:
            self.output.close()
        final_hashes = {name: hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()
                        for name, path in [('app-cli', self.app), ('pingap', self.proxy)]}
        self.result['binary_sha256_after'] = final_hashes
        if final_hashes != self.result['binary_sha256']:
            self.result.update(passed=False, input_changed='A binary changed during native acceptance')
        (self.root / 'receipt.json').write_text(json.dumps(self.result, indent=2) + '\n', encoding='utf-8')
        print(json.dumps(self.result, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app-cli', type=Path, required=True)
    parser.add_argument('--pingap', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    args = parser.parse_args()
    pair = NativePair(args.app_cli, args.pingap, args.root)
    try:
        pair.execute()
    except Exception as error:
        pair.result['error'] = str(error)
        try:
            pair.capture_failure_diagnostics()
        except Exception as diagnostic_error:
            pair.result['diagnostic_error'] = str(diagnostic_error)
    finally:
        pair.close()
    return 0 if pair.result['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
