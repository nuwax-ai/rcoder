#!/usr/bin/env python3
"""Real, no-LLM prod contract against an already running isolated Docker RCoder.

Python >= 3.11. No builds, shared service updates, database cleanup or volume
deletion. The fixture follows workspace-manifest/tests/fixtures/lock_v1.toml,
workspace-manifest/src/release_lock.rs and app-cli's static_content_dir contract.
An empty Start first provisions the base container; real business PostgreSQL
SELECT 1 is a fixture prerequisite before cold replacement on the same mounts.
Success removes only captured, identity-checked containers; failure keeps them.
The ZIP and report remain on disk. Missing evidence is failure, never a skip.

Example: python3 tests-e2e/tools/prod_readiness_contract.py \
  --url http://127.0.0.1:18080 --runtime-image rcoder-userapp-contract:local \
  --report /tmp/prod-contract.json
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import tomllib
import traceback
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile


def run(argv, timeout=30):
    result = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{argv[0:3]} exited {result.returncode}: {result.stderr[-2000:]}')
    return result.stdout


def source_identity(repo):
    git = lambda *args: subprocess.check_output(['git', '-C', str(repo), *args])
    diff = git('diff', '--binary', 'HEAD')
    untracked = {}
    for name in git('ls-files', '--others', '--exclude-standard', '-z').split(b'\0'):
        if name:
            relative = os.fsdecode(name)
            path = repo / relative
            if path.is_file():
                untracked[relative] = hashlib.sha256(path.read_bytes()).hexdigest()
    return {'commit': git('rev-parse', 'HEAD').decode().strip(),
            'diff_sha256': hashlib.sha256(diff).hexdigest(),
            'untracked_sha256': untracked}


def make_artifact(repo, directory, release, marker):
    # The golden lock establishes required field names; static_content_dir is
    # the field emitted by build_release_lock for a static build artifact.
    tomllib.loads((repo / 'crates/workspace-manifest/tests/fixtures/lock_v1.toml').read_text())
    minimum = tomllib.loads((repo / 'crates/app-cli/Cargo.toml').read_text())['package']['version']
    devtool = (repo / 'crates/app-cli/src/build_deploy/devtool.rs').read_text()
    def constant(name):
        import re
        match = re.search(rf'{name}: &str = "([^"]+)"', devtool)
        if not match:
            raise RuntimeError(f'cannot read pinned {name}')
        return match.group(1)
    lock = f'''schema_version = 1
release_id = "{release}"
workspace_name = "prod-contract"
minimum_app_cli_version = "{minimum}"
runtime_image_digest = "prod-readiness-contract"

[pingap]
mode = "managed"
version = "{constant('DEFAULT_PINGAP_VERSION')}"
commit = "{constant('DEFAULT_PINGAP_COMMIT')}"

[[services]]
service_id = "web"
name = "Contract static frontend"
dir = "web"
type = "static"
kind = "web"
enabled = true
port = 4100
static_content_dir = "dist"
logs = []

[services.run]
command = []
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]
startup_path = "/health"
readiness_path = "/health"
liveness_path = "/health"

[services.proxy]
path = "/"
strip_prefix = false
plugins = []
upstream_includes = []

[services.env]
'''
    tomllib.loads(lock)
    files = {
        'workspace.manifest.toml': 'schema_version = 1\n[workspace]\nname = "prod-contract"\n',
        'release.lock.toml': lock,
        'web/project.manifest.toml': 'schema_version = 1\n[project]\nservice_id="web"\nname="Contract frontend"\ntype="static"\n[build]\ncommand=["true"]\nartifact="dist"\n[proxy]\npath="/"\nstrip_prefix=false\n',
        'web/dist/index.html': f'<!doctype html><title>{marker}</title><main>{marker}</main><script src="/app.js"></script>\n',
        'web/dist/app.js': f'window.contractMarker = "{marker}";\n',
    }
    artifact = directory / f'workspace-package-{release}.zip'
    with zipfile.ZipFile(artifact, 'w', zipfile.ZIP_DEFLATED) as archive:
        for name, content in files.items():
            archive.writestr(name, content)
    return artifact


class Contract:
    def __init__(self, args, report, app):
        self.args, self.report, self.app = args, report, app
        self.containers = {}
        self.volumes = None
        self.sentinel = f'/home/user/data/.prod-contract-{app}'
        self.marker = f'PROD_CONTRACT_{app}'
        self.lifecycle = None

    def check(self, name, ok, detail=None):
        self.report['checks'].append({'name': name, 'ok': bool(ok), 'detail': detail})
        print(name, 'PASS' if ok else 'FAIL', flush=True)
        if not ok:
            raise RuntimeError(f'{name}: {detail}')

    def api(self, path, body=None, timeout=20):
        headers = {'content-type': 'application/json'}
        if self.args.api_key:
            headers['x-api-key'] = self.args.api_key
        request = urllib.request.Request(self.args.url.rstrip('/') + path,
            data=None if body is None else json.dumps(body).encode(), headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            raise RuntimeError(f'{path}: HTTP {error.code} {error.read().decode(errors="replace")}') from error
        envelope = json.loads(raw)
        if envelope.get('code') != '0000' or envelope.get('success') is not True:
            raise RuntimeError(f'{path}: {envelope}')
        return status, envelope

    def readiness(self):
        _, response = self.api(f'/api/v1/userapp/{self.app}/prod/readiness?user_id={urllib.parse.quote(self.args.user_id)}')
        data = response['data']
        if not isinstance(data.get('container'), dict):
            raise RuntimeError(f'container observation missing: {data}')
        if data.get('status') == 'failed':
            diagnostic = {'readiness': data}
            try:
                cid, _ = self.current()
                diagnostic['container_id'] = cid
                # Keep the JSON error envelope even when management returns 503.
                diagnostic['runtime'] = json.loads(self.execute(cid, 'curl', '-sS',
                    '--max-time', '8', 'http://127.0.0.1:3010/v1/deploy/status'))
            except Exception as error:
                diagnostic['runtime_observation_error'] = str(error)
            self.report['business_failure'] = diagnostic
            raise RuntimeError(f'business readiness failed: {diagnostic}')
        return data

    def wait(self, name, probe, predicate, seconds=180):
        deadline, last = time.monotonic() + seconds, None
        while time.monotonic() < deadline:
            last = probe()
            if predicate(last):
                self.check(name, True, last)
                return last
            time.sleep(0.15)
        self.check(name, False, {'timeout_seconds': seconds, 'last': last})

    def inspect(self, cid):
        value = json.loads(run(['docker', 'inspect', cid]))[0]
        labels = value['Config'].get('Labels') or {}
        if (value['Id'] != cid or labels.get('app-id') != self.app
                or labels.get('managed-by') != 'rcoder-app-manager'
                or labels.get('service-type') != 'user-app'):
            raise RuntimeError(f'refusing foreign or ambiguous container {cid}')
        if value['Image'] != self.report['expected_image_id']:
            raise RuntimeError(f'container {cid} is not running the requested test image')
        mounts = [{key: mount.get(key) for key in ('Type', 'Name', 'Source', 'Destination')}
                  for mount in value['Mounts']]
        self.containers[cid] = {'id': cid, 'image_id': value['Image'],
            'image_ref': value['Config']['Image'], 'mounts': mounts}
        self.report['containers'] = list(self.containers.values())
        return value

    def current(self):
        ids = run(['docker', 'ps', '-q', '--no-trunc', '--filter', f'label=app-id={self.app}',
                   '--filter', 'label=managed-by=rcoder-app-manager', '--filter', 'label=service-type=user-app']).split()
        if len(ids) != 1:
            raise RuntimeError(f'expected one running owned prod container, got {ids}')
        cid = ids[0]
        value = self.inspect(cid)
        env = dict(item.split('=', 1) for item in value['Config']['Env'] if '=' in item)
        if env.get('APP_ID') != self.app or env.get('PROJECT_ID') != self.app:
            raise RuntimeError('container application binding differs from this fixture')
        mounts = [{key: mount.get(key) for key in ('Type', 'Name', 'Source', 'Destination')}
                  for mount in value['Mounts']]
        return cid, mounts

    def execute(self, cid, *argv):
        self.inspect(cid)
        return run(['docker', 'exec', cid, *argv])

    def runtime_json(self, cid, path):
        raw = self.execute(cid, 'curl', '-fsS', '--max-time', '8', 'http://127.0.0.1:3010' + path)
        response = json.loads(raw)
        # RuntimeKernel has its own v5 envelope; deployment/readiness retain
        # the shared HttpResult envelope. Do not conflate their success codes.
        expected_code = 'OK' if path.startswith('/v1/runtime/') else '0000'
        if response.get('code') != expected_code or response.get('success') is not True:
            raise RuntimeError(f'app-cli {path}: {response}')
        return response['data']

    def wait_postgres(self, cid):
        # Fixture preparation mirrors an already provisioned app210 container.
        # Use the same default TCP/business database target as pg_wait.rs, not
        # pg_isready or the local administrative socket. No product flag changes.
        command = '''export PGCONNECT_TIMEOUT=2
if [ -n "${DATABASE_URL:-}" ]; then
    exec psql "$DATABASE_URL" -X -A -t -v ON_ERROR_STOP=1 -c 'SELECT 1'
fi
export PGPASSWORD="${POSTGRES_PASSWORD:-dev}"
exec psql -h "${PGHOST:-localhost}" -p "${PGPORT:-5432}" \
    -U "${POSTGRES_USER:-dev}" -d "${POSTGRES_DB:-dev}" \
    -X -A -t -v ON_ERROR_STOP=1 -c 'SELECT 1'
'''
        started, last, attempts = time.monotonic(), None, 0
        while time.monotonic() - started < 180:
            self.inspect(cid)
            attempts += 1
            result = subprocess.run(['docker', 'exec', cid, 'sh', '-ec', command],
                capture_output=True, text=True, timeout=10)
            last = {'exit_code': result.returncode, 'stdout': result.stdout.strip(),
                    'stderr': result.stderr[-1000:]}
            if result.returncode == 0:
                self.check('base container business PostgreSQL SELECT 1 succeeds',
                    result.stdout.strip() == '1', {'attempts': attempts,
                        'elapsed_seconds': round(time.monotonic() - started, 3), 'result': last})
                return
            if result.returncode not in (1, 2):
                raise RuntimeError(f'PostgreSQL fixture probe cannot execute: {last}')
            time.sleep(0.4)
        self.check('base container business PostgreSQL SELECT 1 succeeds', False,
                   {'attempts': attempts, 'fixture_timeout_seconds': 180, 'last': last})

    def sql(self, cid, statement):
        command = '''export PGCONNECT_TIMEOUT=3
if [ -n "${DATABASE_URL:-}" ]; then
    exec psql "$DATABASE_URL" -X -A -t -v ON_ERROR_STOP=1 -c "$1"
fi
export PGPASSWORD="${POSTGRES_PASSWORD:-dev}"
exec psql -h "${PGHOST:-localhost}" -p "${PGPORT:-5432}" \
    -U "${POSTGRES_USER:-dev}" -d "${POSTGRES_DB:-dev}" \
    -X -A -t -v ON_ERROR_STOP=1 -c "$1"
'''
        return self.execute(cid, 'sh', '-ec', command, 'contract-sql', statement).strip()

    def verify_mounts(self, cid, mounts, phase):
        relevant = sorted((mount for mount in mounts if mount['Destination'] in
                    (f'/home/user/{self.app}', f'/home/user/{self.app}/code', '/home/user/data', '/home/user/logs')),
                    key=lambda mount: mount['Destination'])
        if self.volumes is None:
            self.check('owned data mount exists', any(m['Destination'] == '/home/user/data' for m in relevant), relevant)
            self.volumes = relevant
            self.report['retained_mounts'] = relevant
            self.execute(cid, 'python3', '-c', 'from pathlib import Path;import sys;Path(sys.argv[1]).write_text(sys.argv[2])', self.sentinel, self.marker)
        else:
            self.check(phase + ': original mounts retained', relevant == self.volumes, relevant)
        self.check(phase + ': data sentinel retained', self.execute(cid, 'cat', self.sentinel) == self.marker)
        return relevant

    def verify_runtime(self, phase, expected_deployment):
        cid, mounts = self.current()
        identity = self.runtime_json(cid, '/v1/runtime/identity')
        deploy = self.runtime_json(cid, '/v1/deploy/status')
        workspace = identity['source_root']
        self.check(phase + ': owner workspace identity', identity['application_id'] == self.app
                   and workspace == f'/home/user/{self.app}/code', identity)
        native = json.loads(self.execute(cid, 'app-cli', 'owner', 'status', '--workspace', workspace))
        operation = deploy.get('operation') or {}
        generation = operation.get('deployment_generation_id')
        self.check(phase + ': persisted original deployment identity',
                   operation.get('operation_id') == expected_deployment and operation.get('persisted') is True
                   and generation == expected_deployment and identity['deployment_generation_id'] == generation,
                   {'operation': operation, 'identity': identity})
        self.check(phase + ': native and deployment generations are separate',
                   native.get('phase') == 'ready' and bool(native.get('generation'))
                   and native['generation'] != generation, native)
        self.check(phase + ': static HTML reachable',
                   self.marker in self.execute(cid, 'curl', '-fsS', '--max-time', '8', 'http://127.0.0.1:9080/'))
        self.check(phase + ': static JS reachable',
                   self.marker in self.execute(cid, 'curl', '-fsS', '--max-time', '8', 'http://127.0.0.1:9080/app.js'))
        self.verify_mounts(cid, mounts, phase)
        self.check(phase + ': original PostgreSQL row retained',
            self.sql(cid, 'SELECT marker FROM public.codex_contract_sentinel WHERE id=1') == self.marker)
        self.report.setdefault('runtime_evidence', []).append({'phase': phase, 'container_id': cid,
            'identity': identity, 'native': native, 'deployment': operation,
            'app_cli_sha256': self.execute(cid, 'sha256sum', '/usr/local/bin/app-cli').split()[0]})
        return cid, identity

    def control(self, action):
        status, response = self.api('/computer/pod/' + action, {
            'app_id': self.app, 'app_stage': 'prod', 'service_type': 'userapp',
            'user_id': self.args.user_id, 'project_id': self.app, 'lifecycle_id': self.lifecycle,
            'request_id': f'{action}-{uuid.uuid4().hex}'})
        record = response['data']
        operation_id = response.get('operation_id')
        self.check(action + ': exact asynchronous admission', status == 202 and bool(operation_id)
                   and record.get('operation_id') == operation_id and record.get('app_id') == self.app
                   and record.get('scope') == 'Prod' and record.get('action') == action, record)
        expected_path = f'/computer/pod/operations/{self.app}/{urllib.parse.quote(operation_id, safe="")}'
        self.check(action + ': operation query is bound to own app', record.get('status_url') == expected_path, record)
        def terminal():
            _, envelope = self.api(expected_path)
            value = envelope['data']
            if value.get('operation_id') != operation_id or value.get('app_id') != self.app:
                raise RuntimeError(f'operation query identity changed: {value}')
            if value.get('state') in ('failed', 'superseded', 'recovery_required'):
                raise RuntimeError(f'{action} failed or remains protected: {value}')
            return value
        if action == 'restart':
            self.wait('readiness observes the admitted restarting operation', self.readiness,
                lambda data: data['container'].get('status') == 'restarting'
                    and (data['container'].get('operation') or {}).get('operation_id') == operation_id,
                seconds=60)
        self.wait(action + ': original operation succeeded', terminal, lambda value: value.get('state') == 'succeeded')
        return operation_id

    def owner_recovery(self, cid, original_identity):
        before = self.runtime_json(cid, '/v1/runtime/identity')
        self.check('fault targets the previously observed owner',
            before['runtime_instance_id'] == original_identity['runtime_instance_id'], before)
        status = self.execute(cid, 'supervisorctl', '-c', '/etc/supervisor/supervisord.conf', 'status')
        candidates = []
        for line in status.splitlines():
            words = line.split()
            if len(words) < 2 or words[1] != 'RUNNING':
                continue
            pid = self.execute(cid, 'supervisorctl', '-c', '/etc/supervisor/supervisord.conf', 'pid', words[0]).strip()
            if not pid.isdigit() or int(pid) <= 1:
                continue
            raw = self.execute(cid, 'cat', f'/proc/{pid}/cmdline').split('\x00')
            if len(raw) >= 2 and Path(raw[0]).name == 'app-cli' and raw[1] == 'serve':
                candidates.append((words[0], pid, raw))
        self.check('exact supervised owner identified', len(candidates) == 1, candidates)
        program, pid, _ = candidates[0]
        # Check the actual argv again in the same process that sends SIGKILL.
        code = ('import os,sys,signal;from pathlib import Path;p=int(sys.argv[1]);'
                'a=Path(f"/proc/{p}/cmdline").read_bytes().split(b"\\0");'
                'assert p>1 and Path(os.fsdecode(a[0])).name=="app-cli" and a[1]==b"serve";'
                'os.kill(p,signal.SIGKILL)')
        self.execute(cid, 'python3', '-c', code, pid)
        self.report['owner_fault'] = {'container_id': cid, 'program': program, 'pid': int(pid)}
        def recovered():
            # A temporary TCP failure is an expected observation after SIGKILL;
            # only this explicit fault-window probe tolerates it, within budget.
            result = subprocess.run(['docker', 'exec', cid, 'curl', '-sS', '--max-time', '3',
                'http://127.0.0.1:3010/v1/runtime/identity'], capture_output=True, text=True, timeout=10)
            if result.returncode:
                if result.returncode in (7, 28, 52, 56):
                    return None
                raise RuntimeError(f'owner probe failed: {result.returncode} {result.stderr}')
            value = json.loads(result.stdout)
            if value.get('code') != 'OK' or value.get('success') is not True:
                if value.get('code') == 'ERR_INITIALIZING':
                    return None
                raise RuntimeError(f'owner recovery remains blocked: {value}')
            return value['data']
        self.wait('supervisord restores a new owner in the same container', recovered,
            lambda identity: identity is not None and identity['runtime_instance_id'] != original_identity['runtime_instance_id'])
        current, _ = self.current()
        self.check('owner recovery keeps exact physical container', current == cid, current)
        self.wait('business ready after owner recovery', self.readiness, lambda data: data['ready'] is True)

    def cleanup(self):
        # No DELETE API, volume rm, -v, prune or broad container name matching.
        remaining = set(run(['docker', 'ps', '-aq', '--no-trunc']).split())
        for cid in self.containers:
            if cid in remaining:
                self.inspect(cid)
                run(['docker', 'rm', '-f', cid])
        self.report['cleanup'] = {'captured_containers_removed': True, 'volumes_removed': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True)
    parser.add_argument('--runtime-image', required=True,
                        help='already built local image configured on the isolated RCoder')
    parser.add_argument('--artifact-advertise-host', default='host.docker.internal')
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--source-dir', default=Path(__file__).resolve().parents[2], type=Path)
    parser.add_argument('--user-id', default='prod-contract-user')
    parser.add_argument('--api-key', default=os.environ.get('RCODER_E2E_API_KEY'))
    args = parser.parse_args()
    parsed = urllib.parse.urlsplit(args.url)
    if parsed.scheme not in ('http', 'https') or parsed.hostname not in ('127.0.0.1', 'localhost', '::1'):
        parser.error('--url must target the already started isolated loopback RCoder')
    if args.report.exists():
        parser.error('--report already exists; preserve previous evidence and choose a new path')
    args.report.parent.mkdir(parents=True, exist_ok=True)
    repo = args.source_dir.resolve()
    app, release = 'prc' + uuid.uuid4().hex[:16], uuid.uuid4().hex
    report = {'success': False, 'app_id': app, 'checks': [], 'containers': [], 'source': source_identity(repo)}
    contract = Contract(args, report, app)
    server = None
    try:
        image = json.loads(run(['docker', 'image', 'inspect', args.runtime_image]))[0]
        report['expected_image_id'] = image['Id']
        report['runtime_image'] = args.runtime_image
        contract.check('no existing resource has this generated app identity',
            not run(['docker', 'ps', '-aq', '--filter', f'label=app-id={app}']).strip())
        _, base = contract.api(f'/api/v1/userapp/{app}/start', {
            'user_id': args.user_id, 'request_id': 'base-' + uuid.uuid4().hex,
            'env': {'PROJECT_ID': app}}, timeout=330)
        contract.check('empty Start creates a provisioned base container',
            bool(base.get('operation_id')) and base['data'].get('operation_id') == base['operation_id'], base['data'])
        _, lifecycle = contract.api(f'/api/v1/userapp/{app}/lifecycle')
        contract.lifecycle = lifecycle['data']['lifecycle_id']
        base_cid, base_mounts = contract.current()
        report['base_container_id'] = base_cid
        contract.wait_postgres(base_cid)
        contract.sql(base_cid, 'CREATE TABLE public.codex_contract_sentinel (id integer PRIMARY KEY, marker text NOT NULL)')
        contract.sql(base_cid, f"INSERT INTO public.codex_contract_sentinel VALUES (1, '{contract.marker}')")
        contract.verify_mounts(base_cid, base_mounts, 'base')
        directory = Path(tempfile.mkdtemp(prefix='prod-readiness-contract-'))
        artifact = make_artifact(repo, directory, release, contract.marker)
        payload = artifact.read_bytes()
        artifact_path = '/' + artifact.name
        downloads = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path != artifact_path:
                    self.send_error(404)
                    return
                downloads.append({'client': self.client_address[0], 'at': time.time()})
                self.send_response(200)
                self.send_header('Content-Type', 'application/zip')
                self.send_header('Content-Length', str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            def log_message(self, *_):
                pass
        server = http.server.ThreadingHTTPServer(('0.0.0.0', 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        artifact_url = f'http://{args.artifact_advertise_host}:{server.server_port}{artifact_path}'
        report['artifact'] = {'path': str(artifact), 'url': artifact_url, 'release_id': release,
                              'sha256': hashlib.sha256(payload).hexdigest(), 'downloads': downloads}
        _, response = contract.api(f'/api/v1/userapp/{app}/start', {
            'user_id': args.user_id, 'request_id': 'deploy-' + uuid.uuid4().hex,
            'lifecycle_id': contract.lifecycle, 'deploy_mode': 'pod',
            'url': artifact_url, 'sha256': report['artifact']['sha256'], 'release_id': release,
            'auto_execute_sql': False, 'env': {'PROJECT_ID': app}}, timeout=330)
        deployment = response.get('operation_id')
        contract.check('deploy response carries the accepted operation',
                       bool(deployment) and response['data'].get('operation_id') == deployment, response['data'])
        cold_cid, cold_mounts = contract.current()
        contract.check('cold deployment replaces the base physical container', cold_cid != base_cid,
            {'before': base_cid, 'after': cold_cid})
        pg_before = next(mount for mount in base_mounts if mount['Destination'] == '/home/user/data')
        pg_after = next(mount for mount in cold_mounts if mount['Destination'] == '/home/user/data')
        contract.check('cold deployment retains the original initialized PG mount', pg_before == pg_after,
            {'before': pg_before, 'after': pg_after})
        contract.verify_mounts(cold_cid, cold_mounts, 'cold-deployed')
        contract.wait('deployed static frontend is ready', contract.readiness, lambda data: data['ready'] is True)
        cid, identity = contract.verify_runtime('deploy', deployment)
        contract.check('real ZIP was fetched', bool(downloads), downloads)
        initial_downloads = len(downloads)
        contract.owner_recovery(cid, identity)
        contract.verify_runtime('owner-recovered', deployment)
        contract.check('owner recovery reuses the confirmed artifact', len(downloads) == initial_downloads, downloads)
        restart = contract.control('restart')
        ready = contract.wait('business ready after physical restart', contract.readiness, lambda data: data['ready'] is True)
        contract.check('readiness keeps the exact terminal restart receipt',
            (ready['container'].get('operation') or {}).get('operation_id') == restart
            and ready['container']['operation'].get('state') == 'succeeded', ready['container'])
        contract.verify_runtime('restarted', deployment)
        stop = contract.control('stop')
        contract.wait('Stop is Stopped with its original succeeded receipt', contract.readiness,
            lambda data: data['ready'] is False and data['container']['status'] == 'stopped'
                and (data['container'].get('operation') or {}).get('operation_id') == stop
                and data['container']['operation'].get('state') == 'succeeded')
        contract.api(f'/api/v1/userapp/{app}/start', {'user_id': args.user_id,
            'lifecycle_id': contract.lifecycle, 'request_id': 'resume-' + uuid.uuid4().hex}, timeout=330)
        contract.wait('explicit Start restores business readiness', contract.readiness, lambda data: data['ready'] is True)
        contract.verify_runtime('started-again', deployment)
        contract.check('compute restarts reuse the confirmed artifact', len(downloads) == initial_downloads, downloads)
        contract.check('source stayed stable throughout the run', source_identity(repo) == report['source'])
        contract.cleanup()
        report['success'] = True
    except Exception as error:
        report['error'] = f'{type(error).__name__}: {error}'
        report['traceback'] = traceback.format_exc()
        report['cleanup'] = {'failure_scene_preserved': True, 'volumes_removed': False}
        try:
            # Capture even when initial deployment failed before readiness.
            # Only exact own-app labels are followed; nothing is stopped here.
            for cid in run(['docker', 'ps', '-aq', '--no-trunc', '--filter', f'label=app-id={app}',
                            '--filter', 'label=managed-by=rcoder-app-manager', '--filter', 'label=service-type=user-app']).split():
                contract.inspect(cid)
        except Exception as collection_error:
            report['failure_capture_error'] = str(collection_error)
        print(report['error'], flush=True)
    finally:
        if server is not None:
            server.shutdown()
            server.server_close()
        args.report.write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
        print(f'report: {args.report}', flush=True)
    return 0 if report['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
