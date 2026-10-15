"""Real rcoder SIGKILL at two gated Docker API boundaries, no production hooks."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import re
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import urllib.request
import uuid
from turso_runtime_contract import REPO, command, http, wait_ready, service_config, isolated_config, remove_private_config

MODES = ('before_create', 'after_start')


def wait_file(path, deadline, process_id=None):
    while not path.exists():
        if time.monotonic() >= deadline:
            raise RuntimeError('fault barrier deadline exceeded')
        if process_id and json.loads(command('docker', 'inspect', process_id))[0]['State']['Status'] != 'running':
            raise RuntimeError('rcoder stopped before the fault barrier')
        time.sleep(0.02)


def db_operation(base, app, user):
    # T4：运行中观察走 HTTP（SQLite 引擎不读 Turso 活库；跨引擎 mmap 读
    # 已被容器 SIGBUS 实测排除）。operations/current 返回在途/最近权威操作。
    request = base + '/api/v1/userapp/' + app + '/operations/current?user_id=' + user
    with urllib.request.urlopen(request, timeout=10) as response:
        body = json.load(response)
    operations = body.get('data') if body.get('code') == '0000' else None
    # operations/current 返回全部在途槽位；故障窗口内恰一条在途操作
    if not isinstance(operations, list) or len(operations) != 1:
        raise RuntimeError('expected exactly one original durable operation')
    return operations[0]


def owned_builder_rows(receipt):
    ids = command('docker', 'ps', '-aq', '--no-trunc', '--filter', 'label=rcoder.io/application-id=' + receipt['app_id']).split()
    rows = json.loads(command('docker', 'inspect', *ids)) if ids else []
    for row in rows:
        labels = row['Config'].get('Labels') or {}
        # owner-id 已随用户绑定移除退役；物理身份锚定 application-id（filter）+
        # service-type + lifecycle-id
        if (labels.get('service-type') != 'user-app-builder'
                or not labels.get('rcoder.io/lifecycle-id')):
            raise RuntimeError('fault fixture discovered a foreign resource')
    return rows


def cleanup(root, run_id, case_id):
    receipt = json.loads((root / 'ownership.json').read_text())
    if (receipt['run_id'] != run_id or receipt['case_id'] != case_id or receipt['root'] != str(root.resolve())
            or not re.fullmatch(r'rcoder-crash-[0-9a-f]{16}', receipt['project'])
            or case_id[:10] not in receipt['app_id']):
        raise ValueError('crash fixture ownership mismatch')
    control = root / 'control'
    if (control / 'barrier.json').exists() and not (control / 'barrier-settled').exists():
        (control / 'release').write_text('discard')
        wait_file(control / 'barrier-settled', time.monotonic() + 10)
    compose = ['docker', 'compose', '-p', receipt['project'], '-f', str(root / 'compose.json')]
    if not (root / 'compose.json').exists():
        remove_private_config(receipt)
        return {'ok': True}
    ids = command('docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + receipt['project']).split()
    for cid in ids:
        row = json.loads(command('docker', 'inspect', cid))[0]
        labels = row['Config'].get('Labels') or {}
        if labels.get('rcoder.e2e.run') != run_id or labels.get('rcoder.e2e.case') != case_id:
            raise RuntimeError('foreign Compose crash resource')
        (root / (cid + '-identity.json')).write_text(json.dumps({key: row[key] for key in ('Id', 'Image', 'State')}, indent=2))
        logs = subprocess.run(['docker', 'logs', '--tail', '300', cid], capture_output=True, text=True, timeout=20)
        (root / (cid + '-logs.txt')).write_text(logs.stdout + logs.stderr)
        if logs.returncode:
            raise RuntimeError('crash fixture diagnostics failed')
    # Stop the owned executor before administrative fixture cleanup; the proxy
    # has settled its withheld request, so no delayed writer can recreate it.
    command(*compose, 'stop', 'rcoder')
    builders = owned_builder_rows(receipt)
    for row in builders:
        if receipt.get('lifecycle_id') != (row['Config'].get('Labels') or {}).get('rcoder.io/lifecycle-id'):
            raise RuntimeError('builder lifecycle changed before crash fixture cleanup')
        logs = subprocess.run(['docker', 'logs', '--tail', '200', row['Id']], capture_output=True, text=True, timeout=20)
        (root / (row['Id'] + '-builder-logs.txt')).write_text(logs.stdout + logs.stderr)
        if logs.returncode:
            raise RuntimeError('builder crash diagnostics failed')
        command('docker', 'rm', '-f', row['Id'])
    command(*compose, 'down', '-v', '--remove-orphans')
    remove_private_config(receipt)
    if owned_builder_rows(receipt):
        raise RuntimeError('owned builder remained after fixture cleanup')
    return {'ok': True, 'administrative_cleanup': True, 'private_data_retained': True}


def main():
    directory = Path(os.environ['E2E_REPORT_DIR']) / 'docker-crash'
    directory.mkdir(parents=True, exist_ok=False)
    run_id, case_id = os.environ['E2E_RUN_ID'], os.environ['E2E_CASE_ID']
    assertions = []

    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': bool(ok), 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))

    for mode in MODES:
        root = (directory / mode).resolve()
        root.mkdir()
        receipt = {'root': str(root), 'run_id': run_id, 'case_id': case_id,
                   'project': 'rcoder-crash-' + uuid.uuid4().hex[:16],
                   'app_id': 'cr-' + case_id[:10] + ('-pre' if mode == 'before_create' else '-post'), 'user_id': 'crash-e2e'}
        (root / 'ownership.json').write_text(json.dumps(receipt, indent=2))
        pool = ThreadPoolExecutor(max_workers=1)
        try:
            expected = os.environ['E2E_TURSO_BINARY_SHA256']
            image_id = command('docker', 'image', 'inspect', '--format', '{{.Id}}', os.environ.get('E2E_TURSO_RUNTIME_IMAGE', 'dev-master-rcoder:latest'))
            private = Path(tempfile.mkdtemp(prefix=receipt['project'] + '-config-')) / 'config.yml'
            private.write_text(isolated_config((REPO / 'docker/config.yml').read_text()))
            private.chmod(0o600)
            receipt['private_config'] = str(private)
            (root / 'ownership.json').write_text(json.dumps(receipt, indent=2))
            config = service_config(root, image_id, run_id, case_id, '/var/run/docker.sock', private)
            service = config['services']['rcoder']
            service['volumes'] = [m for m in service['volumes'] if m['target'] != '/var/run/docker.sock']
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
            config['volumes'] = {'fault-socket': {'labels': {'rcoder.e2e.run': run_id, 'rcoder.e2e.case': case_id}}}
            config['services']['docker-proxy'] = {
                'image': image_id, 'pull_policy': 'never', 'entrypoint': ['python3', '/fault.py'], 'restart': 'no',
                'labels': {'rcoder.e2e.run': run_id, 'rcoder.e2e.case': case_id},
                'environment': {'FAULT_APP_ID': receipt['app_id'], 'FAULT_USER_ID': receipt['user_id'],
                    'FAULT_PROJECT': receipt['project'], 'FAULT_MODE': mode},
                'volumes': [{'type': 'volume', 'source': 'fault-socket', 'target': '/proxy'},
                    {'type': 'bind', 'source': os.environ.get('DOCKER_SOCKET_PATH', '/var/run/docker.sock'), 'target': '/var/run/docker.sock'},
                    {'type': 'bind', 'source': str(script), 'target': '/fault.py', 'read_only': True},
                    {'type': 'bind', 'source': str(control), 'target': '/control'}],
                'healthcheck': {'test': ['CMD', 'python3', '-c', 'import socket;s=socket.socket(socket.AF_UNIX);s.connect("/proxy/docker.sock");s.close()'], 'interval': '1s', 'timeout': '2s', 'retries': 30},
            }
            (root / 'compose.json').write_text(json.dumps(config, indent=2))
            compose = ['docker', 'compose', '-p', receipt['project'], '-f', str(root / 'compose.json')]
            command(*compose, 'up', '-d', '--no-build', '--pull', 'never', timeout=120)
            base = 'http://' + command(*compose, 'port', 'rcoder', '8090')
            wait_ready(base)
            cid = command(*compose, 'ps', '-q', 'rcoder')
            if command('docker', 'exec', cid, 'sha256sum', '/app/bin/rcoder').split()[0] != expected:
                raise RuntimeError('rcoder binary differs from accepted build artifact')
            future = pool.submit(http, base, '/api/v1/userapp/workspace', {'app_id': receipt['app_id'], 'user_id': receipt['user_id']})
            wait_file(control / 'barrier.json', time.monotonic() + 90, cid)
            barrier = json.loads((control / 'barrier.json').read_text())
            original = db_operation(base, receipt['app_id'], receipt['user_id'])
            receipt['lifecycle_id'] = original['lifecycle_id']
            (root / 'ownership.json').write_text(json.dumps(receipt, indent=2))
            builders = owned_builder_rows(receipt)
            if original['state'] != 'Running' or barrier['stage'] != mode or (mode == 'before_create' and builders) or (mode == 'after_start' and (len(builders) != 1 or not builders[0]['State']['Running'])):
                raise RuntimeError('requested crash window was not established')
            record('Docker ' + mode + ' exact barrier established', True)
            command('docker', 'kill', '--signal', 'KILL', cid)
            stopped = json.loads(command('docker', 'inspect', cid))[0]
            if stopped['State']['Running'] or stopped['State']['ExitCode'] != 128 + signal.SIGKILL:
                raise RuntimeError('rcoder was not terminated by SIGKILL')
            record('Docker ' + mode + ' SIGKILL observed', True)
            (control / 'release').write_text('discard')
            wait_file(control / 'barrier-settled', time.monotonic() + 10)
            try:
                future.result(timeout=10)
            except (OSError, RuntimeError):
                pass
            command('docker', 'start', cid)
            # 随机宿主端口（127.0.0.1::8090）在容器 kill→start 后会重新分配，
            # 探活前须重解析映射端口。
            base = 'http://' + command(*compose, 'port', 'rcoder', '8090')
            receipt['base'] = base
            wait_ready(base)
            # 恢复隔离由重启实例的异步扫描落库（启动+5s 周期）；有界等待
            # quarantine 终态，不放宽断言本身。
            deadline = time.monotonic() + 20
            while True:
                restored = db_operation(base, receipt['app_id'], receipt['user_id'])
                if restored['state'] == 'RecoveryRequired' or time.monotonic() >= deadline:
                    break
                time.sleep(0.5)
            after = owned_builder_rows(receipt)
            attempts = (control / 'create-attempts.jsonl').read_text().splitlines()
            if (restored['operation_id'] != original['operation_id'] or restored['state'] != 'RecoveryRequired'
                    or [row['Id'] for row in after] != [row['Id'] for row in builders] or len(attempts) != 1):
                raise RuntimeError('restart replayed or misclassified an interrupted runtime mutation')
            # Wait through one real 5-second recovery scan, then inspect again.
            time.sleep(6)
            if len((control / 'create-attempts.jsonl').read_text().splitlines()) != 1 or db_operation(base, receipt['app_id'], receipt['user_id'])['state'] != 'RecoveryRequired':
                raise RuntimeError('periodic recovery replayed an uncertain command')
            (root / 'recovery.json').write_text(json.dumps({'before': original, 'after': restored,
                'builder_ids': [row['Id'] for row in after], 'rcoder_id': cid, 'image_id': image_id}, indent=2))
            record('Docker ' + mode + ' restart quarantines without replay', True)
        except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
            detail = type(error).__name__ if isinstance(error, (OSError, subprocess.SubprocessError)) \
                else type(error).__name__ + ': ' + str(error)
            record('Docker ' + mode + ' execution', False, detail)
        finally:
            if (root / 'control/barrier.json').exists():
                (root / 'control/release').write_text('discard')
            pool.shutdown(wait=True, cancel_futures=True)
            try:
                result = cleanup(root, run_id, case_id)
                (root / 'cleanup.json').write_text(json.dumps(result, indent=2))
                record('Docker ' + mode + ' owned cleanup', True)
            except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
                record('Docker ' + mode + ' owned cleanup', False, type(error).__name__)
    return int(not assertions or any(not item['ok'] for item in assertions))


if __name__ == '__main__':
    raise SystemExit(main())
