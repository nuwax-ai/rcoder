#!/usr/bin/env python3
"""Run Docker against a private, checked asset snapshot, then remove only it."""
import argparse
import fnmatch
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def snapshot(source, target, references, kind, required=None, trusted_catalog=None):
    fixed = {'agent': {'dbx', 'pingap'}, 'runtime': {'dbx', 'pingap', 'ttyd', 'node', 'go', 'deno'}}
    known = {'dbx', 'pingap', 'ttyd', 'node', 'go', 'deno'}
    if kind == 'downloads':
        if not required:
            raise ValueError('downloads snapshots need explicit required components')
        required = set(required)
        if not required.issubset(known):
            raise ValueError('unknown required components: ' + ', '.join(sorted(required - known)))
    elif kind in fixed:
        if required is not None:
            raise ValueError('explicit required components are only supported for downloads snapshots')
        required = fixed[kind]
    else:
        raise ValueError('unknown asset snapshot kind: ' + kind)
    def ignored(directory, names):
        result = []
        for name in names:
            if name in {'.git', 'target', 'node_modules', '__pycache__'} or name.startswith('.env'):
                result.append(name)
            elif name.startswith('.runtime-stage-') or name == '.runtime-assets-locks':
                result.append(name)
            elif Path(directory).name in ('cache', 'downloads') and any(fnmatch.fnmatch(name, pattern) for pattern in ('dbx-web-*', 'dbx-static', 'pingap-v*-linux-gnu-*-full.tar.gz', 'ttyd-*', 'node-*.tar.gz', 'go*.tar.gz', 'deno-*', '.dbx-*', '*.tmp')):
                result.append(name)
        return result
    shutil.copytree(source, target, dirs_exist_ok=True,
                    ignore=ignored)
    components = set()
    manifests = []
    versions = {}
    for ref in references:
        # The shared publishers name references <component>[ -architecture ].ref.
        # Select before opening them so a consumer without Pingap has no
        # dependency on unrelated Pingap references or CLI source metadata.
        named_component = Path(ref).stem.split('-', 1)[0]
        if kind == 'downloads' and named_component not in required:
            continue
        pointer = json.loads(Path(ref).read_text())
        entry = Path(pointer['entry'])
        if digest(entry / 'manifest.json') != pointer['manifest_sha256']:
            raise ValueError('asset manifest changed: ' + str(ref))
        manifest = json.loads((entry / 'manifest.json').read_text())
        is_dbx = pointer.get('protocol') == 2
        component = 'dbx' if is_dbx else manifest['identity']['component']
        if component != named_component:
            raise ValueError('asset reference component does not match its identity: ' + str(ref))
        if kind == 'agent' and component not in ('dbx', 'pingap'):
            continue
        if component == 'pingap':
            # This module is also loaded via importlib by the public build script.
            # Load only the reviewed sibling, independent of caller sys.path.
            spec = importlib.util.spec_from_file_location('runtime_assets', Path(__file__).with_name('runtime_assets.py'))
            assets = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(assets)
            manifest = assets.verify(entry, trusted_catalog)
        components.add(component)
        if not is_dbx:
            version = manifest['identity']['version']
            if component in versions and versions[component] != version:
                raise ValueError('mixed versions in asset references: ' + component)
            versions[component] = version
        for relative, record in manifest['files'].items():
            path = Path(relative)
            if path.is_absolute() or '..' in path.parts:
                raise ValueError('unsafe asset manifest path')
            checksum = record['sha256'] if is_dbx else record
            original = entry / path
            if original.is_symlink() or digest(original) != checksum:
                raise ValueError('asset content changed: ' + str(original))
            destination = target / ('downloads' if is_dbx else '') / path
            if (kind == 'agent' and component == 'pingap') or (kind == 'downloads' and not is_dbx):
                destination = target / 'downloads' / path.name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(original, destination)
            if digest(destination) != checksum:
                raise ValueError('asset copy changed: ' + str(destination))
        manifests.append({'component': component, 'manifest_sha256': pointer['manifest_sha256'], 'identity': manifest.get('identity', manifest.get('inputs'))})
    if not required.issubset(components):
        raise ValueError('missing asset references: ' + ', '.join(sorted(required - components)))
    if kind == 'agent':
        # The final agent image builds the reviewed paired binary separately
        # from the immutable official release downloads above.
        helper = Path(__file__).with_name('pingap-applied')
        spec = importlib.util.spec_from_file_location('pingap_applied_source', helper / 'apply.py')
        paired = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(paired)
        paired.checked_manifest(helper)
        shutil.copytree(helper, target / 'pingap-applied', dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
    (target / 'asset-manifest.json').write_text(json.dumps(manifests, sort_keys=True) + '\n')
    return versions


def runtime_build_args(versions, command=None):
    names = {'node': 'NODE_RUNTIME_VERSION', 'go': 'GO_VERSION', 'deno': 'DENO_VERSION', 'ttyd': 'TTYD_VERSION'}
    declared = {}
    for index, argument in enumerate(command or []):
        if argument == '--build-arg' and index + 1 < len(command):
            value = command[index + 1]
        elif argument.startswith('--build-arg='):
            value = argument.split('=', 1)[1]
        else:
            continue
        name, separator, requested = value.partition('=')
        declared[name] = requested if separator else os.environ.get(name)
    for component, name in {**names, 'pingap': 'PINGAP_VERSION'}.items():
        if component in versions and name in declared and declared[name] != versions[component]:
            raise ValueError(f'{component} requested version {declared[name]} disagrees with asset version {versions[component]}')
    return [value for component, name in names.items() if component in versions for value in ('--build-arg', name + '=' + versions[component])]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--ref-dir', type=Path, default=Path(os.environ.get('ASSET_REF_DIR', '.cache/build-assets/missing')))
    parser.add_argument('--kind', choices=('agent', 'runtime', 'downloads'), required=True)
    parser.add_argument('--required', action='append', choices=('dbx', 'pingap', 'ttyd', 'node', 'go', 'deno'), help='explicit component contract for downloads snapshots')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command:
        parser.error('Docker command required after --')
    try:
        context_root = Path(__file__).resolve().parents[2] / '.cache/build-contexts'
        context_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='rcoder-asset-context-', dir=context_root) as temporary:
            target = Path(temporary)
            references = sorted(args.ref_dir.glob('*.ref'))
            versions = snapshot(args.source, target, references, args.kind, args.required)
            if versions:
                position = next((i for i, part in enumerate(command) if '{context}' in part and not part.startswith('{context}/')), None)
                if position is None:
                    raise ValueError('Docker command must declare its {context} argument')
                command[position:position] = runtime_build_args(versions, command)
            command = [part.replace('{context}', str(target)).replace('{dockerfile}', str(target / 'Dockerfile')) for part in command]
            return subprocess.call(command)
    except (OSError, ValueError, KeyError) as error:
        print('asset snapshot: ' + str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
