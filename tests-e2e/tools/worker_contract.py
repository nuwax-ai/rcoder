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
import urllib.parse
import zipfile

os.environ['E2E_CONTRACT_DIR'] = 'worker-contract'
import hot_contract as h


def artifact(release, probe, worker='import time; time.sleep(3600)'):
    original = zipfile.ZipFile(io.BytesIO(h.artifact(release, release)))
    lock = original.read('release.lock.toml').decode()
    lock = lock.replace('minimum_app_cli_version = "0.1.3"', 'minimum_app_cli_version = "0.3.9"')
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


def submit_operation(base, request, deadline):
    """Retry only a live zero-write startup rejection, using one original budget."""
    # Freeze once. A retry must never refresh the revision, owner, operation ID
    # or profile from a newer status observation.
    frozen = json.loads(json.dumps(request))
    operation = frozen['operation_id']
    operation_path = '/v1/runtime/operations/' + urllib.parse.quote(operation, safe='')
    owner = frozen['expected_runtime_instance_id']
    request_digest = hashlib.sha256(json.dumps(frozen).encode()).hexdigest()

    def remaining():
        seconds = deadline - time.monotonic()
        if seconds <= 0:
            raise TimeoutError(f'original worker admission deadline expired: {operation}')
        return min(h.REQUEST_TIMEOUT_SECONDS, seconds)

    def observe(path, body=None):
        status, raw = h.request_http(base, path, body, timeout=remaining())
        if body is None:
            h.record_readonly_response(operation, path, status, raw)
        else:
            text = raw.decode('utf-8', errors='replace').replace(h.TOKEN, '<redacted>')
            try:
                response = json.loads(text)
            except ValueError:
                response = {'raw_body': text}
            with (h.REPORT / 'submission-responses.jsonl').open('a') as stream:
                stream.write(json.dumps({'operation_id': operation, 'path': path,
                    'status': status, 'request_sha256': request_digest,
                    'response': response}) + '\n')
        remaining()  # A late reply cannot reset the deadline or count as success.
        return status, raw

    def pause():
        time.sleep(min(.2, remaining()))

    def not_found():
        status, raw = observe(operation_path)
        if status != 404:
            # An existing record may be an earlier accepted write. This fresh
            # worker scenario must stop, rather than resubmit or adopt it.
            raise AssertionError(f'original worker operation is not proven absent: HTTP {status}: '
                                 + raw.decode(errors='replace'))

    status, raw = observe('/v1/runtime/operations', frozen)
    while True:
        try:
            envelope = json.loads(raw)
        except (ValueError, UnicodeDecodeError):
            envelope = None
        rejected = (status == 409 and isinstance(envelope, dict)
                    and envelope.get('success') is False
                    and envelope.get('code') == 'ERR_INITIALIZING'
                    and envelope.get('data') is None
                    and envelope.get('operation_id') is None)
        if not rejected:
            if status == 202:
                accepted = envelope.get('data') if isinstance(envelope, dict) else None
                if (not isinstance(accepted, dict) or envelope.get('success') is not True
                        or accepted.get('operation_id') != operation
                        or accepted.get('poll') != operation_path):
                    raise AssertionError('worker admission returned an ambiguous operation receipt')
            return status, raw
        while True:
            not_found()
            recovery_status, recovery_body = observe('/v1/runtime/recovery')
            if h.initializing_response(recovery_status, recovery_body):
                pause()
                continue
            recovery = json.loads(recovery_body)
            if (recovery_status != 200 or not isinstance(recovery, dict)
                    or recovery.get('success') is not True
                    or not isinstance(recovery.get('data'), dict)
                    or recovery['data'].get('runtime_instance_id') != owner):
                raise AssertionError('original worker owner changed or recovery observation failed')
            # Ready can race another same-ID dispatch. A second exact-ID 404
            # must follow the same-owner observation before identical resubmit.
            not_found()
            break
        pause()
        status, raw = observe('/v1/runtime/operations', frozen)


def source_files():
    """Source Start reloads current manifests, rather than a deployed lock."""
    return {
        'workspace.manifest.toml': '''schema_version = 1
[workspace]
name = "worker-source"
''',
        'web/project.manifest.toml': '''schema_version = 1
[project]
service_id = "web"
name = "Web"
type = "node"
[build]
command = ["node", "--check", "server.js"]
artifact = "artifact.zip"
[run]
command = ["node", "server.js"]
shutdown_timeout_seconds = 3
[devrun]
command = ["node", "server.js"]
[health]
startup_probe = "http"
startup_timeout_seconds = 20
[proxy]
path = "/"
''',
        'worker/project.manifest.toml': '''schema_version = 1
[project]
service_id = "worker"
name = "Worker"
type = "python"
kind = "worker"
[build]
command = ["python3", "-m", "py_compile", "main.py"]
artifact = "artifact.zip"
[run]
command = ["python3", "main.py"]
shutdown_timeout_seconds = 2
[devrun]
command = ["python3", "main.py"]
[health]
startup_probe = "process"
startup_timeout_seconds = 12
''',
        # Source ports are allocated by the actual manifest compiler; only
        # artifact fixtures carry their explicit locked 4200/4201 ports.
        'web/server.js': "require('http').createServer((q,s)=>s.end(require('./dependency.js'))).listen(Number(process.env.PORT),'0.0.0.0');\n",
        'worker/main.py': 'import time; time.sleep(3600)\n',
    }


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
            h.record_readonly_response('worker-startup', '/v1/runtime/identity', status, body)
            if h.initializing_response(status, body):
                return None
            if status != 200:
                raise AssertionError(f'owner identity initialization: HTTP {status}: {body.decode(errors="replace")}')
            identity = json.loads(body)['data']
            # Identity is intentionally served before startup recovery finishes.
            # This is a current observation, not a future admission lease. The
            # submit helper handles only a subsequent explicit zero-write gate.
            status, body = h.request_http(base, '/v1/runtime/recovery')
            h.record_readonly_response('worker-startup', '/v1/runtime/recovery', status, body)
            if h.initializing_response(status, body):
                return None
            if status != 200:
                raise AssertionError(f'owner initialization: HTTP {status}: {body.decode(errors="replace")}')
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
            request = {
                'operation_id': op, 'expected_runtime_instance_id': identity['runtime_instance_id'],
                'expected_revision': view(base, '/v1/runtime/status')['revision'],
                'workspace_id': identity['workspace_id'], 'kind': kind, 'profile': profile}
            status, data = submit_operation(
                base, request, time.monotonic() + h.REQUEST_TIMEOUT_SECONDS)
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
            code = '''import pathlib,socket,tomllib
lock=tomllib.loads(pathlib.Path('/review/code/release.lock.toml').read_text())
port=next(service['port'] for service in lock['services'] if service['service_id']=='worker')
s=socket.socket(); s.settimeout(1)
print(s.connect_ex(('127.0.0.1',port))); s.close()'''
            h.check('worker has no listener', h.docker('exec', cid, 'python3', '-c', code) != '0', 'actual locked worker port')
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
        # Keep the deployed web artifact (including its real symlink) intact.
        # Write real source/manifests only; never use an old release.lock as the
        # authority for a current Source request or extract over the symlink.
        install = '''import json,pathlib,sys
for name,content in json.loads(sys.argv[1]).items():
 path=pathlib.Path('/review/code',name)
 path.parent.mkdir(parents=True,exist_ok=True)
 path.write_text(content)'''
        files = source_files()
        (h.REPORT / 'source-files.json').write_text(json.dumps(files, indent=2))
        h.docker('exec', cid, 'python3', '-c', install, json.dumps(files))
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
