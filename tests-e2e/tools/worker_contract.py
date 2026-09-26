#!/usr/bin/env python3
"""Real two-engine worker lifecycle: no listener, early exit, cancel and redeploy.

Reuses the hot-deploy fixture's HTTP/artifact server and durable Docker ownership.
The Rust caller chooses the engine; both source and artifact profiles are exercised.
"""
import hashlib
import http.server
import io
import json
import os
import platform
import threading
import time
import zipfile

os.environ['E2E_CONTRACT_DIR'] = 'worker-contract'
import hot_contract as h


def artifact(release, probe, worker='import time; time.sleep(3600)', dev=False):
    original = zipfile.ZipFile(io.BytesIO(h.artifact(release, release)))
    lock = original.read('release.lock.toml').decode()
    lock = lock.replace('minimum_app_cli_version = "0.1.3"', 'minimum_app_cli_version = "0.3.9"')
    lock = lock.replace('version = "0.14.1"', 'version = "0.14.3"')
    lock = lock.replace('c74e4eaa44e64958cffa18c33e8bbf5995b6844f',
                        'cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8')
    # Web health is explicit too, so supervisord must really check HTTP.
    lock = lock.replace('[services.health]', '[services.health]\nstartup_probe = "http"', 1)
    lock = lock.replace('startup_timeout_seconds = 3', 'startup_timeout_seconds = 20')
    lock += f'''
[[services]]
service_id = "worker"
name = "worker"
dir = "worker"
type = "python"
kind = "worker"
enabled = true
port = 4201
logs = []
env = {{}}
[services.run]
command = ["python3", "main.py"]
shutdown_timeout_seconds = 2
[services.health]
startup_timeout_seconds = 12
'''
    if probe:
        lock += f'startup_probe = "{probe}"\n'
    if dev:
        lock += '[services.devrun]\ncommand = ["python3", "main.py"]\n'
    data = io.BytesIO()
    with zipfile.ZipFile(data, 'w', zipfile.ZIP_DEFLATED) as output:
        for info in original.infolist():
            if info.filename != 'release.lock.toml':
                output.writestr(info, original.read(info))
        output.writestr('release.lock.toml', lock)
        output.writestr('worker/main.py', worker + '\n')
    return data.getvalue()


def view(base, path):
    status, data = h.request_http(base, path)
    if status != 200:
        raise AssertionError(f'{path}: HTTP {status}')
    return json.loads(data)['data']


def main():
    engine = os.environ['E2E_APP_CLI_ENGINE']
    h.PAYLOADS.update({
        '/worker-good': artifact('worker-good', 'process'),
        '/worker-http': artifact('worker-http', 'http'),
        '/worker-exit': artifact('worker-exit', 'process', 'raise SystemExit(0)'),
    })
    server = http.server.ThreadingHTTPServer(('0.0.0.0', 0), h.Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    cid = None
    try:
        entry = (['-e', 'APP_CLI_SKIP_PG_WAIT=1', '--entrypoint', '/usr/local/bin/app-cli',
                  h.RUNTIME_IMAGE, 'serve', '--workspace', '/review/code', '--log-dir', '/review/logs']
                 if engine == 'builtin' else
                 ['-e', 'USERAPP_WORKSPACE_DIR=/review', '-e', 'APP_ID=worker-review', h.RUNTIME_IMAGE])
        host_gateway = ['--add-host=host.docker.internal:host-gateway'] if platform.system() == 'Linux' else []
        cid = h.run_owned(h.REPORT, h.RUN, os.environ['E2E_CASE_ID'], ['-d', *host_gateway,
                         '-p', '127.0.0.1::3010', '-p', '127.0.0.1::9080',
                         '-e', 'APP_CLI_DEPLOY_TOKEN=' + h.TOKEN, *entry], engine, docker_fn=h.docker)
        base = 'http://' + h.docker('port', cid, '3010/tcp')
        app = 'http://' + h.docker('port', cid, '9080/tcp')
        def initialized():
            status, body = h.request_http(base, '/v1/runtime/identity')
            if status != 200:
                return None
            identity = json.loads(body)['data']
            # Identity is intentionally served before startup recovery finishes.
            # This read shares the write-admission initialization gate; never
            # retry a deployment mutation to paper over a premature submission.
            status, body = h.request_http(base, '/v1/runtime/recovery')
            if status == 503 and json.loads(body).get('code') == 'ERR_INVALID_STATE':
                return None
            if status != 200:
                raise AssertionError(f'owner initialization: HTTP {status}: {body.decode()}')
            return identity
        identity = h.poll(initialized)
        h.check('startup probe capability', 'startup-probe-v1' in identity['capabilities'], str(identity))
        physical = json.loads(h.docker('inspect', '--format', '{{json .}}', cid))
        (h.REPORT / 'identity.json').write_text(json.dumps({k: physical[k] for k in ['Id', 'Image', 'Name']}))

        def submit(kind, op, payload=None):
            profile = {'profile': 'source', 'input': {'workspace_id': identity['workspace_id']}}
            if payload:
                profile = {'profile': 'artifact', 'input': {'artifact': {'source': 'url', 'value': {
                    'url': f'http://host.docker.internal:{server.server_port}' + payload,
                    'sha256': hashlib.sha256(h.PAYLOADS[payload]).hexdigest()}}}}
            status, data = h.request_http(base, '/v1/runtime/operations', {
                'operation_id': op, 'expected_runtime_instance_id': identity['runtime_instance_id'],
                'expected_revision': view(base, '/v1/runtime/status')['revision'],
                'workspace_id': identity['workspace_id'], 'kind': kind, 'profile': profile})
            h.check(op + ' accepted', status == 202, data.decode())

        def terminal(op, wanted):
            def read():
                record = view(base, '/v1/runtime/operations/' + op)
                return record if record['state'] in ['succeeded', 'failed', 'cancelled', 'recovery_required'] else None
            record = h.poll(read, 90)
            (h.REPORT / (op + '.json')).write_text(json.dumps(record, indent=2))
            h.check(op + ' terminal', record['operation_id'] == op and record['state'] == wanted, str(record))
            return record

        def worker_roots():
            # Read actual processes, not a cached status. Parent shell is absent.
            code = '''import json,pathlib
roots=[]
for p in pathlib.Path('/proc').iterdir():
 if p.name.isdigit():
  try:
   argv=(p/'cmdline').read_bytes().split(b'\\0')
   if len(argv)>1 and argv[0].endswith(b'python3') and argv[1]==b'main.py': roots.append(int(p.name))
  except (OSError,ProcessLookupError): pass
print(json.dumps(roots))'''
            return json.loads(h.docker('exec', cid, 'python3', '-c', code))

        def healthy():
            h.check('one live worker root', len(worker_roots()) == 1, str(worker_roots()))
            code = 'import socket; s=socket.socket(); s.settimeout(1); print(s.connect_ex(("127.0.0.1",4201))); s.close()'
            h.check('worker has no listener', h.docker('exec', cid, 'python3', '-c', code) != '0', 'worker port 4201')
            h.check('web actual HTTP', h.poll(lambda: h.request_http(app, '/')[0] == 200), 'Pingap HTTP')

        # A real HTTP probe must reject this no-listener process on both engines.
        submit('deploy', 'http-no-listener', '/worker-http')
        failed = terminal('http-no-listener', 'failed')
        h.check('failed worker diagnostic', 'worker' in failed.get('error_message', ''), str(failed))
        h.check('failed worker cleaned', h.poll(lambda: not worker_roots()), 'no worker after failed startup')
        submit('deploy', 'process-artifact', '/worker-good')
        terminal('process-artifact', 'succeeded')
        healthy()
        submit('stop', 'stop-worker')
        terminal('stop-worker', 'succeeded')
        h.check('stop cleans worker', not worker_roots(), 'actual process inventory')
        # The source profile selects devrun; it must honor process instead of legacy TCP.
        source_data = artifact('worker-source', 'process', dev=True)
        import base64
        # Keep the deployed web artifact (including its real symlink) intact.
        # Python ZipFile.extractall would turn the symlink into an ordinary JS file.
        unpack = '''import base64,io,pathlib,sys,zipfile
with zipfile.ZipFile(io.BytesIO(base64.b64decode(sys.argv[1]))) as archive:
 for name in ('release.lock.toml', 'worker/main.py'):
  pathlib.Path('/review/code',name).write_bytes(archive.read(name))'''
        h.docker('exec', cid, 'python3', '-c', unpack, base64.b64encode(source_data).decode())
        submit('start', 'process-source')
        terminal('process-source', 'succeeded')
        healthy()
        previous = worker_roots()
        submit('restart', 'restart-worker')
        terminal('restart-worker', 'succeeded')
        healthy()
        h.check('restart replaces worker', worker_roots() != previous, str(previous))
        submit('deploy', 'early-exit', '/worker-exit')
        terminal('early-exit', 'failed')
        h.check('exit zero is failure', not worker_roots(), 'normal early exit cannot complete startup')
        # During a fresh startup, Stop must interrupt the observation and clean the tree.
        submit('deploy', 'cancel-start', '/worker-good')
        h.poll(lambda: bool(worker_roots()))
        started = time.monotonic()
        submit('stop', 'interrupt-worker')
        terminal('interrupt-worker', 'succeeded')
        cancelled = view(base, '/v1/runtime/operations/cancel-start')
        h.check('stop interrupts startup', cancelled['state'] != 'succeeded' and not worker_roots() and time.monotonic() - started < 15, str(cancelled))
        h.check('owner and container retained', identity['runtime_instance_id'] == view(base, '/v1/runtime/identity')['runtime_instance_id'] and h.docker('inspect', '--format', '{{.Id}}', cid) == cid, cid)
    finally:
        result = h.cleanup_owned(h.REPORT, h.RUN, os.environ['E2E_CASE_ID'], docker_fn=h.docker, secrets=(h.TOKEN,))
        h.RESULTS.append({'name': 'owned resource cleanup', 'ok': result['ok'], 'detail': json.dumps(result)})
        (h.REPORT / 'assertions.json').write_text(json.dumps(h.RESULTS, indent=2))
        server.shutdown()


if __name__ == '__main__':
    main()
