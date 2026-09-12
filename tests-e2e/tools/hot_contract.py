#!/usr/bin/env python3
"""Real Docker/app-cli A/B deployment acceptance; no AI calls or simulated app-cli."""
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import subprocess
import stat
import threading
import time
import urllib.error
import urllib.request
import uuid
import zipfile

from hot_cleanup import run_owned, cleanup as cleanup_owned

REPORT = Path(os.environ['E2E_REPORT_DIR']) / 'hot-contract'
REPORT.mkdir(parents=True, exist_ok=True)
RUN = os.environ['E2E_RUN_ID']
TOKEN = uuid.uuid4().hex
GENERATION = str(uuid.uuid4())
RUNTIME_IMAGE = os.environ.get('E2E_RUNTIME_IMAGE', 'dev-app-runtime:latest')
RESULTS = []


def check(name, ok, detail):
    RESULTS.append({'name': name, 'ok': bool(ok), 'detail': detail})
    (REPORT / 'assertions.json').write_text(json.dumps(RESULTS, indent=2))
    if not ok:
        raise AssertionError(name + ': ' + detail)


def docker(*args):
    try:
        return subprocess.check_output(['docker', *args], text=True, timeout=90, stderr=subprocess.STDOUT).strip()
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f'Docker command failed ({error.returncode}): ' + (error.output or '').replace(TOKEN, '<redacted>')) from None
    except subprocess.TimeoutExpired:
        raise RuntimeError('Docker command timed out after 90 seconds') from None


def artifact(release, content, extra=None, command="server.js"):
    manifest = f'''schema_version = 1
release_id = "{release}"
workspace_name = "review"
minimum_app_cli_version = "0.1.3"
runtime_image_digest = "{RUNTIME_IMAGE}"
[pingap]
mode = "managed"
version = "0.14.1"
commit = "c74e4eaa44e64958cffa18c33e8bbf5995b6844f"
[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200
[services.run]
command = ["node", "{command}"]
migrate = ["sh", "-c", "echo {release} >> /review/migrations.log"]
depends_on = []
shutdown_timeout_seconds = 3
[services.proxy]
path = "/"
strip_prefix = false
[services.health]
startup_timeout_seconds = 3
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"
[[services.logs]]
id = "application"
glob = "web*.log*"
format = "jsonl"
[services.env]
'''
    body = io.BytesIO()
    with zipfile.ZipFile(body, 'w', zipfile.ZIP_DEFLATED) as archive:
        archive.writestr('release.lock.toml', manifest)
        archive.writestr('web/server.js', "require('http').createServer((q,s)=>s.end(require('./dependency.js'))).listen(4200,'0.0.0.0');")
        archive.writestr('web/modules/content.js', 'module.exports=' + json.dumps(content))
        dependency = zipfile.ZipInfo('web/dependency.js')
        dependency.create_system = 3
        dependency.external_attr = (stat.S_IFLNK | 0o777) << 16
        archive.writestr(dependency, 'modules/content.js')
        if extra:
            archive.writestr(*extra)
    return body.getvalue()


def static_artifact(release, content, directory, port, broken=False):
    original = zipfile.ZipFile(io.BytesIO(artifact(release, content)))
    manifest = original.read('release.lock.toml').decode().replace('type = "node"', 'type = "static"', 1)
    manifest = manifest.replace('port = 4200', f'port = {port}\nstatic_content_dir = "{directory}"', 1)
    manifest = manifest.replace('command = ["node", "server.js"]', 'command = []', 1)
    if broken:
        manifest += '''
[[services]]
service_id = "broken"
name = "Broken"
dir = "broken"
type = "node"
kind = "web"
enabled = true
port = 4209
logs = []
env = {}
[services.run]
command = ["node", "missing.js"]
shutdown_timeout_seconds = 1
[services.health]
startup_timeout_seconds = 2
'''
    data = io.BytesIO()
    with zipfile.ZipFile(data, 'w', zipfile.ZIP_DEFLATED) as archive:
        archive.writestr('release.lock.toml', manifest)
        archive.writestr(f'web/{directory}/index.html', content)
        if directory != 'static-a':
            archive.writestr('web/static-a/index.html', 'STALE_STATIC_ROOT')
        if broken:
            archive.writestr('broken/placeholder', 'missing program is intentional')
    return data.getvalue()


A = artifact('manifest-A', 'content-A')
B = artifact('manifest-B', 'content-B')
missing = io.BytesIO()
with zipfile.ZipFile(missing, 'w') as z:
    z.writestr('unrelated.txt', 'no lock')
# B exceeds all former test budgets, using stored data so download size also grows.
large_b = io.BytesIO(B)
with zipfile.ZipFile(large_b, 'a', zipfile.ZIP_STORED) as archive:
    archive.writestr('large-file', 'x' * 20000)
    for index in range(12):
        archive.writestr(f'entries/{index}', 'entry')
B = large_b.getvalue()
PAYLOADS = {'/A': A, '/B': B, '/slow': B, '/truncated': B, '/badzip': b'not a zip', '/missing': missing.getvalue(), '/idle': B, '/orchestrate': artifact('manifest-broken', 'content-broken', command='missing.js')}
def link_payload(links):
    data = io.BytesIO(A)
    with zipfile.ZipFile(data, 'a', zipfile.ZIP_DEFLATED) as archive:
        for name, target in links:
            link = zipfile.ZipInfo(name)
            link.create_system = 3
            link.external_attr = (stat.S_IFLNK | 0o777) << 16
            archive.writestr(link, target)
    return data.getvalue()


PAYLOADS.update({
    '/static-a': static_artifact('static-A', 'STATIC_A', 'static-a', 4202),
    '/static-b': static_artifact('static-B', 'STATIC_B', 'static-b', 4202),
    '/static-c': static_artifact('static-C', 'STATIC_C', 'static-c', 4203),
    '/static-failed': static_artifact('static-failed', 'MUST_NOT_SERVE', 'static-failed', 4204, broken=True),
    '/after-static': artifact('after-static', 'AFTER_STATIC'),
})

PAYLOADS['/link-escape'] = link_payload([('escape', '../outside')])
PAYLOADS['/link-cycle'] = link_payload([('cycle-a', 'cycle-b'), ('cycle-b', 'cycle-a')])
RELEASE = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        payload = PAYLOADS.get(self.path)
        if payload is None:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        if self.path == '/slow':
            RELEASE.wait(60)
        if self.path == '/idle':
            time.sleep(7)
        if self.path == '/truncated':
            payload = payload[:len(payload) // 2]
        try:
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True


def request_http(base, path, body=None):
    req = urllib.request.Request(base + path, data=json.dumps(body).encode() if body is not None else None,
                                 headers={'X-Deploy-Token': TOKEN, 'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(req, timeout=10) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def poll(fn, timeout=90):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = fn()
            if last:
                return last
        except (OSError, ValueError):
            pass
        time.sleep(.2)
    raise TimeoutError('bounded state poll timed out: ' + str(last))


def successful_identity(operation, expected_operation, artifact_release):
    return (operation.get('operation_id') == expected_operation
            and operation.get('request_release_id') == 'request-' + expected_operation
            and operation.get('artifact_release_id') == artifact_release
            and operation.get('deployment_generation_id') == GENERATION
            and operation.get('deploy_stage') == 'succeeded'
            and operation.get('persisted') is True
            and operation.get('phase') == 'running'
            and not operation.get('error'))


def main():
    server = http.server.ThreadingHTTPServer(('0.0.0.0', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    container = None
    try:
        builtin = os.environ.get('E2E_APP_CLI_ENGINE') == 'builtin'
        entry = ['-e', 'APP_CLI_SKIP_PG_WAIT=1', '--entrypoint', '/usr/local/bin/app-cli', RUNTIME_IMAGE, '--workspace', '/review/code', '--log-dir', '/review/logs', 'serve'] if builtin else ['-e', 'USERAPP_WORKSPACE_DIR=/review', '-e', 'APP_ID=app-review', RUNTIME_IMAGE]
        container = run_owned(REPORT, RUN, os.environ['E2E_CASE_ID'], ['-d',
                           '-p', '127.0.0.1::3010', '-p', '127.0.0.1::9080',
                           '-e', 'APP_CLI_DEPLOY_TOKEN=' + TOKEN,
                           '-e', f'APP_DEPLOY_URL=http://host.docker.internal:{server.server_port}/A',
                           '-e', 'APP_RELEASE_ID=request-initial-A',
                           '-e', 'APP_DEPLOY_OPERATION_ID=initial-A',
                           '-e', 'APP_DEPLOY_GENERATION_ID=' + GENERATION,
                           '-e', 'APP_DEPLOY_SHA256=' + hashlib.sha256(A).hexdigest(),
                           '-e', 'APP_DEPLOY_MAX_FILE_BYTES=4096',
                           '-e', 'APP_DEPLOY_MAX_DOWNLOAD_BYTES=16384',
                           '-e', 'APP_DEPLOY_MAX_EXTRACTED_BYTES=6144',
                           '-e', 'APP_DEPLOY_MAX_ENTRIES=8',
                           '-e', 'APP_DEPLOY_READ_IDLE_SECONDS=5',
                           *entry], 'builtin' if builtin else 'supervisord', docker_fn=docker)
        identity = json.loads(docker('inspect', '--format', '{{json .}}', container))
        (REPORT / 'identity.json').write_text(json.dumps({k: identity[k] for k in ['Id', 'Image', 'Name']}))
        base = 'http://' + docker('port', container, '3010/tcp')
        app = 'http://' + docker('port', container, '9080/tcp')
        poll(lambda: request_http(base, '/health')[0] == 200)

        def submit(path, op, sha=None):
            body = {'url': f'http://host.docker.internal:{server.server_port}{path}', 'release_id': 'request-' + op, 'operation_id': op, 'deployment_generation_id': GENERATION}
            if sha:
                body['sha256'] = sha
            status, data = request_http(base, '/v1/deploy', body)
            return status, json.loads(data)

        def terminal(op):
            status, data = request_http(base, '/v1/deploy/status')
            if status != 200:
                raise AssertionError('Status endpoint returned HTTP ' + str(status))
            data = json.loads(data)['data']
            operation = data.get('operation') or {}
            if data.get('protocol_version', 0) < 4:
                raise AssertionError('Unified deployment protocol v4 is required')
            if operation.get('operation_id') == op and operation.get('phase') in ['running', 'failed'] and data.get('phase') in ['running', 'failed'] and (operation.get('recovery') or {}).get('status') != 'pending':
                (REPORT / (op + '.json')).write_text(json.dumps(data, indent=2))
                return operation
            return None

        def port_closed(port):
            script = f"fetch('http://127.0.0.1:{port}/health').then(()=>process.exit(1)).catch(()=>process.exit(0))"
            return subprocess.run(['docker', 'exec', container, 'node', '-e', script], timeout=10, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0

        configured_env = dict(entry.split('=', 1) for entry in identity['Config']['Env'] if '=' in entry)
        check('cold A operation configured', configured_env.get('APP_DEPLOY_OPERATION_ID') == 'initial-A' and configured_env.get('APP_DEPLOY_GENERATION_ID') == GENERATION and configured_env.get('APP_RELEASE_ID') == 'request-initial-A', 'verified actual container operation and generation env')
        result = poll(lambda: terminal('initial-A'))
        check('A identity', successful_identity(result, 'initial-A', 'manifest-A'), str(result))
        check('A serves content', poll(lambda: request_http(app, '/')[1] == b'content-A'), 'actual response body')
        for name, path, sha in [('missing-url', '/404', None), ('truncated', '/truncated', None), ('sha', '/B', '0'*64), ('zip', '/badzip', None), ('manifest', '/missing', None), ('idle', '/idle', None), ('link-escape', '/link-escape', None), ('link-cycle', '/link-cycle', None)]:
            status, _ = submit(path, name, sha)
            check(name + ' accepted', status == 202, str(status))
            result = poll(lambda: terminal(name))
            check(name + ' fails correct operation', result['phase'] == 'failed' and result.get('operation_id') == name and result.get('request_release_id') == 'request-' + name and result.get('deployment_generation_id') == GENERATION and result.get('deploy_stage') == 'failed' and result.get('persisted') is True, str(result))
            check(name + ' old content healthy', request_http(app, '/')[1] == b'content-A' and request_http(base, '/ready')[0] == 200, 'A response and readiness')
            residue = docker('exec', container, 'sh', '-c', 'find /review/.incoming /review/.staging -mindepth 1 -print')
            check(name + ' no temporary residue', not residue, residue)
        status, _ = submit('/orchestrate', 'broken-B')
        check('broken B accepted', status == 202, str(status))
        result = poll(lambda: terminal('broken-B'))
        check('broken B operation failed', result['phase'] == 'failed', str(result))
        check('switched failure does not restore old readiness', request_http(base, '/ready')[0] != 200 and port_closed(4200), str(result))
        recovery = result.get('recovery') or {}
        check('migration reversal never claimed', recovery.get('database_migrations_reversed') is not True, str(recovery))
        migrations = docker('exec', container, 'cat', '/review/migrations.log').splitlines()
        check('failed switch does not rerun old migrations', migrations == ['manifest-A', 'manifest-broken'], str(migrations))
        status, _ = submit('/A', 'manual-A')
        check('manual redeploy accepted', status == 202, str(status))
        result = poll(lambda: terminal('manual-A'))
        check('manual redeploy restores A', successful_identity(result, 'manual-A', 'manifest-A') and poll(lambda: request_http(app, '/')[1] == b'content-A'), str(result))
        status, _ = submit('/slow', 'winner-B')
        check('slow B accepted', status == 202, str(status))
        status, _ = submit('/A', 'loser-A')
        check('concurrent deployment rejected', status == 409, str(status))
        check('A serves during prepare', request_http(app, '/')[1] == b'content-A', 'actual response body')
        RELEASE.set()
        result = poll(lambda: terminal('winner-B'))
        check('B identity', successful_identity(result, 'winner-B', 'manifest-B'), str(result))
        check('former capacity and entry settings do not reject B', result['phase'] == 'running', 'B exceeds former download, total, file and entry settings')
        check('B serves content', poll(lambda: request_http(app, '/')[1] == b'content-B'), 'actual response body')
        docker('restart', container)
        base = 'http://' + docker('port', container, '3010/tcp')
        app = 'http://' + docker('port', container, '9080/tcp')
        result = poll(lambda: terminal('winner-B'))
        check('restart retains B operation identity', successful_identity(result, 'winner-B', 'manifest-B'), str(result))
        check('restart retains B content despite cold A env', poll(lambda: request_http(app, '/')[1] == b'content-B'), 'immutable env still targets A')
        check('restart preserves container identity', docker('inspect', '--format', '{{.Id}}', container) == identity['Id'], identity['Id'])
        for path, op, content in [('/static-a', 'static-A', b'STATIC_A'), ('/static-b', 'static-B', b'STATIC_B'), ('/static-c', 'static-C', b'STATIC_C')]:
            status, data = submit(path, op)
            check(op + ' accepted', status == 202, str(data))
            result = poll(lambda: terminal(op))
            check(op + ' identity', successful_identity(result, op, op), str(result))
            check(op + ' actual content', poll(lambda: request_http(app, '/')[1] == content), 'actual proxy response')
        check('static old port released', port_closed(4202), 'port 4202 must no longer accept requests')
        status, data = submit('/static-failed', 'static-failed')
        check('static failure accepted', status == 202, str(data))
        result = poll(lambda: terminal('static-failed'))
        check('static failed deployment stays failed', result['phase'] == 'failed' and request_http(base, '/ready')[0] != 200, str(result))
        check('static failed generation port released', port_closed(4204), 'port 4204 must be released after failure')
        status, data = submit('/after-static', 'after-static')
        check('static removal accepted', status == 202, str(data))
        result = poll(lambda: terminal('after-static'))
        check('static removal serves replacement', result['phase'] == 'running' and poll(lambda: request_http(app, '/')[1] == b'AFTER_STATIC'), str(result))
        check('removed static listener closed', port_closed(4203), 'port 4203 must be released when removed')
        check('container unchanged', docker('inspect', '--format', '{{.Id}}', container) == container, container)
    finally:
        RELEASE.set()
        result = cleanup_owned(REPORT, RUN, os.environ['E2E_CASE_ID'], docker_fn=docker, secrets=(TOKEN,))
        for detail in result['errors']:
            if detail.startswith('pre-cleanup diagnostics'):
                RESULTS.append({'name': 'pre-cleanup diagnostics', 'ok': False, 'detail': detail})
        RESULTS.append({'name': 'owned resource cleanup', 'ok': result['ok'], 'detail': json.dumps(result)})
        (REPORT / 'assertions.json').write_text(json.dumps(RESULTS, indent=2))
        server.shutdown()


if __name__ == '__main__':
    main()
