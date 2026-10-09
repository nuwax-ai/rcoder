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
sys.path.insert(0, str(ROOT))
from tools.build.pingap_identity import source_identity


def prepare_context(root, destination):
    def ignored(directory, names):
        return [name for name in names if name in {'target', 'target-console', 'target-unstable', 'node_modules', '.git', 'reports', '__pycache__'} or name.startswith('.env')]
    for filename in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(root / filename, destination / filename)
    for directory in ['crates', 'tests-e2e']:
        shutil.copytree(root / directory, destination / directory, ignore=ignored)
    # The paired Pingap builder runs inside this named context. Include only
    # reviewed build helpers and source identity/catalog, never runtime secrets.
    build_tools = destination / 'tools/build'
    build_tools.mkdir(parents=True)
    for filename in ['pingap_identity.py', 'pingap-assets.json']:
        shutil.copy2(root / 'tools/build' / filename, build_tools / filename)
    shutil.copytree(root / 'tools/build/pingap-applied', build_tools / 'pingap-applied', ignore=ignored)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('runtime_dir', type=Path, nargs='?')
    parser.add_argument('--also-runtime', action='store_true')
    parser.add_argument('--pingap-version', default=os.environ.get('PINGAP_VERSION'))
    parser.add_argument('--pingap-commit', default=os.environ.get('PINGAP_COMMIT'))
    parser.add_argument('--print-pingap-identity', action='store_true', help='只打印已解析构建身份，不下载或构建')
    args = parser.parse_args()
    try:
        identity = source_identity(ROOT)
    except (OSError, ValueError) as error:
        print('Pingap source identity: ' + str(error), file=sys.stderr)
        return 1
    if args.pingap_version is None:
        args.pingap_version = identity['version']
    if args.pingap_commit is None:
        args.pingap_commit = identity['commit']
    if args.print_pingap_identity:
        print(args.pingap_version + ' ' + args.pingap_commit)
        return 0
    if args.runtime_dir is None:
        parser.error('runtime_dir is required unless --print-pingap-identity is used')
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
