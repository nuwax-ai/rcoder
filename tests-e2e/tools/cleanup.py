"""Reclaim only this case's reserved namespace or this run's explicit label."""
import json
import os
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
    try:
        ids = command('docker', 'ps', '-aq', '--no-trunc').split()
        if not ids:
            return errors
        # Never persist the full inspect result, which includes secrets.
        containers = json.loads(command('docker', 'inspect', *ids))
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
    return errors
