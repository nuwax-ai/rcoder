"""Verify Docker's real context filtering without reading any user workspace."""
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
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
        if len(ids) > 1:
            raise ValueError('multiple containers matched the reserved build context name')
        creation_state = receipt.get('creation_state', 'completed' if receipt.get('container_id') else 'unknown')
        evidence['creation_state'] = creation_state
        evidence['observed_container_ids'] = ids
        if not ids and creation_state in ('pending', 'unknown'):
            evidence['outcome'] = 'uncertain'
            raise ValueError('build context creation outcome is uncertain; empty inventory does not prove completion')
        for cid in ids:
            info = json.loads(docker('inspect', cid))[0]
            if cid in existing_ids or info['Name'] != '/' + name or any((info['Config'].get('Labels') or {}).get(k) != v for k, v in labels.items()) or (receipt.get('container_id') and receipt['container_id'] != cid):
                raise ValueError('build context container owner changed')
            evidence['container_id'] = cid
            # A uniquely owned physical resource closes an ambiguous create call.
            # Persist before deletion so later cleanup can distinguish absence.
            receipt['container_id'] = cid
            receipt['creation_state'] = 'completed'
            (directory / 'ownership.json').write_text(json.dumps(receipt, indent=2))
            evidence['creation_state'] = 'completed'
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


def image_layer_view(archive):
    """Inventory every saved image layer; deleted files still count as shipped."""
    def normalized(name):
        path = PurePosixPath(name)
        if path.is_absolute() or '..' in path.parts:
            raise ValueError('non-relative image layer member')
        return str(path).removeprefix('./').rstrip('/')

    entries = []
    with tarfile.open(archive) as saved:
        manifest_file = saved.extractfile('manifest.json')
        if manifest_file is None:
            raise ValueError('image save manifest is missing')
        manifest = json.load(manifest_file)
        if len(manifest) != 1 or not manifest[0].get('Layers'):
            raise ValueError('artifacts image must contain one image with nonempty layers')
        for layer_name in manifest[0]['Layers']:
            layer_file = saved.extractfile(normalized(layer_name))
            if layer_file is None:
                raise ValueError('image save layer is missing')
            with tarfile.open(fileobj=layer_file, mode='r:*') as layer:
                for member in layer.getmembers():
                    name = normalized(member.name)
                    if name == '.' and member.isdir():
                        continue
                    kind = 'file' if member.isfile() else 'directory' if member.isdir() else 'other'
                    entries.append({'layer': layer_name, 'path': name, 'kind': kind, 'mode': member.mode})
    return entries


def only_artifact_files(entries, paths):
    allowed_directories = {str(parent) for path in paths for parent in PurePosixPath(path).parents if str(parent) != '.'}
    files = [entry['path'] for entry in entries if entry['kind'] == 'file']
    return (len(files) == len(paths) and set(files) == set(paths)
            and all((entry['kind'] == 'file' and entry['path'] in paths)
                    or (entry['kind'] == 'directory' and entry['path'] in allowed_directories)
                    for entry in entries))


def artifact_stage(source):
    """Keep the production final stage verbatim; only replace its builder input."""
    match = re.search(r'^FROM\s+scratch\s+AS\s+artifacts\s*$', source, re.M | re.I)
    if match is None:
        raise ValueError('production Dockerfile is missing the scratch artifacts stage')
    if not re.search(r'^FROM\s+debian:12\s+AS\s+builder\s*$', source, re.M | re.I):
        raise ValueError('production Dockerfile is missing the named builder stage')
    return source[match.start():]


def run(directory, run_id, case_id, ignore=None, artifacts=None):
    directory.mkdir(parents=True, exist_ok=True)
    token = uuid.uuid4().hex
    receipt = {'run_id': run_id, 'case_id': case_id, 'token': token, 'container_name': 'rcoder-context-' + token, 'image_tag': 'rcoder-context:' + token, 'creation_state': 'not_started'}
    receipt_path = directory / 'ownership.json'
    receipt_path.write_text(json.dumps(receipt, indent=2))
    assertions = []
    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': bool(ok), 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))
    prefix = 'Docker artifacts' if artifacts is not None else 'Docker context'
    try:
        rules = (REPO / '.dockerignore').read_bytes() if ignore is None else ignore
        (directory / 'dockerignore-sha256.txt').write_text(hashlib.sha256(rules).hexdigest())
        with tempfile.TemporaryDirectory(prefix='rcoder-context-') as temp:
            context = Path(temp)
            probe = 'rcoder-context-source-' + token
            payloads = {}
            if artifacts is None:
                (context / '.dockerignore').write_bytes(rules)
                (context / 'Dockerfile').write_text('FROM scratch\nCOPY . /\nCMD ["/not-executed"]\n')
                (context / 'Cargo.toml').write_text(probe)
                paths = ['docker/userapp-workspace/private.txt', 'docker/app-workspace/private.txt', '.env', 'nested/.env.local']
            else:
                stage = artifact_stage(artifacts)
                record('Docker artifacts production stage exists', True)
                (directory / 'production-dockerfile-sha256.txt').write_text(hashlib.sha256(artifacts.encode()).hexdigest())
                (context / 'Dockerfile').write_text('FROM scratch AS builder\nCOPY fixture/ /\n' + stage)
                paths = ['fixture/build/crates/private-source.rs', 'fixture/root/.cargo/registry/cache/probe', 'fixture/root/.rustup/toolchains/probe']
                for path in ['build/target/release/agent_runner', 'build/crates/app-cli/target/release/app-cli']:
                    payloads[path] = '#!/bin/sh\n# synthetic executable ' + path + ' ' + token + '\n'
                    target = context / 'fixture' / path
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(payloads[path])
                    target.chmod(0o755)
            for path in paths:
                target = context / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text('synthetic-private-marker')
            labels = ['--label', 'rcoder.e2e.run=' + run_id, '--label', 'rcoder.e2e.case=' + case_id, '--label', 'rcoder.e2e.context=' + token]
            output = docker('build', *labels, '-t', receipt['image_tag'], str(context), timeout=180)
            (directory / 'build.log').write_text(output)
            receipt['image_id'] = docker('image', 'inspect', '--format', '{{.Id}}', receipt['image_tag'])
            receipt_path.write_text(json.dumps(receipt, indent=2))
            receipt['creation_state'] = 'pending'
            receipt_path.write_text(json.dumps(receipt, indent=2))
            receipt['container_id'] = docker('create', '--name', receipt['container_name'], *labels, receipt['image_id'])
            receipt['creation_state'] = 'completed'
            receipt_path.write_text(json.dumps(receipt, indent=2))
            archive = context / 'export.tar'
            docker('export', '-o', str(archive), receipt['container_id'])
            with tarfile.open(archive) as image:
                members = {item.name.removeprefix('./').lstrip('/'): item for item in image.getmembers()}
                (directory / 'members.json').write_text(json.dumps(sorted(members), indent=2))
                if artifacts is None:
                    source = image.extractfile(members['Cargo.toml']) if 'Cargo.toml' in members else None
                    record('Docker context retains source probe', source is not None and source.read().decode() == probe)
                    record('Docker context excludes userapp runtime data', not any(p == 'docker/userapp-workspace' or p.startswith('docker/userapp-workspace/') for p in members))
                    record('Docker context excludes app runtime data', not any(p == 'docker/app-workspace' or p.startswith('docker/app-workspace/') for p in members))
                    record('Docker context excludes local credentials', '.env' not in members and 'nested/.env.local' not in members)
                else:
                    record('Docker artifacts create without command override', True, receipt['container_id'])
                    for path, payload in payloads.items():
                        member = members.get(path)
                        source = image.extractfile(member) if member is not None and member.isfile() else None
                        record('Docker artifacts preserve ' + Path(path).name, source is not None and source.read().decode() == payload and member.mode & 0o111 == 0o111)
            if artifacts is not None:
                saved_image = context / 'image.tar'
                docker('image', 'save', '-o', str(saved_image), receipt['image_id'])
                view = image_layer_view(saved_image)
                (directory / 'image-layer-members.json').write_text(json.dumps(view, indent=2, sort_keys=True))
                record('Docker artifacts exclude source cache and toolchain', only_artifact_files(view, payloads), json.dumps(view, sort_keys=True))
    except Exception as error:
        record(prefix + ' execution', False, type(error).__name__ + ': ' + str(error))
    finally:
        try:
            cleanup(directory, run_id, case_id)
            record(prefix + ' owned resources cleaned', True)
        except Exception as error:
            record(prefix + ' owned resources cleaned', False, type(error).__name__ + ': ' + str(error))
    return assertions


def run_artifacts(directory, run_id, case_id, source=None):
    if source is None:
        source = (REPO / 'docker/rcoder-agent-runner/Dockerfile.build').read_text()
    return run(directory, run_id, case_id, artifacts=source)


if __name__ == '__main__':
    rows = run(Path(os.environ['E2E_REPORT_DIR']) / 'build-context', os.environ['E2E_RUN_ID'], os.environ['E2E_CASE_ID'])
    raise SystemExit(int(any(not item['ok'] for item in rows)))
