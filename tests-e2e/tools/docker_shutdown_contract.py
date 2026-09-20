"""Real SIGTERM while an owned UserApp create is held at a Docker API barrier.

No Cargo/build, no production hooks, no existing Compose stack. Reuses the crash
fixture's physical ownership checks and cleanup, but verifies graceful drain.
"""
from concurrent.futures import ThreadPoolExecutor
import argparse
import hashlib
import http.client as http_client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request
import urllib.error
import uuid
from docker_crash_contract import cleanup, db_operation, owned_builder_rows, wait_file
from turso_runtime_contract import REPO, command, http, isolated_config, service_config, wait_ready, parse_observer_output


def running(cid):
    return json.loads(command('docker', 'inspect', cid))[0]['State']['Running']


def messages(root):
    return '\n'.join(path.read_text(errors='replace') for path in (root / 'logs').rglob('*') if path.is_file())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image-metadata', type=Path, help='Optional frozen metadata; otherwise use E2E_TURSO_RUNTIME_IMAGE/BINARY_SHA256')
    parser.add_argument('--observer', type=Path, help='Existing same-schema observer; also supports E2E_USERAPP_OBSERVER_BINARY or the report observer directory')
    args = parser.parse_args()
    if args.image_metadata:
        metadata = json.loads(args.image_metadata.read_text())
    else:
        tag = os.environ['E2E_TURSO_RUNTIME_IMAGE']
        metadata = {'tag': tag, 'image_id': command('docker', 'image', 'inspect', tag, '--format', '{{.Id}}'),
                    'binary_sha256': os.environ['E2E_TURSO_BINARY_SHA256']}
    if len(metadata['binary_sha256']) != 64 or any(char not in '0123456789abcdef' for char in metadata['binary_sha256']):
        raise ValueError('Expected a frozen SHA256 for the RCoder binary')
    image = metadata['image_id']
    if command('docker', 'image', 'inspect', metadata['tag'], '--format', '{{.Id}}') != image:
        raise RuntimeError('Master image tag does not match frozen metadata')
    report = Path(os.environ.get('E2E_REPORT_DIR') or tempfile.mkdtemp(prefix='rcoder-sigterm-')).resolve()
    root = report / 'docker-shutdown'
    root.mkdir(parents=True, exist_ok=False)
    observer = root / 'userapp-db-observer'
    frozen_observer = report / 'observer/userapp-db-observer'
    observer_source = args.observer or (Path(os.environ['E2E_USERAPP_OBSERVER_BINARY'])
        if os.environ.get('E2E_USERAPP_OBSERVER_BINARY') else frozen_observer)
    if not observer_source.is_file():
        raise RuntimeError('Frozen same-schema observer is missing; supply --observer or E2E_USERAPP_OBSERVER_BINARY')
    observer.write_bytes(observer_source.read_bytes())
    observer.chmod(0o700)
    observer_sha = hashlib.sha256(observer.read_bytes()).hexdigest()
    run_id = os.environ.get('E2E_RUN_ID', uuid.uuid4().hex)
    case_id = os.environ.get('E2E_CASE_ID', uuid.uuid4().hex)
    receipt = {'root': str(root), 'run_id': run_id, 'case_id': case_id,
               'project': 'rcoder-crash-' + uuid.uuid4().hex[:16],
               'app_id': 'cr-' + case_id[:10] + '-term', 'user_id': 'shutdown-e2e'}
    assertions = []
    pool = ThreadPoolExecutor(max_workers=1)
    persistent = None

    def record(name, ok, detail=None):
        assertions.append({'name': name, 'ok': bool(ok), 'detail': detail})
        (root / 'assertions.json').write_text(json.dumps(assertions, indent=2))
        if not ok:
            raise RuntimeError(name)

    try:
        private = Path(tempfile.mkdtemp(prefix=receipt['project'] + '-config-')) / 'config.yml'
        private.write_text(isolated_config((REPO / 'docker/config.yml').read_text()))
        private.chmod(0o600)
        receipt['private_config'] = str(private)
        (root / 'ownership.json').write_text(json.dumps(receipt, indent=2))
        config = service_config(root, image, run_id, case_id, '/var/run/docker.sock', private)
        service = config['services']['rcoder']
        service['volumes'] = [mount for mount in service['volumes'] if mount['target'] != '/var/run/docker.sock']
        service['volumes'].append({'type': 'volume', 'source': 'fault-socket', 'target': '/proxy'})
        service['environment']['DOCKER_SOCKET_PATH'] = '/proxy/docker.sock'
        service['depends_on'] = {'docker-proxy': {'condition': 'service_healthy'}}
        for mount in service['volumes']:
            if mount.get('type') == 'bind' and not mount.get('read_only'):
                Path(mount['source']).mkdir(parents=True, exist_ok=True)
        control = root / 'control'
        control.mkdir()
        script = root / 'docker_fault_proxy.py'
        script.write_text(Path(__file__).with_name('docker_fault_proxy.py').read_text())
        labels = {'rcoder.e2e.run': run_id, 'rcoder.e2e.case': case_id}
        config['volumes'] = {'fault-socket': {'labels': labels}}
        config['services']['docker-proxy'] = {
            'image': image, 'pull_policy': 'never', 'entrypoint': ['python3', '/fault.py'], 'restart': 'no',
            'labels': labels,
            'environment': {'FAULT_APP_ID': receipt['app_id'], 'FAULT_USER_ID': receipt['user_id'],
                            'FAULT_PROJECT': receipt['project'], 'FAULT_MODE': 'before_create'},
            'volumes': [{'type': 'volume', 'source': 'fault-socket', 'target': '/proxy'},
                        {'type': 'bind', 'source': os.environ.get('DOCKER_SOCKET_PATH', '/var/run/docker.sock'), 'target': '/var/run/docker.sock'},
                        {'type': 'bind', 'source': str(script), 'target': '/fault.py', 'read_only': True},
                        {'type': 'bind', 'source': str(control), 'target': '/control'}],
            'healthcheck': {'test': ['CMD', 'python3', '-c', 'import socket;s=socket.socket(socket.AF_UNIX);s.connect("/proxy/docker.sock");s.close()'],
                            'interval': '1s', 'timeout': '2s', 'retries': 30},
        }
        (root / 'compose.json').write_text(json.dumps(config, indent=2))
        compose = ['docker', 'compose', '-p', receipt['project'], '-f', str(root / 'compose.json')]
        command(*compose, 'up', '-d', '--no-build', '--pull', 'never', timeout=120)
        base = 'http://' + command(*compose, 'port', 'rcoder', '8090')
        wait_ready(base)
        cid = command(*compose, 'ps', '-q', 'rcoder')
        record('Frozen master binary identity', command('docker', 'exec', cid, 'sha256sum', '/app/bin/rcoder').split()[0] == metadata['binary_sha256'])
        address = urllib.parse.urlsplit(base)
        persistent = socket.create_connection((address.hostname, address.port), timeout=10)
        persistent.sendall(b'GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n')
        response = http_client.HTTPResponse(persistent)
        response.begin()
        response.read()
        record('Existing HTTP connection permits keep-alive before shutdown', response.status == 200 and not response.will_close)
        future = pool.submit(http, base, '/api/v1/userapp/workspace', {'app_id': receipt['app_id']})
        wait_file(control / 'barrier.json', time.monotonic() + 90, cid)
        original = db_operation(base, receipt['app_id'], receipt['user_id'])
        receipt['lifecycle_id'] = original['lifecycle_id']
        (root / 'ownership.json').write_text(json.dumps(receipt, indent=2))
        record('Durable Running original operation held before remote create', original['state'] == 'Running' and not owned_builder_rows(receipt))
        new_app = receipt['app_id'].removesuffix('-term') + '-late'
        def late_identity_absent():
            try:
                response = urllib.request.urlopen(base + '/api/v1/userapp/' + new_app + '/operations/current', timeout=10)
            except urllib.error.HTTPError as response_error:
                response = response_error
            with response:
                body = json.load(response)
            return body.get('code') == 'ERR_APP_NOT_FOUND'
        record('Late request identifier is valid and initially absent', late_identity_absent())
        command('docker', 'kill', '--signal', 'TERM', cid)
        deadline = time.monotonic() + 10
        while 'received SIGTERM' not in messages(root) and time.monotonic() < deadline:
            time.sleep(0.05)
        record('Process handled real SIGTERM', 'received SIGTERM' in messages(root))
        payload = json.dumps({'app_id': new_app}).encode()
        request = b'POST /api/v1/userapp/workspace HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: ' + str(len(payload)).encode() + b'\r\nConnection: keep-alive\r\n\r\n' + payload
        rejected = False
        observation = ''
        try:
            persistent.sendall(request)
            persistent.settimeout(5)
            late = http_client.HTTPResponse(persistent)
            late.begin()
            body = late.read()
            observation = f'HTTP {late.status}'
            rejected = late.status >= 400 or json.loads(body).get('success') is False
        except (OSError, http_client.HTTPException):
            rejected = True
            observation = 'existing socket closed/reset; no reconnect attempted'
        record('Old keep-alive cannot admit new UserApp operation', rejected, observation)
        time.sleep(1)
        record('Shutdown waits for accepted operation before store close', running(cid) and 'RCoder graceful shutdown completed' not in messages(root))
        (control / 'release').write_text('discard')
        wait_file(control / 'barrier-settled', time.monotonic() + 10)
        try:
            future.result(timeout=70)
        except (OSError, RuntimeError):
            pass
        deadline = time.monotonic() + 75
        while running(cid) and time.monotonic() < deadline:
            time.sleep(0.2)
        stopped = json.loads(command('docker', 'inspect', cid))[0]['State']
        log = messages(root)
        record('Store drain and graceful process exit confirmed', not stopped['Running'] and stopped['ExitCode'] == 0 and 'RCoder graceful shutdown completed' in log,
               {'exit_code': stopped['ExitCode']})
        observed = subprocess.run([str(observer), str((root / 'data/userapp.turso.db').resolve())], capture_output=True, text=True, timeout=60)
        record('Offline same-engine observer acquires released store lock', observed.returncode == 0, {'exit_code': observed.returncode, 'diagnostic': observed.stderr.strip()[-500:]})
        lifecycles, operations = parse_observer_output(observed.stdout)
        persisted = [operation for operation in operations if operation.get('operation_id') == original['operation_id']]
        record('Original protection persisted before any restart recovery', len(persisted) == 1 and persisted[0]['state'] == 'RecoveryRequired' and persisted[0]['lifecycle_id'] == original['lifecycle_id'])
        record('Offline snapshot has no rejected application identity', not any(row.get('app_id') == new_app for row in lifecycles + operations))
        command('docker', 'start', cid)
        base = 'http://' + command(*compose, 'port', 'rcoder', '8090')
        wait_ready(base)
        restored = db_operation(base, receipt['app_id'], receipt['user_id'])
        record('Restart retains original uncertain identity and protection', restored['operation_id'] == original['operation_id'] and restored['lifecycle_id'] == original['lifecycle_id'] and restored['state'] == 'RecoveryRequired')
        record('Rejected keep-alive left no durable application identity or operations', late_identity_absent())
        attempts = (control / 'create-attempts.jsonl').read_text().splitlines()
        record('No physical create or replay after signal', len(attempts) == 1 and not owned_builder_rows(receipt))
        (root / 'evidence.json').write_text(json.dumps({'image': metadata, 'original': original, 'restored': restored,
            'keepalive': observation, 'stopped': stopped, 'create_attempts': len(attempts), 'before_restart': persisted[0], 'observer_sha256': observer_sha}, indent=2))
    except Exception as error:
        assertions.append({'name': 'SIGTERM fixture execution', 'ok': False, 'detail': type(error).__name__ + ': ' + str(error)[:500]})
        (root / 'assertions.json').write_text(json.dumps(assertions, indent=2))
    finally:
        if persistent:
            persistent.close()
        if (root / 'control/barrier.json').exists():
            (root / 'control/release').write_text('discard')
        pool.shutdown(wait=True, cancel_futures=True)
        try:
            result = cleanup(root, run_id, case_id)
            (root / 'cleanup.json').write_text(json.dumps(result, indent=2))
            assertions.append({'name': 'Owned SIGTERM fixture cleanup', 'ok': True})
        except Exception as error:
            assertions.append({'name': 'Owned SIGTERM fixture cleanup', 'ok': False, 'detail': type(error).__name__ + ': ' + str(error)[:300]})
        (root / 'assertions.json').write_text(json.dumps(assertions, indent=2))
    print(root)
    return int(any(not item['ok'] for item in assertions))


if __name__ == '__main__':
    raise SystemExit(main())
