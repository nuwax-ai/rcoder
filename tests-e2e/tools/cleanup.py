"""Reclaim only this case's reserved namespace or this run's explicit label."""
import json
import os
import re
from pathlib import Path
import subprocess
import urllib.request


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, timeout=60)


def owned(container, case_id, run_id, registered):
    name = container['Name'].lstrip('/')
    labels = container['Config'].get('Labels') or {}
    reserved = len(case_id) >= 10 and case_id[:10] in name and name.startswith(('rcoder-app-builder-', 'rcoder-app-', 'dev-rcoder-agent-runner-', 'dev-master-rcoder-'))
    return reserved or labels.get('rcoder.e2e.run') == run_id or registered.get(container['Id']) == name


def cleanup_case(case_id, run_id, directory, existing_ids=()):
    errors = []
    root = directory / 'resources'
    root.mkdir(exist_ok=True)
    # A PG child may have been killed during cleanup. Named volumes and a
    # pre-creation receipt let the parent remove the whole owned Compose project.
    context_directory = directory / 'build-context'
    if (context_directory / 'ownership.json').exists():
        try:
            from build_context_contract import cleanup as cleanup_build_context
            cleanup_build_context(context_directory, run_id, case_id, existing_ids)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            errors.append('Docker build context ownership cleanup failed: ' + type(error).__name__)
    receipt_path = directory / 'pg-contract' / 'ownership.json'
    if receipt_path.exists():
        try:
            receipt = json.loads(receipt_path.read_text())
            project = receipt['project']
            config = Path(receipt['compose_file']).resolve()
            if receipt.get('run_id') != run_id or receipt.get('case_id') != case_id or not re.fullmatch(r'rcoder-pg-[0-9a-f]{16}', project) or config != (directory / 'pg-contract' / 'compose.json').resolve():
                raise ValueError('PG ownership receipt mismatch')
            project_ids = command('docker', 'ps', '-aq', '--no-trunc', '--filter', 'label=com.docker.compose.project=' + project).split()
            if project_ids:
                project_containers = json.loads(command('docker', 'inspect', *project_ids))
                if any(row['Id'] in existing_ids or (row['Config'].get('Labels') or {}).get('rcoder.e2e.run') != run_id for row in project_containers):
                    raise ValueError('PG project contains a preexisting or foreign container')
            result = subprocess.run(['docker', 'compose', '-p', project, '-f', str(config), 'down', '-v', '--remove-orphans'], env=dict(os.environ, PG_CONTRACT_PASSWORD='unused-for-cleanup'), capture_output=True, text=True, timeout=60)
            record = {'project': project, 'run_id': run_id, 'case_id': case_id, 'ok': result.returncode == 0}
            (root / 'pg-project-fallback-cleanup.json').write_text(json.dumps(record))
            if result.returncode:
                errors.append('PG owned project cleanup failed')
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            errors.append('PG ownership cleanup failed: ' + type(error).__name__)
    try:
        ids = command('docker', 'ps', '-aq', '--no-trunc').split()
        # Never persist the full inspect result, which includes secrets.
        containers = json.loads(command('docker', 'inspect', *ids)) if ids else []
    except (OSError, subprocess.SubprocessError, ValueError) as error:
        return ['cleanup inventory failed: ' + type(error).__name__]
    registered = {}
    for path in root.glob('*-ownership.json'):
        try:
            receipt = json.loads(path.read_text())
            if receipt.get('case_id') == case_id:
                registered[receipt['id']] = receipt['name']
        except (ValueError, KeyError):
            errors.append('invalid ownership receipt: ' + path.name)
    for container in containers:
        name = container['Name'].lstrip('/')
        cid = container['Id']
        if not owned(container, case_id, run_id, registered):
            continue
        if (container['Config'].get('Labels') or {}).get('com.docker.compose.project', '').startswith('rcoder-pg-'):
            errors.append('PG project container remains; refusing bare container removal: ' + name)
            continue
        if cid in existing_ids:
            errors.append(name + ' existed before this run; refusing cleanup')
            continue
        record = {'id': cid, 'name': name, 'image': container['Image'], 'state': container['State']}
        (root / (name + '-remaining.json')).write_text(json.dumps(record, indent=2))
        try:
            text = command('docker', 'logs', '--tail', '500', cid)
            for pair in container['Config'].get('Env') or []:
                key, _, value = pair.partition('=')
                if len(value) >= 4 and any(word in key.upper() for word in ['KEY', 'TOKEN', 'SECRET', 'PASSWORD']):
                    text = text.replace(value, '[REDACTED]')
            (root / (name + '-remaining.log')).write_text(text)
        except (OSError, subprocess.SubprocessError) as error:
            errors.append(name + ' diagnostic capture failed: ' + type(error).__name__)
        try:
            if name.startswith('rcoder-app-builder-'):
                app_id = name.removeprefix('rcoder-app-builder-')
                base = os.environ.get('RCODER_URL', 'http://127.0.0.1:8090')
                request = urllib.request.Request(base + '/api/v1/userapp/' + app_id + '/delete/app', data=b'{}', headers={'Content-Type': 'application/json'})
                with urllib.request.urlopen(request, timeout=60) as response:
                    if json.load(response).get('code') != '0000':
                        raise RuntimeError('purge rejected')
            else:
                command('docker', 'rm', '-f', cid)
            record['ok'] = True
        except (OSError, subprocess.SubprocessError, ValueError, RuntimeError) as error:
            record['ok'] = False
            record['error'] = type(error).__name__
            errors.append(name + ' owned cleanup failed: ' + type(error).__name__)
        (root / (name + '-fallback-cleanup.json')).write_text(json.dumps(record, indent=2))
    receipt_path = directory / 'docker-lifecycle' / 'ownership.json'
    if receipt_path.exists():
        record = {'ok': False, 'run_id': run_id, 'case_id': case_id}
        try:
            receipt = json.loads(receipt_path.read_text())
            volume = receipt['volume_name']
            if receipt.get('run_id') != run_id or receipt.get('case_id') != case_id or not re.fullmatch(r'rcoder-test-identity-[0-9a-f]{32}', volume):
                raise ValueError('Docker volume ownership receipt mismatch')
            record['volume'] = volume
            names = command('docker', 'volume', 'ls', '--format', '{{.Name}}').splitlines()
            if volume in names:
                info = json.loads(command('docker', 'volume', 'inspect', volume))[0]
                labels = info.get('Labels') or {}
                if labels.get('rcoder.e2e.run') != run_id or labels.get('rcoder.e2e.case') != case_id:
                    raise ValueError('Docker volume was replaced by another owner')
                command('docker', 'volume', 'rm', volume)
            record['ok'] = True
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            errors.append('Docker owned volume cleanup failed: ' + type(error).__name__)
        (root / 'docker-volume-fallback-cleanup.json').write_text(json.dumps(record))
    return errors
