"""Reclaim only this case's reserved namespace or this run's explicit label."""
import json
import os
import re
from pathlib import Path
import subprocess
import urllib.request

from cleanup_diagnostics import PurgeRejected, redact, secrets_from, transport_failure


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, timeout=60)


def owned(container, case_id, run_id, registered):
    name = container['Name'].lstrip('/')
    labels = container['Config'].get('Labels') or {}
    reserved = len(case_id) >= 10 and case_id[:10] in name and name.startswith(('rcoder-app-builder-', 'rcoder-app-', 'dev-rcoder-agent-runner-', 'dev-master-rcoder-'))
    return reserved or labels.get('rcoder.e2e.run') == run_id or registered.get(container['Id']) == name


def cleanup_pg_project(directory, run_id, case_id, existing_ids=()):
    """An empty inventory does not settle an accepted Docker create request."""
    receipt_path = directory / 'pg-contract' / 'ownership.json'
    receipt = json.loads(receipt_path.read_text())
    project = receipt['project']
    config = Path(receipt['compose_file']).resolve()
    if receipt.get('run_id') != run_id or receipt.get('case_id') != case_id or not re.fullmatch(r'rcoder-pg-[0-9a-f]{16}', project) or config != (directory / 'pg-contract' / 'compose.json').resolve():
        raise ValueError('PG ownership receipt mismatch')
    ids = command('docker', 'ps', '-aq', '--no-trunc', '--filter', 'label=com.docker.compose.project=' + project).split()
    if ids:
        rows = json.loads(command('docker', 'inspect', *ids))
        if len(rows) != 1:
            raise ValueError('PG project has an unexpected container set')
        row = rows[0]
        labels = row['Config'].get('Labels') or {}
        if (row['Id'] in existing_ids or row['Name'] != '/' + project + '-postgres-1'
                or labels.get('rcoder.e2e.run') != run_id
                or labels.get('com.docker.compose.project') != project
                or labels.get('com.docker.compose.service') != 'postgres'
                or (receipt.get('container_id') and receipt['container_id'] != row['Id'])):
            raise ValueError('PG project contains a preexisting or foreign container')
        # This project creates exactly one service once. Its physical identity
        # proves a formerly uncertain create has reached the runtime registry.
        receipt.update(creation_state='completed', container_id=row['Id'])
        receipt_path.write_text(json.dumps(receipt, indent=2))
        evidence = {key: row[key] for key in ('Id', 'Name', 'Image', 'State') if key in row}
        (directory / 'pg-contract' / 'cleanup-identity.json').write_text(json.dumps(evidence, indent=2))
    settled = receipt.get('creation_state') in ('not_started', 'completed')
    result = subprocess.run(['docker', 'compose', '-p', project, '-f', str(config), 'down', '-v', '--remove-orphans'], env=dict(os.environ, PG_CONTRACT_PASSWORD='unused-for-cleanup'), capture_output=True, text=True, timeout=60)
    (directory / 'pg-contract' / 'cleanup-command.log').write_text((getattr(result, 'stdout', '') or '') + (getattr(result, 'stderr', '') or ''))
    detail = 'PG creation outcome unresolved' if not settled else ('PG owned project cleanup failed' if result.returncode else '')
    return {'project': project, 'run_id': run_id, 'case_id': case_id,
            'creation_settled': settled, 'ok': result.returncode == 0 and settled, 'detail': detail}


def cleanup_case(case_id, run_id, directory, existing_ids=()):
    errors = []
    root = directory / 'resources'
    root.mkdir(exist_ok=True)
    managed_names = set()
    # Reclaim either small build fixture after an interrupted child process.
    for context_name in ('build-context', 'build-artifacts'):
        context_directory = directory / context_name
        if (context_directory / 'ownership.json').exists():
            try:
                context_receipt = json.loads((context_directory / 'ownership.json').read_text())
                managed_names.add(context_receipt['container_name'])
                from build_context_contract import cleanup as cleanup_build_context
                cleanup_build_context(context_directory, run_id, case_id, existing_ids)
            except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
                errors.append('Docker build context ownership cleanup failed: ' + type(error).__name__)
    hot_directory = directory / 'hot-contract'
    if (hot_directory / 'ownership.json').exists():
        try:
            hot_receipt = json.loads((hot_directory / 'ownership.json').read_text())
            managed_names.add(hot_receipt['name'])
            from hot_cleanup import cleanup as cleanup_hot
            hot_result = cleanup_hot(hot_directory, run_id, case_id, existing_ids)
            if not hot_result['ok']:
                errors.extend('Hot fixture cleanup failed: ' + error for error in hot_result['errors'])
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            errors.append('Hot fixture ownership cleanup failed: ' + type(error).__name__)
    # A PG child may have been killed during cleanup. Named volumes and a
    # pre-creation receipt let the parent remove the whole owned Compose project.
    receipt_path = directory / 'pg-contract' / 'ownership.json'
    if receipt_path.exists():
        try:
            record = cleanup_pg_project(directory, run_id, case_id, existing_ids)
            (root / 'pg-project-fallback-cleanup.json').write_text(json.dumps(record))
            if not record['ok']:
                errors.append(record['detail'])
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            errors.append('PG ownership cleanup failed: ' + type(error).__name__)
            (root / 'pg-project-fallback-cleanup.json').write_text(json.dumps({'run_id': run_id, 'case_id': case_id, 'ok': False, 'detail': errors[-1]}))
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
        if name in managed_names:
            errors.append('Resource requires its creation-aware cleanup: ' + name)
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
            text = redact(text, secrets_from(container))
            (root / (name + '-remaining.log')).write_text(text)
        except (OSError, subprocess.SubprocessError) as error:
            diagnostic = transport_failure(error, secrets_from(container))
            (root / (name + '-diagnostic-failure.json')).write_text(json.dumps(diagnostic))
            errors.append(name + ' diagnostic capture failed: ' + json.dumps(diagnostic))
        try:
            if name.startswith('rcoder-app-builder-'):
                app_id = name.removeprefix('rcoder-app-builder-')
                base = os.environ.get('RCODER_URL', 'http://127.0.0.1:8090')
                request = urllib.request.Request(base + '/api/v1/userapp/' + app_id + '/delete/app', data=b'{}', headers={'Content-Type': 'application/json'})
                with urllib.request.urlopen(request, timeout=60) as response:
                    body = json.load(response)
                    if body.get('code') != '0000':
                        raise PurgeRejected(body, secrets_from(container))
            else:
                command('docker', 'rm', '-f', cid)
            record['ok'] = True
        except (OSError, subprocess.SubprocessError, ValueError, RuntimeError) as error:
            record['ok'] = False
            record['error'] = type(error).__name__
            record['diagnostic'] = error.diagnostic if isinstance(error, PurgeRejected) else transport_failure(error, secrets_from(container))
            errors.append(name + ' owned cleanup failed: ' + json.dumps(record['diagnostic']))
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
