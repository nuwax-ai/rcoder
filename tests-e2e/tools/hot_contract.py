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

REPORT = Path(os.environ['E2E_REPORT_DIR']) / 'hot-contract'
REPORT.mkdir(parents=True, exist_ok=True)
RUN = os.environ['E2E_RUN_ID']
TOKEN = uuid.uuid4().hex
RUNTIME_IMAGE = os.environ.get('E2E_RUNTIME_IMAGE', 'dev-app-runtime:latest')
RESULTS = []


def check(name, ok, detail):
    RESULTS.append({'name': name, 'ok': bool(ok), 'detail': detail})
    (REPORT / 'assertions.json').write_text(json.dumps(RESULTS, indent=2))
    if not ok:
        raise AssertionError(name + ': ' + detail)


def docker(*args):
    return subprocess.check_output(['docker', *args], text=True, timeout=90, stderr=subprocess.STDOUT).strip()


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


def main():
    server = http.server.ThreadingHTTPServer(('0.0.0.0', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    container = None
    try:
        builtin = os.environ.get('E2E_APP_CLI_ENGINE') == 'builtin'
        entry = ['-e', 'APP_CLI_SKIP_PG_WAIT=1', '--entrypoint', '/usr/local/bin/app-cli', RUNTIME_IMAGE, '--workspace', '/review/code', '--log-dir', '/review/logs', 'serve'] if builtin else ['-e', 'USERAPP_WORKSPACE_DIR=/review', '-e', 'APP_ID=app-review', RUNTIME_IMAGE]
        container = docker('run', '-d', '--label', 'rcoder.e2e.run=' + RUN,
                           '--name', 'rcoder-review-' + uuid.uuid4().hex[:16],
                           '-p', '127.0.0.1::3010', '-p', '127.0.0.1::9080',
                           '-e', 'APP_CLI_DEPLOY_TOKEN=' + TOKEN,
                           '-e', 'APP_DEPLOY_MAX_FILE_BYTES=4096',
                           '-e', 'APP_DEPLOY_MAX_DOWNLOAD_BYTES=16384',
                           '-e', 'APP_DEPLOY_MAX_EXTRACTED_BYTES=6144',
                           '-e', 'APP_DEPLOY_MAX_ENTRIES=8',
                           '-e', 'APP_DEPLOY_READ_IDLE_SECONDS=5',
                           *entry)
        identity = json.loads(docker('inspect', '--format', '{{json .}}', container))
        (REPORT / 'identity.json').write_text(json.dumps({k: identity[k] for k in ['Id', 'Image', 'Name']}))
        base = 'http://' + docker('port', container, '3010/tcp')
        app = 'http://' + docker('port', container, '9080/tcp')
        poll(lambda: request_http(base, '/health')[0] == 200)

        def submit(path, op, sha=None):
            body = {'url': f'http://host.docker.internal:{server.server_port}{path}', 'release_id': 'request-' + op, 'operation_id': op}
            if sha:
                body['sha256'] = sha
            status, data = request_http(base, '/v1/deploy', body)
            return status, json.loads(data)

        def terminal(op):
            status, data = request_http(base, '/v1/deploy/status')
            data = json.loads(data)['data']
            operation = data.get('operation') or {}
            if operation.get('operation_id') == op and operation.get('phase') in ['running', 'failed']:
                (REPORT / (op + '.json')).write_text(json.dumps(data, indent=2))
                return operation
            return None

        status, accepted = submit('/A', 'initial-A')
        check('A accepted', status == 202, str(accepted))
        result = poll(lambda: terminal('initial-A'))
        check('A identity', result['phase'] == 'running' and result['artifact_release_id'] == 'manifest-A', str(result))
        check('A serves content', poll(lambda: request_http(app, '/')[1] == b'content-A'), 'actual response body')
        for name, path, sha in [('missing-url', '/404', None), ('truncated', '/truncated', None), ('sha', '/B', '0'*64), ('zip', '/badzip', None), ('manifest', '/missing', None), ('idle', '/idle', None), ('link-escape', '/link-escape', None), ('link-cycle', '/link-cycle', None)]:
            status, _ = submit(path, name, sha)
            check(name + ' accepted', status == 202, str(status))
            result = poll(lambda: terminal(name))
            check(name + ' fails correct operation', result['phase'] == 'failed', str(result))
            check(name + ' old content healthy', request_http(app, '/')[1] == b'content-A' and request_http(base, '/ready')[0] == 200, 'A response and readiness')
            residue = docker('exec', container, 'sh', '-c', 'find /review/.incoming /review/.staging -mindepth 1 -print')
            check(name + ' no temporary residue', not residue, residue)
        status, _ = submit('/orchestrate', 'broken-B')
        check('broken B accepted', status == 202, str(status))
        result = poll(lambda: terminal('broken-B'))
        check('broken B operation failed', result['phase'] == 'failed', str(result))
        def restored():
            op = terminal('broken-B')
            return op if op and (op.get('recovery') or {}).get('status') == 'restored' else None
        result = poll(restored)
        check('old code and orchestration restored', request_http(app, '/')[1] == b'content-A' and request_http(base, '/ready')[0] == 200, str(result))
        check('migration reversal never claimed', result['recovery']['database_migrations_reversed'] is False, str(result['recovery']))
        migrations = docker('exec', container, 'cat', '/review/migrations.log').splitlines()
        check('recovery does not rerun old migrations', migrations == ['manifest-A', 'manifest-broken'], str(migrations))
        status, _ = submit('/slow', 'winner-B')
        check('slow B accepted', status == 202, str(status))
        status, _ = submit('/A', 'loser-A')
        check('concurrent deployment rejected', status == 409, str(status))
        check('A serves during prepare', request_http(app, '/')[1] == b'content-A', 'actual response body')
        RELEASE.set()
        result = poll(lambda: terminal('winner-B'))
        check('B identity', result['phase'] == 'running' and result['artifact_release_id'] == 'manifest-B', str(result))
        check('former capacity and entry settings do not reject B', result['phase'] == 'running', 'B exceeds former download, total, file and entry settings')
        check('B serves content', poll(lambda: request_http(app, '/')[1] == b'content-B'), 'actual response body')
        check('container unchanged', docker('inspect', '--format', '{{.Id}}', container) == container, container)
    finally:
        RELEASE.set()
        if container:
            try:
                (REPORT / 'container.log').write_text(docker('logs', container).replace(TOKEN, '<redacted>'))
                if not builtin:
                    logs = docker('exec', container, 'sh', '-c', 'tail -n 200 /home/user/logs/app-cli.out.log /home/user/logs/app-cli.err.log')
                    (REPORT / 'app-cli.log').write_text(logs.replace(TOKEN, '<redacted>'))
            except Exception as error:
                RESULTS.append({'name': 'pre-cleanup diagnostics', 'ok': False, 'detail': str(error)})
            try:
                owned = docker('inspect', '--format', '{{index .Config.Labels "rcoder.e2e.run"}}', container)
                if owned != RUN:
                    raise RuntimeError('cleanup ownership mismatch')
                docker('rm', '-f', container)
                RESULTS.append({'name': 'owned resource cleanup', 'ok': True, 'detail': container})
            except Exception as error:
                RESULTS.append({'name': 'owned resource cleanup', 'ok': False, 'detail': str(error)})
            (REPORT / 'assertions.json').write_text(json.dumps(RESULTS, indent=2))
        server.shutdown()


if __name__ == '__main__':
    main()
