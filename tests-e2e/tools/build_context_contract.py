"""Verify Docker's real context filtering without reading any user workspace."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import uuid

REPO = Path(__file__).resolve().parents[2]


def docker(*args, timeout=60):
    return subprocess.check_output(['docker', *args], text=True, stderr=subprocess.STDOUT, timeout=timeout).strip()


def cleanup(directory, run_id, case_id, existing_ids=()):
    """Use pre-created names plus immutable identities and exact owner labels."""
    receipt = json.loads((directory / 'ownership.json').read_text())
    token = receipt.get('token', '')
    if (receipt.get('run_id'), receipt.get('case_id')) != (run_id, case_id) or not re.fullmatch('[0-9a-f]{32}', token):
        raise ValueError('build context ownership receipt mismatch')
    name = 'rcoder-context-' + token
    tag = 'rcoder-context:' + token
    if receipt.get('container_name') != name or receipt.get('image_tag') != tag:
        raise ValueError('build context resource name mismatch')
    labels = {'rcoder.e2e.run': run_id, 'rcoder.e2e.case': case_id, 'rcoder.e2e.context': token}
    evidence = {'run_id': run_id, 'case_id': case_id, 'ok': False}
    try:
        ids = docker('ps', '-aq', '--no-trunc', '--filter', 'name=^/' + name + '$').split()
        for cid in ids:
            info = json.loads(docker('inspect', cid))[0]
            if cid in existing_ids or info['Name'] != '/' + name or any((info['Config'].get('Labels') or {}).get(k) != v for k, v in labels.items()) or (receipt.get('container_id') and receipt['container_id'] != cid):
                raise ValueError('build context container owner changed')
            evidence['container_id'] = cid
            docker('rm', cid)
        ids = docker('image', 'ls', '--no-trunc', '-q', tag).split()
        for iid in set(ids):
            info = json.loads(docker('image', 'inspect', iid))[0]
            if any((info['Config'].get('Labels') or {}).get(k) != v for k, v in labels.items()) or (receipt.get('image_id') and receipt['image_id'] != iid):
                raise ValueError('build context image owner changed')
            evidence['image_id'] = iid
            docker('image', 'rm', tag)
        evidence['ok'] = True
    finally:
        (directory / 'cleanup.json').write_text(json.dumps(evidence, indent=2))


def run(directory, run_id, case_id, ignore=None):
    directory.mkdir(parents=True, exist_ok=True)
    token = uuid.uuid4().hex
    receipt = {'run_id': run_id, 'case_id': case_id, 'token': token, 'container_name': 'rcoder-context-' + token, 'image_tag': 'rcoder-context:' + token}
    receipt_path = directory / 'ownership.json'
    receipt_path.write_text(json.dumps(receipt, indent=2))
    assertions = []
    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': bool(ok), 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))
    try:
        rules = (REPO / '.dockerignore').read_bytes() if ignore is None else ignore
        (directory / 'dockerignore-sha256.txt').write_text(hashlib.sha256(rules).hexdigest())
        with tempfile.TemporaryDirectory(prefix='rcoder-context-') as temp:
            context = Path(temp)
            (context / '.dockerignore').write_bytes(rules)
            (context / 'Dockerfile').write_text('FROM scratch\nCOPY . /\nCMD ["/not-executed"]\n')
            probe = 'rcoder-context-source-' + token
            (context / 'Cargo.toml').write_text(probe)
            for path in ['docker/userapp-workspace/private.txt', 'docker/app-workspace/private.txt', '.env', 'nested/.env.local']:
                target = context / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text('synthetic-private-marker')
            labels = ['--label', 'rcoder.e2e.run=' + run_id, '--label', 'rcoder.e2e.case=' + case_id, '--label', 'rcoder.e2e.context=' + token]
            output = docker('build', *labels, '-t', receipt['image_tag'], str(context), timeout=180)
            (directory / 'build.log').write_text(output)
            receipt['image_id'] = docker('image', 'inspect', '--format', '{{.Id}}', receipt['image_tag'])
            receipt_path.write_text(json.dumps(receipt, indent=2))
            receipt['container_id'] = docker('create', '--name', receipt['container_name'], *labels, receipt['image_id'])
            receipt_path.write_text(json.dumps(receipt, indent=2))
            archive = context / 'export.tar'
            docker('export', '-o', str(archive), receipt['container_id'])
            with tarfile.open(archive) as image:
                members = {item.name.removeprefix('./').lstrip('/'): item for item in image.getmembers()}
                (directory / 'members.json').write_text(json.dumps(sorted(members), indent=2))
                source = image.extractfile(members['Cargo.toml']) if 'Cargo.toml' in members else None
                record('Docker context retains source probe', source is not None and source.read().decode() == probe)
                record('Docker context excludes userapp runtime data', not any(p == 'docker/userapp-workspace' or p.startswith('docker/userapp-workspace/') for p in members))
                record('Docker context excludes app runtime data', not any(p == 'docker/app-workspace' or p.startswith('docker/app-workspace/') for p in members))
                record('Docker context excludes local credentials', '.env' not in members and 'nested/.env.local' not in members)
    except Exception as error:
        record('Docker build context execution', False, type(error).__name__ + ': ' + str(error))
    finally:
        try:
            cleanup(directory, run_id, case_id)
            record('Docker context owned resources cleaned', True)
        except Exception as error:
            record('Docker context owned resources cleaned', False, type(error).__name__ + ': ' + str(error))
    return assertions


if __name__ == '__main__':
    rows = run(Path(os.environ['E2E_REPORT_DIR']) / 'build-context', os.environ['E2E_RUN_ID'], os.environ['E2E_CASE_ID'])
    raise SystemExit(int(any(not item['ok'] for item in rows)))
