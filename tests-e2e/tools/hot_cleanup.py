"""Durable ownership of a hot-deploy fixture, including ambiguous Docker run."""
import json
from pathlib import Path
import re
import subprocess
import uuid


def docker(*args):
    try:
        return subprocess.check_output(['docker', *args], text=True, stderr=subprocess.STDOUT, timeout=90).strip()
    except subprocess.SubprocessError as error:
        # Docker exceptions may include deployment credentials in argv/output.
        raise RuntimeError('Docker diagnostic failed: ' + type(error).__name__) from None


def run_owned(directory, run_id, case_id, args, engine, docker_fn=docker):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    token = uuid.uuid4().hex
    receipt = {'run_id': run_id, 'case_id': case_id, 'ownership_token': token,
               'name': 'rcoder-review-' + token, 'creation_state': 'pending', 'engine': engine}
    path = directory / 'ownership.json'
    # Never replace the identity of an earlier attempt in the same report.
    with path.open('x') as stream:
        json.dump(receipt, stream, indent=2)
    cid = docker_fn('run', '--name', receipt['name'], '--label', 'rcoder.e2e.run=' + run_id,
                    '--label', 'rcoder.e2e.case=' + case_id,
                    '--label', 'rcoder.e2e.hot=' + token, *args)
    if not re.fullmatch('[0-9a-f]{64}', cid):
        raise ValueError('Docker run returned no valid physical container identity')
    receipt['container_id'] = cid
    receipt['creation_state'] = 'completed'
    path.write_text(json.dumps(receipt, indent=2))
    return cid


def cleanup(directory, run_id, case_id, existing_ids=(), docker_fn=docker, secrets=()):
    """Return durable cleanup evidence; ok=False includes unresolved creation."""
    directory = Path(directory)
    result = {'run_id': run_id, 'case_id': case_id, 'ok': False, 'errors': []}
    try:
        path = directory / 'ownership.json'
        receipt = json.loads(path.read_text())
        token = receipt.get('ownership_token', '')
        if receipt.get('run_id') != run_id or receipt.get('case_id') != case_id or not re.fullmatch('[0-9a-f]{32}', token) or receipt.get('name') != 'rcoder-review-' + token:
            raise ValueError('hot fixture ownership receipt mismatch')
        ids = docker_fn('ps', '-aq', '--no-trunc', '--filter', 'name=^/' + receipt['name'] + '$').split()
        result['observed_ids'] = ids
        if len(ids) > 1:
            raise ValueError('multiple hot fixtures matched reserved name')
        if not ids and (receipt.get('creation_state') != 'completed' or not receipt.get('container_id')):
            result['outcome'] = 'uncertain'
            raise ValueError('hot fixture creation outcome unresolved; empty inventory is not completion')
        if ids:
            cid = ids[0]
            info = json.loads(docker_fn('inspect', cid))[0]
            labels = info['Config'].get('Labels') or {}
            expected = {'rcoder.e2e.run': run_id, 'rcoder.e2e.case': case_id, 'rcoder.e2e.hot': token}
            if cid in existing_ids or info['Id'] != cid or info['Name'] != '/' + receipt['name'] or any(labels.get(k) != v for k, v in expected.items()) or (receipt.get('container_id') and receipt['container_id'] != cid):
                raise ValueError('hot fixture physical identity or owner mismatch')
            receipt.update(container_id=cid, creation_state='completed')
            path.write_text(json.dumps(receipt, indent=2))
            result['container_id'] = cid
            redact = list(secrets)
            for pair in info['Config'].get('Env') or []:
                key, _, value = pair.partition('=')
                if value and any(word in key.upper() for word in ('TOKEN', 'PASSWORD', 'SECRET', 'KEY')):
                    redact.append(value)
            commands = [('container.log', ('logs', cid))]
            if receipt.get('engine') != 'builtin':
                commands.append(('app-cli.log', ('exec', cid, 'sh', '-c', 'tail -n 200 /home/user/logs/app-cli.out.log /home/user/logs/app-cli.err.log')))
            for name, command in commands:
                try:
                    text = docker_fn(*command)
                    for secret in redact:
                        if secret:
                            text = text.replace(secret, '<redacted>')
                    (directory / name).write_text(text)
                except Exception as error:
                    result['errors'].append('pre-cleanup diagnostics ' + name + ': ' + type(error).__name__)
            docker_fn('rm', '-f', cid)
        result['outcome'] = 'removed'
        result['ok'] = not result['errors']
    except Exception as error:
        # Deliberately omit raw Docker output/argv to protect deployment tokens.
        detail = str(error) if isinstance(error, ValueError) else type(error).__name__
        result['errors'].append(detail)
    (directory / 'cleanup.json').write_text(json.dumps(result, indent=2))
    return result
