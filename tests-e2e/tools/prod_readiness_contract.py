#!/usr/bin/env python3
"""Real, no-LLM prod contract with a private Docker RCoder control plane.

Python >= 3.11. A frozen paired build receipt is required; no builds,
shared service updates, database cleanup or volume
deletion. The fixture follows workspace-manifest/tests/fixtures/lock_v1.toml,
workspace-manifest/src/release_lock.rs and app-cli's static_content_dir contract.
An empty Start first provisions the base container; real business PostgreSQL
SELECT 1 is a fixture prerequisite before cold replacement on the same mounts.
Success removes only captured containers and the owned control plane/network.
On failure the strict parent uses ownership.json for precise fallback cleanup;
unknown creation cannot be treated as cleaned. All volumes and data remain.
The ZIP and report remain on disk. Missing evidence is failure, never a skip.

Example: python3 tests-e2e/tools/prod_readiness_contract.py \
  --url http://127.0.0.1:18080 --runtime-image rcoder-userapp-contract:local \
  --controller-container <full-owned-controller-id> \
  --build-receipt /tmp/frozen-paired-build.json --report /tmp/prod-contract.json
"""
import argparse
import hashlib
import http.server
import json
import os
import re
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


REQUIRED_STEPS = frozenset({
    'paired build inputs verified',
    'owned isolated controller prepared',
    'isolated controller and project network removed with data retained',
    'cold deployment declaration matches runtime receipt',
    'cold native and deployment generations are separate',
    'hot new artifact B is committed without container replacement',
    'readiness observes the admitted restarting operation',
    'readiness keeps the exact terminal restart receipt',
    'Stop is Stopped with its original succeeded receipt',
    'explicit Start restores business readiness',
    'supervisord restores a new owner in the same container',
    'owner recovery keeps exact physical container',
    'invalid artifact retains concrete failure and accepted operation',
    'invalid artifact result belongs to accepted operation',
    'source stayed stable throughout the run',
    'owned containers removed and volumes retained',
})


def source_identity(repo, input_manifest=None):
    """Match strict launcher input hashing, including snapshots without .git."""
    manifest = input_manifest or os.environ.get('E2E_INPUT_MANIFEST')
    if manifest:
        entries = json.loads(Path(manifest).read_text())
        if not isinstance(entries, dict) or not entries:
            raise ValueError('frozen input manifest must be a nonempty mapping')
        names = sorted(entries)
        head = os.environ.get('E2E_ORIGIN_HEAD')
        if not head:
            raise ValueError('frozen snapshot requires E2E_ORIGIN_HEAD')
    else:
        names = sorted(set(os.fsdecode(name) for name in subprocess.check_output(
            ['git', '-C', str(repo), 'ls-files', '-z', '--cached', '--others', '--exclude-standard']
        ).split(b'\0') if name))
        head = subprocess.check_output(['git', '-C', str(repo), 'rev-parse', 'HEAD']).decode().strip()
    digest = hashlib.sha256()
    for name in names:
        relative = Path(name)
        if relative.is_absolute() or '..' in relative.parts:
            raise ValueError('input manifest path escapes source root')
        path = repo / relative
        digest.update(name.encode() + b'\0')
        if path.is_symlink():
            digest.update(b'<link>' + os.readlink(path).encode())
        elif path.is_file():
            digest.update(path.read_bytes())
        else:
            digest.update(b'<missing>')
    return {'origin_head': head, 'worktree_sha256': digest.hexdigest()}


def require_report_complete(report, owned_controller=True):
    checks = report.get('checks', [])
    names = {row.get('name') for row in checks if row.get('ok') is True}
    required = REQUIRED_STEPS if owned_controller else REQUIRED_STEPS - {
        'owned isolated controller prepared',
        'isolated controller and project network removed with data retained',
    }
    missing = required - names
    if missing:
        raise ValueError('missing required steps: ' + ', '.join(sorted(missing)))
    if any(row.get('ok') is not True for row in checks):
        raise ValueError('report contains failed checks')
    cleanup = report.get('cleanup') or {}
    if cleanup.get('captured_containers_removed') is not True or cleanup.get('volumes_removed') is not False:
        raise ValueError('owned cleanup not confirmed')


def validate_receipt(receipt, source):
    import re
    if receipt.get('version') != 1 or receipt.get('source') != source:
        raise ValueError('build receipt does not match frozen source inputs')
    for role, key in (('rcoder', 'binary_sha256'), ('runtime', 'app_cli_sha256')):
        record = receipt.get(role) or {}
        if not re.fullmatch(r'sha256:[0-9a-f]{64}', str(record.get('image_id', ''))):
            raise ValueError('missing immutable ' + role + ' image identity')
        if not re.fullmatch(r'[0-9a-f]{64}', str(record.get(key, ''))):
            raise ValueError('missing build-time ' + role + ' binary identity')
    return receipt



def persist_ownership(root, receipt):
    path = Path(root) / 'ownership.json'
    temporary = path.with_suffix('.tmp')
    with temporary.open('w') as handle:
        json.dump(receipt, handle, indent=2)
        handle.flush()
        os.fsync(handle.fileno())
    temporary.replace(path)


def validate_controller(row, receipt):
    from turso_runtime_contract import validate_owned
    validate_owned(row, receipt)
    if (row['Image'] != receipt['rcoder_image_id'] or row['Name'] != '/' + receipt['project'] + '-rcoder-1'
            or receipt.get('controller_id') not in (None, row['Id'])):
        raise ValueError('isolated controller physical identity mismatch')
    mounts = {mount['Destination']: mount for mount in row['Mounts']}
    for folder in ('data', 'logs', 'project_workspace', 'computer-project-workspace', 'userapp-workspace', 'app-workspace'):
        mount = mounts.get('/app/' + folder) or {}
        if mount.get('Type') != 'bind' or Path(mount.get('Source', '')).resolve() != Path(receipt['root']) / folder:
            raise ValueError('isolated controller writable mount is foreign')


def prepare_controller(root, repo, build, source, run_id, case_id, app_id):
    """Only a frozen, validated build may create a private control plane."""
    from turso_runtime_contract import isolated_config, service_config, wait_ready
    validate_receipt(build, source)  # Before directory/ownership or Docker side effects.
    root = Path(root).resolve()
    if (root / 'ownership.json').exists():
        raise ValueError('owned fixture already exists; preserve the previous run')
    root.mkdir(parents=True, exist_ok=True)
    receipt = {'version': 1, 'root': str(root), 'run_id': run_id, 'case_id': case_id,
               'project': 'rcoder-prod-' + uuid.uuid4().hex[:16], 'app_id': app_id,
               'rcoder_image_id': build['rcoder']['image_id'], 'runtime_image_id': build['runtime']['image_id'],
               'controller_state': 'not_started', 'application_state': 'not_started',
               'controller_id': None, 'application_containers': {}, 'source': source}
    source_config = isolated_config((repo / 'docker/config.yml').read_text())
    # Service image references remain unused in this no-agent fixture. The one
    # production runtime is selected by the same platform env used by Start.
    source_config = source_config.replace('sha256:local-development', build['runtime']['image_id'])
    config_path = root / 'config.yml'
    config_path.write_text(source_config)
    config_path.chmod(0o600)
    config = service_config(root, build['rcoder']['image_id'], run_id, case_id,
                            os.environ.get('DOCKER_SOCKET_PATH', '/var/run/docker.sock'), config_path)
    service = config['services']['rcoder']
    service['environment'].update(RCODER_RUNTIME_IMAGE_DIGEST=build['runtime']['image_id'],
                                  COMPOSE_PROJECT_NAME=receipt['project'], DOCKER_NETWORK_BASE_NAME='agent-network')
    service['networks'] = ['agent-network']
    config['networks'] = {'agent-network': {'labels': {'rcoder.e2e.run': run_id, 'rcoder.e2e.case': case_id}}}
    for mount in service['volumes']:
        if mount.get('type') == 'bind' and not mount.get('read_only') and mount['target'] != '/var/run/docker.sock':
            Path(mount['source']).mkdir(parents=True, exist_ok=True)
    (root / 'compose.json').write_text(json.dumps(config, indent=2))
    persist_ownership(root, receipt)
    compose = ['docker', 'compose', '-p', receipt['project'], '-f', str(root / 'compose.json')]
    receipt['controller_state'] = 'unknown'
    persist_ownership(root, receipt)
    try:
        run(compose + ['up', '-d', '--no-build', '--pull', 'never'], timeout=120)
        ids = run(['docker', 'ps', '-aq', '--no-trunc', '--filter', 'label=com.docker.compose.project=' + receipt['project']]).split()
        if len(ids) != 1:
            raise RuntimeError('isolated controller creation has ambiguous physical results')
        row = json.loads(run(['docker', 'inspect', ids[0]]))[0]
        validate_controller(row, receipt)
        receipt.update(controller_state='completed', controller_id=row['Id'])
        address = run(compose + ['port', 'rcoder', '8090']).strip()
        receipt['base'] = 'http://' + address
        persist_ownership(root, receipt)
        wait_ready(receipt['base'])
        return receipt
    except Exception as error:
        receipt['prepare_error'] = type(error).__name__ + ': ' + str(error)
        persist_ownership(root, receipt)
        raise


def cleanup(root, run_id, case_id, existing_ids=()):
    """Parent fallback: exact captured objects, no volume deletion or unknown success."""
    root = Path(root).resolve()
    receipt = json.loads((root / 'ownership.json').read_text())
    if (receipt.get('version') != 1 or receipt.get('run_id') != run_id or receipt.get('case_id') != case_id
            or receipt.get('root') != str(root) or not re.fullmatch(r'rcoder-prod-[0-9a-f]{16}', receipt.get('project', ''))):
        raise ValueError('prod cleanup ownership receipt mismatch')
    if receipt['controller_state'] == 'not_started':
        return {'ok': True, 'creation_started': False, 'volumes_removed': False, 'retained_data': str(root)}
    ids = run(['docker', 'ps', '-aq', '--no-trunc', '--filter', 'label=com.docker.compose.project=' + receipt['project']]).split()
    rows = json.loads(run(['docker', 'inspect', *ids])) if ids else []
    if len(rows) > 1:
        raise ValueError('unexpected isolated control plane container set')
    for row in rows:
        if row['Id'] in existing_ids:
            raise ValueError('refusing preexisting controller')
        validate_controller(row, receipt)
    if receipt['controller_state'] == 'unknown' and not rows:
        return {'ok': False, 'detail': 'controller creation outcome unknown; empty inventory is not completion',
                'volumes_removed': False, 'retained_data': str(root)}
    if rows:
        row = rows[0]
        receipt.update(controller_state='completed', controller_id=row['Id'])
        persist_ownership(root, receipt)
        # Stop the one captured writer before examining application resources.
        run(['docker', 'stop', '-t', '10', row['Id']], timeout=30)
    all_ids = set(run(['docker', 'ps', '-aq', '--no-trunc']).split())
    captured = receipt['application_containers']
    app_ids = set(run(['docker', 'ps', '-aq', '--no-trunc', '--filter', 'label=app-id=' + receipt['app_id']]).split())
    if app_ids - set(captured) or (receipt['application_state'] == 'unknown' and not captured):
        return {'ok': False, 'detail': 'application creation outcome needs exact physical inspection',
                'volumes_removed': False, 'retained_data': str(root)}
    app_rows = []
    for cid, frozen in captured.items():
        if cid not in all_ids:
            continue
        if cid in existing_ids:
            raise ValueError('refusing preexisting application container')
        row = json.loads(run(['docker', 'inspect', cid]))[0]
        labels = row['Config'].get('Labels') or {}
        mounts = [{key: mount.get(key) for key in ('Type', 'Name', 'Source', 'Destination')} for mount in row['Mounts']]
        environment = dict(item.split('=', 1) for item in row['Config']['Env'] if '=' in item)
        if (row['Id'] != cid or row['Name'].lstrip('/') != frozen['name']
                or row['Image'] != receipt['runtime_image_id'] or mounts != frozen['mounts']
                or environment.get('APP_ID') != receipt['app_id'] or environment.get('PROJECT_ID') != receipt['app_id']
                or labels.get('app-id') != receipt['app_id'] or labels.get('service-type') != 'user-app'
                or labels.get('managed-by') != 'rcoder-app-manager'):
            raise ValueError('application container no longer matches captured ownership')
        app_rows.append(row)
    for row in app_rows:
        run(['docker', 'rm', '-f', row['Id']])
    for row in rows:
        run(['docker', 'rm', row['Id']])
    network_ids = run(['docker', 'network', 'ls', '-q', '--filter', 'label=com.docker.compose.project=' + receipt['project']]).split()
    networks = json.loads(run(['docker', 'network', 'inspect', *network_ids])) if network_ids else []
    for network in networks:
        labels = network.get('Labels') or {}
        if (network['Name'] != receipt['project'] + '_agent-network'
                or labels.get('rcoder.e2e.run') != run_id or labels.get('rcoder.e2e.case') != case_id
                or network.get('Containers')):
            raise ValueError('project network identity changed or still has consumers')
    for network in networks:
        run(['docker', 'network', 'rm', network['Id']])
    receipt['cleanup_complete'] = True
    persist_ownership(root, receipt)
    return {'ok': True, 'captured_containers_removed': True, 'network_removed': True,
            'volumes_removed': False, 'retained_data': str(root)}


def declaration_matches(raw_environment, expected):
    """Require each actual cold declaration exactly once, not dict first-wins."""
    for key, value in expected.items():
        matches = [item.split('=', 1)[1] for item in raw_environment if item.startswith(key + '=')]
        if matches != [value]:
            return False
    return True


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
        self.build_receipt = None
        self.owned_controller = None
        self.fixture_root = None

    def check(self, name, ok, detail=None):
        self.report['checks'].append({'name': name, 'ok': bool(ok), 'detail': detail})
        print(name, 'PASS' if ok else 'FAIL', flush=True)
        if not ok:
            raise RuntimeError(f'{name}: {detail}')

    def api_raw(self, path, body=None, timeout=20):
        headers = {'content-type': 'application/json'}
        if self.args.api_key:
            headers['x-api-key'] = self.args.api_key
        request = urllib.request.Request(self.args.url.rstrip('/') + path,
            data=None if body is None else json.dumps(body).encode(), headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read()
        return status, json.loads(raw)

    def api(self, path, body=None, timeout=20):
        status, envelope = self.api_raw(path, body, timeout)
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
        environment = dict(item.split('=', 1) for item in value['Config']['Env'] if '=' in item)
        if environment.get('APP_ID') != self.app or environment.get('PROJECT_ID') != self.app:
            raise ValueError('application env identity differs from the owned fixture')
        if value['Image'] != self.report['expected_image_id']:
            raise RuntimeError(f'container {cid} is not running the requested test image')
        mounts = [{key: mount.get(key) for key in ('Type', 'Name', 'Source', 'Destination')}
                  for mount in value['Mounts']]
        self.containers[cid] = {'id': cid, 'name': value['Name'].lstrip('/'), 'image_id': value['Image'],
            'image_ref': value['Config']['Image'], 'mounts': mounts}
        self.report['containers'] = list(self.containers.values())
        if self.owned_controller is not None:
            self.owned_controller['application_containers'] = dict(self.containers)
            self.owned_controller['application_state'] = 'completed'
            persist_ownership(self.fixture_root, self.owned_controller)
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

    def verify_runtime(self, phase, expected_deployment, expected_generation=None, marker=None, release=None):
        expected_generation = expected_generation or expected_deployment
        marker = marker or self.marker
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
                   and generation == expected_generation and identity['deployment_generation_id'] == generation,
                   {'operation': operation, 'identity': identity})
        self.check(('cold native and deployment generations are separate' if phase == 'deploy' else phase + ': native and deployment generations are separate'),
                   native.get('phase') == 'ready' and bool(native.get('generation'))
                   and native['generation'] != generation, native)
        self.check(phase + ': static HTML reachable',
                   marker in self.execute(cid, 'curl', '-fsS', '--max-time', '8', 'http://127.0.0.1:9080/'))
        self.check(phase + ': static JS reachable',
                   marker in self.execute(cid, 'curl', '-fsS', '--max-time', '8', 'http://127.0.0.1:9080/app.js'))
        if release is not None:
            self.check(phase + ': exact artifact release committed', operation.get('artifact_release_id') == release
                       and operation.get('request_release_id') == release
                       and operation.get('phase') == 'running' and operation.get('deploy_stage') == 'succeeded', operation)
        self.check(phase + ': app-cli matches frozen build',
                   self.execute(cid, 'sha256sum', '/usr/local/bin/app-cli').split()[0] == self.build_receipt['runtime']['app_cli_sha256'])
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
        # A retained Linux pidfd pins the captured process across PID reuse.
        # Recheck live management identity and native ownership before signaling.
        code = r'''import fcntl,json,os,select,signal,sys,urllib.request
from pathlib import Path
pid, expected_instance, expected_app = int(sys.argv[1]), sys.argv[2], sys.argv[3]
assert pid > 1
pidfd = os.pidfd_open(pid)
try:
    proc = Path('/proc', str(pid))
    argv = proc.joinpath('cmdline').read_bytes().split(b"\0")
    assert Path(os.fsdecode(argv[0])).name == 'app-cli' and argv[1] == b'serve'
    with urllib.request.urlopen('http://127.0.0.1:3010/v1/runtime/identity', timeout=3) as response:
        envelope = json.load(response)
    identity = envelope['data']
    assert envelope['code'] == 'OK' and envelope['success'] is True
    assert identity['runtime_instance_id'] == expected_instance and identity['application_id'] == expected_app
    roots = set()
    for handle in proc.joinpath('fd').iterdir():
        try:
            target = Path(os.readlink(handle))
        except FileNotFoundError:
            continue
        if target.is_absolute() and target.name == 'owner.lock':
            roots.add(target.parent)
    assert len(roots) == 1, 'captured owner native lock root is ambiguous'
    scope = roots.pop()
    discovery = json.loads(scope.joinpath('supervisor.json').read_text())
    snapshot = discovery['snapshot']
    assert snapshot['binding'] == {'component': 'app-cli', 'resource': identity['source_root']}
    generation = json.loads(scope.joinpath('work', snapshot['generation'], 'generation.json').read_text())
    assert generation['worker_pid'] == pid and generation['supervisor'] == discovery['instance']
    assert generation['phase'] == 'Running'
    for lock in (scope/'owner.lock', scope/'work'/generation['id']/'generation.lock'):
        stat = lock.stat()
        held = False
        for handle in proc.joinpath('fd').iterdir():
            try:
                fd = handle.stat()
                held |= (fd.st_dev, fd.st_ino) == (stat.st_dev, stat.st_ino)
            except FileNotFoundError:
                continue
        assert held, 'captured process does not hold native lock'
        with lock.open('rb') as handle:
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise AssertionError('native lock is not currently held')
    assert json.loads(scope.joinpath('supervisor.json').read_text())['instance'] == discovery['instance']
    signal.pidfd_send_signal(pidfd, signal.SIGKILL)
    assert select.select([pidfd], [], [], 10)[0], 'captured owner did not exit'
finally:
    os.close(pidfd)
'''
        self.execute(cid, 'python3', '-c', code, pid, original_identity['runtime_instance_id'], self.app)
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
        self.check('owned containers removed and volumes retained', True, self.report['cleanup'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', help='optional existing isolated loopback controller; requires --controller-container')
    parser.add_argument('--runtime-image',
                        help='already built local image configured on the isolated RCoder')
    parser.add_argument('--artifact-advertise-host', default='host.docker.internal')
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--source-dir', default=Path(os.environ.get('E2E_SOURCE_ROOT') or Path(__file__).resolve().parents[2]), type=Path)
    parser.add_argument('--build-receipt', required=True, type=Path)
    parser.add_argument('--controller-container', help='captured isolated RCoder full container ID')
    parser.add_argument('--user-id', default='prod-contract-user')
    parser.add_argument('--api-key', default=os.environ.get('RCODER_E2E_API_KEY'))
    args = parser.parse_args()
    if bool(args.url) != bool(args.controller_container):
        parser.error('--url and --controller-container must be supplied together')
    if args.url:
        parsed = urllib.parse.urlsplit(args.url)
        if (parsed.scheme not in ('http', 'https') or parsed.hostname not in ('127.0.0.1', 'localhost', '::1')
                or parsed.username or parsed.password or parsed.query or parsed.fragment
                or parsed.path not in ('', '/') or parsed.port is None):
            parser.error('--url must be an explicit isolated loopback endpoint')
    if args.report.exists():
        parser.error('--report already exists; preserve previous evidence and choose a new path')
    args.report.parent.mkdir(parents=True, exist_ok=True)
    repo = args.source_dir.resolve()
    app, release = 'prc' + uuid.uuid4().hex[:16], uuid.uuid4().hex
    report = {'success': False, 'app_id': app, 'checks': [], 'containers': [], 'source': source_identity(repo)}
    contract = Contract(args, report, app)
    server = None
    try:
        contract.build_receipt = validate_receipt(json.loads(args.build_receipt.read_text()), report['source'])
        if not args.controller_container:
            contract.fixture_root = args.report.parent.resolve()
            run_id = os.environ.get('E2E_RUN_ID') or uuid.uuid4().hex
            case_id = os.environ.get('E2E_CASE_ID') or uuid.uuid4().hex
            contract.owned_controller = prepare_controller(contract.fixture_root, repo, contract.build_receipt,
                                                           report['source'], run_id, case_id, app)
            args.controller_container = contract.owned_controller['controller_id']
            args.url = contract.owned_controller['base']
        args.runtime_image = args.runtime_image or contract.build_receipt['runtime']['image_id']
        parsed = urllib.parse.urlsplit(args.url)
        controller = json.loads(run(['docker', 'inspect', args.controller_container]))[0]
        labels = controller['Config'].get('Labels') or {}
        if controller['Id'] != args.controller_container or not controller['State']['Running']:
            raise ValueError('captured controller is not the running physical instance')
        if os.environ.get('E2E_STRICT') == '1' and (labels.get('rcoder.e2e.run') != os.environ.get('E2E_RUN_ID')
                or labels.get('rcoder.e2e.case') != os.environ.get('E2E_CASE_ID')):
            raise ValueError('controller does not belong to strict case')
        ports = controller['NetworkSettings'].get('Ports') or {}
        matches_url = any(binding.get('HostPort') == str(parsed.port) and binding.get('HostIp') in ('127.0.0.1', '::1')
                          for bindings in ports.values() for binding in (bindings or []))
        if not matches_url:
            raise ValueError('loopback URL is not bound to captured controller')
        image = json.loads(run(['docker', 'image', 'inspect', args.runtime_image]))[0]
        expected = contract.build_receipt
        controller_hash = run(['docker', 'exec', args.controller_container, 'sha256sum', '/app/bin/rcoder']).split()[0]
        contract.check('paired build inputs verified', controller['Image'] == expected['rcoder']['image_id']
                       and controller_hash == expected['rcoder']['binary_sha256']
                       and image['Id'] == expected['runtime']['image_id'], {'source': report['source'], 'build': expected})
        report['expected_image_id'] = expected['runtime']['image_id']
        report['runtime_image'] = args.runtime_image
        if contract.owned_controller is not None:
            contract.check('owned isolated controller prepared', True,
                           {'id': args.controller_container, 'project': contract.owned_controller['project']})
        contract.check('no existing resource has this generated app identity',
            not run(['docker', 'ps', '-aq', '--filter', f'label=app-id={app}']).strip())
        if contract.owned_controller is not None:
            contract.owned_controller['application_state'] = 'unknown'
            persist_ownership(contract.fixture_root, contract.owned_controller)
        _, base = contract.api(f'/api/v1/userapp/{app}/start', {
            'user_id': args.user_id, 'request_id': 'base-' + uuid.uuid4().hex,
            'env': {'PROJECT_ID': app}}, timeout=330)
        contract.check('empty Start creates a provisioned base container',
            bool(base.get('operation_id')) and base['data'].get('operation_id') == base['operation_id'], base['data'])
        _, lifecycle = contract.api(f'/api/v1/userapp/{app}/lifecycle')
        contract.lifecycle = lifecycle['data']['lifecycle_id']
        base_cid, base_mounts = contract.current()
        report['base_container_id'] = base_cid
        contract.check('base runtime app-cli matches frozen build',
                       contract.execute(base_cid, 'sha256sum', '/usr/local/bin/app-cli').split()[0]
                       == contract.build_receipt['runtime']['app_cli_sha256'])
        contract.wait_postgres(base_cid)
        contract.sql(base_cid, 'CREATE TABLE public.codex_contract_sentinel (id integer PRIMARY KEY, marker text NOT NULL)')
        contract.sql(base_cid, f"INSERT INTO public.codex_contract_sentinel VALUES (1, '{contract.marker}')")
        contract.verify_mounts(base_cid, base_mounts, 'base')
        directory = Path(tempfile.mkdtemp(prefix='prod-readiness-contract-'))
        artifact = make_artifact(repo, directory, release, contract.marker)
        payload = artifact.read_bytes()
        artifact_path = '/' + artifact.name
        release_b = uuid.uuid4().hex
        marker_b = contract.marker + '_B'
        artifact_b = make_artifact(repo, directory, release_b, marker_b)
        payload_b = artifact_b.read_bytes()
        paths = {artifact_path: payload, '/' + artifact_b.name: payload_b, '/invalid.zip': b'not a ZIP archive'}
        downloads = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path not in paths:
                    self.send_error(404)
                    return
                downloads.append({'path': self.path, 'client': self.client_address[0], 'at': time.time()})
                content = paths[self.path]
                self.send_response(200)
                self.send_header('Content-Type', 'application/zip')
                self.send_header('Content-Length', str(len(content)))
                self.end_headers()
                self.wfile.write(content)
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
        cid, identity = contract.verify_runtime('deploy', deployment, release=release)
        expected_env = {'APP_DEPLOY_OPERATION_ID': deployment, 'APP_DEPLOY_GENERATION_ID': deployment,
                        'APP_RELEASE_ID': release, 'APP_DEPLOY_URL': artifact_url, 'APP_DEPLOY_SHA256': report['artifact']['sha256']}
        raw_cold_env = contract.inspect(cid)['Config']['Env']
        contract.check('cold deployment declaration matches runtime receipt',
                       declaration_matches(raw_cold_env, expected_env), expected_env)
        contract.check('real ZIP was fetched', bool(downloads), downloads)
        initial_downloads = len(downloads)
        contract.owner_recovery(cid, identity)
        contract.verify_runtime('owner-recovered', deployment)
        contract.check('owner recovery reuses the confirmed artifact', len(downloads) == initial_downloads, downloads)
        hot_url = f'http://{args.artifact_advertise_host}:{server.server_port}/{artifact_b.name}'
        _, hot = contract.api(f'/api/v1/userapp/{app}/start', {
            'user_id': args.user_id, 'request_id': 'hot-' + uuid.uuid4().hex,
            'lifecycle_id': contract.lifecycle, 'deploy_mode': 'hot', 'url': hot_url,
            'sha256': hashlib.sha256(payload_b).hexdigest(), 'release_id': release_b,
            'auto_execute_sql': False}, timeout=330)
        hot_operation = hot.get('operation_id')
        contract.check('hot response carries accepted operation', bool(hot_operation)
                       and hot['data'].get('operation_id') == hot_operation, hot['data'])
        contract.wait('hot deployment B becomes business ready', contract.readiness, lambda data: data['ready'] is True)
        hot_cid, _ = contract.verify_runtime('hot', hot_operation, deployment, marker_b, release_b)
        contract.check('hot new artifact B is committed without container replacement', hot_cid == cid
                       and any(row['path'] == '/' + artifact_b.name for row in downloads))
        initial_downloads = len(downloads)
        restart = contract.control('restart')
        ready = contract.wait('business ready after physical restart', contract.readiness, lambda data: data['ready'] is True)
        contract.check('readiness keeps the exact terminal restart receipt',
            (ready['container'].get('operation') or {}).get('operation_id') == restart
            and ready['container']['operation'].get('state') == 'succeeded', ready['container'])
        contract.verify_runtime('restarted', hot_operation, deployment, marker_b, release_b)
        stop = contract.control('stop')
        contract.wait('Stop is Stopped with its original succeeded receipt', contract.readiness,
            lambda data: data['ready'] is False and data['container']['status'] == 'stopped'
                and (data['container'].get('operation') or {}).get('operation_id') == stop
                and data['container']['operation'].get('state') == 'succeeded')
        contract.api(f'/api/v1/userapp/{app}/start', {'user_id': args.user_id,
            'lifecycle_id': contract.lifecycle, 'request_id': 'resume-' + uuid.uuid4().hex}, timeout=330)
        contract.wait('explicit Start restores business readiness', contract.readiness, lambda data: data['ready'] is True)
        contract.verify_runtime('started-again', hot_operation, deployment, marker_b, release_b)
        contract.check('compute restarts reuse the confirmed artifact', len(downloads) == initial_downloads, downloads)
        invalid_release = uuid.uuid4().hex
        invalid_url = f'http://{args.artifact_advertise_host}:{server.server_port}/invalid.zip'
        status, failed = contract.api_raw(f'/api/v1/userapp/{app}/start', {
            'user_id': args.user_id, 'request_id': 'bad-artifact-' + uuid.uuid4().hex,
            'lifecycle_id': contract.lifecycle, 'deploy_mode': 'hot', 'url': invalid_url,
            'sha256': hashlib.sha256(paths['/invalid.zip']).hexdigest(), 'release_id': invalid_release,
            'auto_execute_sql': False}, timeout=330)
        failed_id = failed.get('operation_id')
        contract.check('invalid artifact retains concrete failure and accepted operation', status >= 400
                       and failed.get('success') is False and bool(failed_id)
                       and bool(failed.get('message')) and 'underlying cause' not in failed['message']
                       and failed['message'] != 'Backend service failed', failed)
        _, failure_observation = contract.api(f'/api/v1/userapp/{app}/operations/{urllib.parse.quote(failed_id, safe="")}')
        contract.check('invalid artifact result belongs to accepted operation',
                       failure_observation['data'].get('operation_id') == failed_id
                       and failure_observation['data'].get('app_id') == app
                       and failure_observation['data'].get('state') == 'Failed'
                       and bool(failure_observation['data'].get('error_message')), failure_observation['data'])
        contract.check('source stayed stable throughout the run', source_identity(repo) == report['source'])
        if contract.owned_controller is not None:
            outcome = cleanup(contract.fixture_root, contract.owned_controller['run_id'], contract.owned_controller['case_id'])
            report['controller_cleanup'] = outcome
            report['cleanup'] = outcome
            contract.check('owned containers removed and volumes retained',
                           outcome.get('ok') is True and outcome.get('captured_containers_removed') is True
                           and outcome.get('volumes_removed') is False, outcome)
            contract.check('isolated controller and project network removed with data retained',
                           outcome.get('ok') is True and outcome.get('network_removed') is True
                           and outcome.get('volumes_removed') is False, outcome)
        else:
            contract.cleanup()
        report['full_owned_acceptance'] = contract.owned_controller is not None
        if contract.owned_controller is None:
            report['compatibility_mode'] = 'external-controller-core-only'
        require_report_complete(report, owned_controller=contract.owned_controller is not None)
        report['success'] = True
    except Exception as error:
        report['error'] = f'{type(error).__name__}: {error}'
        report['traceback'] = traceback.format_exc()
        if not report.get('cleanup', {}).get('captured_containers_removed'):
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
