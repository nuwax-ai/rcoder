#!/usr/bin/env python3
"""Build runtime using a bounded, credential-free named Cargo context.

默认只构建 base（dev-app-runtime-base）。`--also-runtime` 在同一份准备好的源
上下文上继续构建 runtime 层（dev-app-runtime）：app-cli / file-server-proxy 的
编译位于 Dockerfile.runtime（分层见 docker/app-runtime-base/Dockerfile 头注释）。
"""
from pathlib import Path
import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PINGAP_VERSION = '0.15.0'
PINGAP_COMMIT = '8270a1ebb7a238ea86fa220215714613410378bb'


def prepare_context(root, destination):
    def ignored(directory, names):
        return [name for name in names if name in {'target', 'target-console', 'target-unstable', 'node_modules', '.git', 'reports', '__pycache__'} or name.startswith('.env')]
    for filename in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(root / filename, destination / filename)
    for directory in ['crates', 'tests-e2e']:
        shutil.copytree(root / directory, destination / directory, ignore=ignored)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('runtime_dir', type=Path)
    parser.add_argument('--also-runtime', action='store_true')
    parser.add_argument('--pingap-version', default=os.environ.get('PINGAP_VERSION', PINGAP_VERSION))
    parser.add_argument('--pingap-commit', default=os.environ.get('PINGAP_COMMIT', PINGAP_COMMIT))
    args = parser.parse_args()
    runtime = args.runtime_dir.resolve()
    status = subprocess.call([sys.executable, str(ROOT / 'k8s/scripts/pingap_version_gate.py'),
                              '--pingap-version', args.pingap_version, '--pingap-commit', args.pingap_commit])
    if status:
        return status
    module_spec = importlib.util.spec_from_file_location('asset_context', ROOT / 'tools/build/asset_context.py')
    assets = importlib.util.module_from_spec(module_spec)
    module_spec.loader.exec_module(assets)
    temporary_root = ROOT / '.cache' / 'build-contexts'
    temporary_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='rcoder-runtime-source-', dir=temporary_root) as temporary:
        source = Path(temporary)
        prepare_context(ROOT, source)
        references = Path(os.environ.get('ASSET_REF_DIR', str(source / 'references')))
        if not list(references.glob('*.ref')):
            status = subprocess.call(['make', 'docker-build-runtime-assets', f'ASSET_REF_DIR={references}',
                                      f'PINGAP_VERSION={args.pingap_version}', f'PINGAP_COMMIT={args.pingap_commit}'], cwd=ROOT)
            if status:
                return status
        context = source / 'runtime-context'
        versions = assets.snapshot(runtime, context, sorted(references.glob('*.ref')), 'runtime')
        size = sum(path.stat().st_size for path in source.rglob('*') if path.is_file())
        print(f'Cargo source context: {size / 1024 / 1024:.1f} MiB', flush=True)
        status = subprocess.call(['docker', 'build', *assets.runtime_build_args(versions), '--build-context', f'rcoder={source}',
                                  '--build-arg', 'PINGAP_VERSION=' + args.pingap_version,
                                  '--build-arg', 'PINGAP_COMMIT=' + args.pingap_commit,
                                  '-t', 'dev-app-runtime-base:latest', '-f', str(context / 'Dockerfile'), str(context)], cwd=ROOT)
        if status != 0 or not args.also_runtime:
            return status
        base_spec = importlib.util.spec_from_file_location('runtime_base', ROOT / 'tools/build/runtime_base.py')
        base = importlib.util.module_from_spec(base_spec)
        base_spec.loader.exec_module(base)
        try:
            receipt = base.verify_base('dev-app-runtime-base:latest', args.pingap_version, args.pingap_commit, '22.23.2')
            receipt['build_reference'] = base.pin_local_image(receipt['image_id'])
        except (ValueError, OSError) as error:
            print('runtime base validation: ' + str(error), file=sys.stderr)
            return 1
        print('RCoder runtime base: ' + json.dumps(receipt, sort_keys=True), flush=True)
        return subprocess.call(['docker', 'build', '--build-context', f'rcoder={source}',
                                '--build-arg', 'BASE_IMAGE=' + receipt['build_reference'],
                                '--build-arg', 'PINGAP_VERSION=' + args.pingap_version,
                                '--build-arg', 'PINGAP_COMMIT=' + args.pingap_commit,
                                '--build-arg', 'NODE_RUNTIME_VERSION=22.23.2',
                                '-t', 'dev-app-runtime:latest', '-f', str(context / 'Dockerfile.runtime'), str(context)], cwd=ROOT)


if __name__ == '__main__':
    sys.exit(main())
