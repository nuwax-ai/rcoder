"""Real isolated Compose rcoder recreation with Turso and HTTP identity checks.

Uses the frozen rcoder image binary, never a host dev-hot target mount. Each of
three deployment configurations is rendered/validated before deriving a minimal
rcoder-only project with private writable mounts and automatic cleanup disabled.
Database snapshots are taken offline (container stopped) through the lock-holding
Turso observer binary; the SQLite engine never reads the live library.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
import tempfile
import urllib.error
import urllib.request
import uuid
from turso_compose_contract import inspect as inspect_contract
from first_open_contract import exercise as exercise_first_open

REPO = Path(__file__).resolve().parents[2]
CONFIGURATIONS = (
    REPO / 'docker/docker-compose.yml',
    REPO.parent / 'build-agent-docker/docker-userapp-computer/docker-compose.yml',
    REPO.parent / 'build-agent-docker/docker/docker-compose.yml',
)
OBSERVER_CASE_DIR = 'observer'


def command(*args, timeout=90):
    return subprocess.check_output(args, text=True, stderr=subprocess.PIPE, timeout=timeout).strip()


def observer_binary(report_dir):
    """Build and pin the offline Turso observer once per case directory."""
    directory = Path(report_dir) / OBSERVER_CASE_DIR
    if not directory.exists():
        directory.mkdir(parents=True)
        build = subprocess.run(['cargo', 'build', '-p', 'rcoder-e2e', '--locked', '--bin', 'userapp-db-observer',
                                '--message-format=json'], cwd=REPO, capture_output=True, text=True, timeout=1800)
        (directory / 'build.jsonl').write_text(build.stdout)
        (directory / 'build.stderr').write_text(build.stderr)
        if build.returncode:
            raise RuntimeError('offline observer build failed')
        rows = [json.loads(line) for line in build.stdout.splitlines()]
        binaries = [row['executable'] for row in rows if row.get('reason') == 'compiler-artifact'
                    and row.get('executable') and row.get('target', {}).get('name') == 'userapp-db-observer']
        if len(binaries) != 1:
            raise RuntimeError('offline observer executable missing or ambiguous')
        (directory / 'identity.json').write_text(json.dumps({
            'binary_sha256': hashlib.sha256(Path(binaries[0]).read_bytes()).hexdigest(),
            'evidence_level': 'turso_offline_lock_holding_observer'}, indent=2))
        shutil_target = directory / 'userapp-db-observer'
        shutil_target.write_bytes(Path(binaries[0]).read_bytes())
        shutil_target.chmod(0o755)
    return directory / 'userapp-db-observer'


def offline_snapshot(root, app_id, compose):
    """Stop rcoder, take the lock-holding Turso snapshot, restart and wait ready.

    The container must be running when called; returns (snapshot, base).
    """
    database = root / 'data/userapp.turso.db'
    command(*compose, 'stop', 'rcoder', timeout=120)
    try:
        observer = observer_binary(__import__('os').environ['E2E_REPORT_DIR'])
        result = subprocess.run([str(observer), str(database.resolve())],
                                capture_output=True, text=True, timeout=60)
        if result.returncode:
            raise RuntimeError('offline observer failed: ' + result.stderr.strip()[-400:])
        lifecycles, operations = parse_observer_output(result.stdout)
        app_lifecycle = [record for record in lifecycles if record.get('app_id') == app_id]
        app_operations = {record['operation_id']: record for record in operations
                          if record.get('app_id') == app_id}
        if len(app_lifecycle) != 1 or not app_operations:
            raise RuntimeError('HTTP lifecycle and operation were not persisted to mounted Turso')
        metadata = database.stat()
        snapshot = {'device': metadata.st_dev, 'inode': metadata.st_ino,
                    'lifecycle': app_lifecycle[0], 'operations': app_operations}
    finally:
        command(*compose, 'start', 'rcoder', timeout=120)
    base = 'http://' + command(*compose, 'port', 'rcoder', '8090')
    wait_ready(base)
    return snapshot, base


def parse_observer_output(stdout):
    """Observer line format: {"section": ...} markers, one record JSON per line."""
    section = None
    lifecycles, operations = [], []
    for line in stdout.splitlines():
        if not line:
            continue
        row = json.loads(line)
        if row.get('section'):
            section = row['section']
        elif section == 'lifecycles':
            lifecycles.append(row)
        elif section == 'operations':
            operations.append(row)
        else:
            raise RuntimeError('observer record before any section marker')
    if section != 'operations':
        raise RuntimeError('observer output ended before the operations section')
    return lifecycles, operations


def service_config(root, image, run_id, case_id, socket, config_path=None):
    environment = {
        'RCODER_PORT': '8090', 'RCODER_USERAPP_STORAGE_BACKEND': 'turso',
        'RCODER_USERAPP_TURSO_PATH': '/app/data/userapp.turso.db',
        'RCODER_USERAPP_RECYCLE_ENABLED': 'false', 'RCODER_AUTO_CLEANUP': 'false',
        'RCODER_WORKSPACE_ROOT': '/app/app-workspace', 'DOCKER_SOCKET_PATH': '/var/run/docker.sock',
        'RUST_LOG': 'info', 'ENABLE_TTYD': 'false',
        # preview_coordinator.enabled 的内部令牌经 env 注入（fail-fast）——
        # 隔离栈与 dev compose 同源（默认 dev 令牌）
        'RCODER_PREVIEW_INTERNAL_TOKEN': 'local-dev-preview-token-0123456789',
    }
    mounts = [{'type': 'bind', 'source': str(root / folder), 'target': '/app/' + folder}
              for folder in ('data', 'logs', 'project_workspace', 'computer-project-workspace',
                             'userapp-workspace', 'app-workspace')]
    mounts += [{'type': 'bind', 'source': socket, 'target': '/var/run/docker.sock'},
               {'type': 'bind', 'source': str(config_path or root / 'config.yml'),
                'target': '/app/config.yml', 'read_only': True}]
    return {'services': {'rcoder': {
        'image': image, 'pull_policy': 'never', 'entrypoint': ['/app/bin/rcoder'],
        'command': ['--port', '8090'], 'working_dir': '/app', 'restart': 'no',
        'environment': environment, 'volumes': mounts, 'ports': ['127.0.0.1::8090'],
        'labels': {'rcoder.e2e.run': run_id, 'rcoder.e2e.case': case_id},
    }}}


def isolated_config(source):
    # Replace this complete known section, never a generic "enabled" field.
    # Other cleanup settings fall back to defaults while the scheduler is off.
    result, count = re.subn(r'(?ms)^cleanup_config:\n.*?(?=^[^ \t#\n]|\Z)',
                            'cleanup_config:\n  enabled: false\n\n', source)
    if count != 1:
        raise ValueError('expected exactly one cleanup configuration section')
    return result


def http(base, path, body=None, timeout=120):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(base + path, data=data, headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        result = json.load(response)
        if response.status != 200 or result.get('code') != '0000':
            raise RuntimeError('HTTP contract failed: ' + str(result.get('code')))
        return result


def wait_ready(base):
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(base + '/health', timeout=2) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.25)
    raise RuntimeError('isolated rcoder did not become healthy')


def validate_owned(row, receipt):
    labels = row['Config'].get('Labels') or {}
    if (labels.get('com.docker.compose.project') != receipt['project']
            or labels.get('com.docker.compose.service') != 'rcoder'
            or labels.get('rcoder.e2e.run') != receipt['run_id']
            or labels.get('rcoder.e2e.case') != receipt['case_id']):
        raise ValueError('isolated Compose resource ownership mismatch')


def remove_private_config(receipt):
    if receipt.get('private_config'):
        path = Path(receipt['private_config'])
        if (path.name != 'config.yml' or not path.parent.name.startswith(receipt['project'] + '-config-')
                or path.parent.parent.resolve() != Path(tempfile.gettempdir()).resolve()
                or path.is_symlink() or path.parent.is_symlink()):
            raise ValueError('private configuration cleanup identity mismatch')
        path.unlink(missing_ok=True)
        if path.parent.exists():
            path.parent.rmdir()  # only an empty directory; never recursive


def cleanup(root, run_id, case_id):
    receipt_path = root / 'ownership.json'
    receipt = json.loads(receipt_path.read_text())
    if (receipt['run_id'] != run_id or receipt['case_id'] != case_id
            or not re.fullmatch(r'rcoder-turso-[0-9a-f]{16}', receipt['project'])
            or Path(receipt['root']).resolve() != root.resolve()):
        raise ValueError('Turso runtime receipt ownership mismatch')
    if not (root / 'compose.json').exists() and not receipt.get('creation_started'):
        remove_private_config(receipt)
        return {'ok': True, 'project': receipt['project'], 'creation_started': False}
    compose = ['docker', 'compose', '-p', receipt['project'], '-f', str(root / 'compose.json')]
    ids = command('docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + receipt['project']).split()
    for cid in ids:
        row = json.loads(command('docker', 'inspect', cid))[0]
        validate_owned(row, receipt)
        (root / (cid + '-identity.json')).write_text(json.dumps({key: row[key] for key in ('Id', 'Image', 'State', 'Mounts')}, indent=2))
        try:
            (root / (cid + '-logs.txt')).write_text(command('docker', 'logs', '--tail', '300', cid))
        except subprocess.CalledProcessError:
            # A nonzero diagnostics command cannot turn cleanup into success.
            raise RuntimeError('Turso runtime log capture failed')
    # The builder is removed via the still-running isolated platform, preserving
    # its captured lifecycle fence; never fall back to logical-name docker rm.
    if receipt.get('lifecycle_id') and not receipt.get('app_deleted'):
        http(receipt['base'], '/api/v1/userapp/' + receipt['app_id'] + '/delete/app', {
            'user_id': receipt['user_id'], 'lifecycle_id': receipt['lifecycle_id'],
            'request_id': 'cleanup-' + case_id,
        })
        remaining = command('docker', 'ps', '-aq', '--filter',
                            'label=rcoder.io/application-id=' + receipt['app_id']).split()
        if remaining:
            raise RuntimeError('Turso runtime application resources remain after deletion')
        receipt['app_deleted'] = True
        receipt_path.write_text(json.dumps(receipt, indent=2))
    elif receipt.get('creation_started') and not receipt.get('app_deleted'):
        # A transport failure may leave a durable application: do not down the
        # control plane or erase its DB and pretend that cleanup is complete.
        raise RuntimeError('Turso runtime application creation outcome requires inspection')
    command(*compose, 'down', '--remove-orphans')
    if command('docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + receipt['project']):
        raise RuntimeError('Turso runtime project containers remain')
    remove_private_config(receipt)
    # Keep private data and evidence for inspection. No host recursive deletion.
    return {'ok': True, 'project': receipt['project'], 'retained_data': str(root / 'data')}


def main():
    directory = Path(os.environ['E2E_REPORT_DIR']) / 'turso-runtime'
    directory.mkdir(parents=True, exist_ok=False)
    run_id, case_id = os.environ['E2E_RUN_ID'], os.environ['E2E_CASE_ID']
    image = os.environ.get('E2E_TURSO_RUNTIME_IMAGE', 'dev-master-rcoder:latest')
    expected_binary = os.environ.get('E2E_TURSO_BINARY_SHA256', '')
    assertions = []

    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': ok, 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))

    if not re.fullmatch(r'[0-9a-f]{64}', expected_binary):
        record('Turso frozen binary prerequisite', False, 'Set E2E_TURSO_BINARY_SHA256 from the accepted build artifact')
        return 1
    for index, source in enumerate(CONFIGURATIONS):
        root = directory / str(index)
        root.mkdir()
        receipt = {'root': str(root.resolve()), 'run_id': run_id, 'case_id': case_id,
                   'project': 'rcoder-turso-' + uuid.uuid4().hex[:16],
                   'app_id': 'tu-' + case_id[:10] + '-' + str(index), 'user_id': 'turso-e2e'}
        receipt_path = root / 'ownership.json'
        receipt_path.write_text(json.dumps(receipt, indent=2))
        try:
            contract = inspect_contract(source, root.resolve() / 'data')
            (root / 'source-contract.json').write_text(json.dumps(contract, indent=2))
            record(f'Turso Compose {index} configuration', True)
            image_id = command('docker', 'image', 'inspect', '--format', '{{.Id}}', image)
            # Configuration can contain credentials. Keep its private copy out
            # of the report tree; the report only records its temporary path.
            private = Path(tempfile.mkdtemp(prefix=receipt['project'] + '-config-'))
            config_file = private / 'config.yml'
            config_file.write_text(isolated_config((REPO / 'docker/config.yml').read_text()))
            config_file.chmod(0o600)
            receipt['private_config'] = str(config_file)
            receipt_path.write_text(json.dumps(receipt, indent=2))
            config = service_config(root.resolve(), image_id, run_id, case_id,
                                    os.environ.get('DOCKER_SOCKET_PATH', '/var/run/docker.sock'), config_file)
            for mount in config['services']['rcoder']['volumes']:
                if not mount.get('read_only') and mount['source'].startswith(str(root.resolve()) + '/'):
                    Path(mount['source']).mkdir(parents=True, exist_ok=True)
            (root / 'compose.json').write_text(json.dumps(config, indent=2))
            compose = ['docker', 'compose', '-p', receipt['project'], '-f', str(root / 'compose.json')]
            command(*compose, 'up', '-d', '--no-build', '--pull', 'never')
            receipt['base'] = 'http://' + command(*compose, 'port', 'rcoder', '8090')
            receipt_path.write_text(json.dumps(receipt, indent=2))
            wait_ready(receipt['base'])
            old_id = command(*compose, 'ps', '-q', 'rcoder')
            row = json.loads(command('docker', 'inspect', old_id))[0]
            validate_owned(row, receipt)
            expected_data = str(root.resolve() / 'data')
            if row['Image'] != image_id or not any(m['Destination'] == '/app/data' and m['Source'] == expected_data and m['RW'] for m in row['Mounts']):
                raise RuntimeError('running image or Turso directory mount does not match contract')
            binary_hash = command('docker', 'exec', old_id, 'sha256sum', '/app/bin/rcoder').split()[0]
            if binary_hash != expected_binary:
                raise RuntimeError('image rcoder binary does not match the accepted build artifact')
            receipt['creation_started'] = True
            receipt_path.write_text(json.dumps(receipt, indent=2))
            concurrency = exercise_first_open(receipt['base'], receipt['app_id'], receipt['user_id'])
            (root / 'first-open.json').write_text(json.dumps(concurrency, indent=2))
            record(f'Turso Compose {index} first-open convergence', True)
            path = '/api/v1/userapp/' + receipt['app_id']
            life = http(receipt['base'], path + '/lifecycle?user_id=' + receipt['user_id'])['data']
            receipt['lifecycle_id'] = life['lifecycle_id']
            builders = command('docker', 'ps', '-aq', '--no-trunc', '--filter',
                               'label=rcoder.io/application-id=' + receipt['app_id'], '--filter',
                               'label=service-type=user-app-builder').split()
            if len(builders) != 1:
                raise RuntimeError('first-open did not produce exactly one physical builder')
            builder = json.loads(command('docker', 'inspect', builders[0]))[0]
            labels = builder['Config'].get('Labels') or {}
            # 共享模型：owner-id 标签已退役（用户绑定移除）；物理身份锚定
            # application-id == app_id + lifecycle-id == 权威 lifecycle
            if labels.get('rcoder.io/application-id') != receipt['app_id'] or labels.get('rcoder.io/lifecycle-id') != life['lifecycle_id']:
                raise RuntimeError('first-open builder identity or lifecycle mismatched')
            receipt['builder_id'] = builder['Id']
            record(f'Turso Compose {index} one builder identity', True)
            receipt_path.write_text(json.dumps(receipt, indent=2))
            # current 指针在成功终态后按设计清空；用 first-open 回传的
            # operation_id 定位已完成操作做归属断言。
            op = http(receipt['base'], path + '/operations/' + concurrency['operation_id']
                      + '?user_id=' + receipt['user_id'])['data']
            if not op or op['state'] != 'Succeeded' or op['lifecycle_id'] != life['lifecycle_id']:
                raise RuntimeError('builder creation has no matching successful durable operation')
            # 离线快照：停容器 → 持锁 Turso 观察器直读 → 重启探活
            before, receipt['base'] = offline_snapshot(root.resolve(), receipt['app_id'], compose)
            receipt_path.write_text(json.dumps(receipt, indent=2))
            if before['lifecycle'] != life or op['operation_id'] not in before['operations']:
                raise RuntimeError('HTTP identity does not match mounted database content')
            record(f'Turso Compose {index} HTTP persisted', True)
            command(*compose, 'up', '-d', '--force-recreate', '--no-build', '--pull', 'never')
            receipt['base'] = 'http://' + command(*compose, 'port', 'rcoder', '8090')
            receipt_path.write_text(json.dumps(receipt, indent=2))
            wait_ready(receipt['base'])
            new_id = command(*compose, 'ps', '-q', 'rcoder')
            current_builder = command('docker', 'ps', '-aq', '--no-trunc', '--filter',
                                      'label=rcoder.io/application-id=' + receipt['app_id'], '--filter',
                                      'label=service-type=user-app-builder').split()
            if current_builder != [receipt['builder_id']]:
                # 失败取证：登记的 builder 身份与重建后清单
                evidence = {
                    'registered_builder_id': receipt['builder_id'],
                    'current_builder_ids': current_builder,
                    'builder_rows': [json.loads(row) for row in (
                        command('docker', 'inspect', *current_builder).splitlines() if current_builder else [])],
                    'recreation': {'old_id': old_id, 'new_id': new_id},
                    'new_instance_log': subprocess.run(['docker', 'logs', '--tail', '300', new_id],
                                                       capture_output=True, text=True, timeout=30).stdout[-60000:],
                }
                (root / 'builder-replacement.json').write_text(json.dumps(evidence, indent=2)[:100000])
                raise RuntimeError('rcoder recreation replaced the existing application builder')
            after_life = http(receipt['base'], path + '/lifecycle?user_id=' + receipt['user_id'])['data']
            after_op = http(receipt['base'], path + '/operations/' + concurrency['operation_id']
                            + '?user_id=' + receipt['user_id'])['data']
            after, receipt['base'] = offline_snapshot(root.resolve(), receipt['app_id'], compose)
            receipt_path.write_text(json.dumps(receipt, indent=2))
            if old_id == new_id or before != after or life != after_life or op != after_op:
                raise RuntimeError('container recreation did not preserve lifecycle and operation exactly')
            if command('docker', 'exec', new_id, 'sha256sum', '/app/bin/rcoder').split()[0] != binary_hash:
                raise RuntimeError('rcoder executable changed during persistence acceptance')
            (root / 'recreation.json').write_text(json.dumps({'old_container_id': old_id, 'new_container_id': new_id,
                'image_id': image_id, 'binary_sha256': binary_hash, 'database': after}, indent=2))
            record(f'Turso Compose {index} recreated identity', True)
            # Delete only the captured test lifecycle while its platform is still
            # healthy, then prove a bad Turso target cannot start in memory mode.
            http(receipt['base'], path + '/delete/app', {
                'user_id': receipt['user_id'], 'lifecycle_id': receipt['lifecycle_id'],
                'request_id': 'cleanup-' + case_id,
            })
            if command('docker', 'ps', '-aq', '--filter', 'label=rcoder.io/application-id=' + receipt['app_id']):
                raise RuntimeError('test application survived confirmed full deletion')
            receipt['app_deleted'] = True
            receipt_path.write_text(json.dumps(receipt, indent=2))
            # 路径指向目录（非法——必须命名文件）→ 启动 fail-fast，绝不静默换内存态
            config['services']['rcoder']['environment']['RCODER_USERAPP_TURSO_PATH'] = '/app/data'
            (root / 'compose.json').write_text(json.dumps(config, indent=2))
            command(*compose, 'up', '-d', '--force-recreate', '--no-build', '--pull', 'never')
            bad_id = command('docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + receipt['project'])
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                bad = json.loads(command('docker', 'inspect', bad_id))[0]
                validate_owned(bad, receipt)
                if bad['State']['Status'] == 'exited':
                    break
                time.sleep(0.25)
            else:
                raise RuntimeError('invalid Turso configuration did not terminate startup')
            if bad['State']['ExitCode'] == 0:
                raise RuntimeError('invalid Turso configuration exited successfully')
            record(f'Turso Compose {index} invalid startup rejected', True)

        except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
            # Do not include raw CLI stderr or URLs which may contain credentials.
            # Constructed RuntimeError/ValueError messages are safe literals.
            detail = type(error).__name__ if isinstance(error, (OSError, subprocess.SubprocessError)) \
                else type(error).__name__ + ': ' + str(error)
            record(f'Turso Compose {index} execution', False, detail)
        finally:
            try:
                result = cleanup(root, run_id, case_id)
                (root / 'cleanup.json').write_text(json.dumps(result, indent=2))
                record(f'Turso Compose {index} owned cleanup', True)
            except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
                record(f'Turso Compose {index} owned cleanup', False, type(error).__name__)
    return int(not assertions or any(not item['ok'] for item in assertions))


if __name__ == '__main__':
    raise SystemExit(main())
