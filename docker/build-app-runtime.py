#!/usr/bin/env python3
"""Build runtime using a bounded, credential-free named Cargo context.

默认只构建 base（dev-app-runtime-base）。`--also-runtime` 在同一份准备好的源
上下文上继续构建 runtime 层（dev-app-runtime）：app-cli / file-server-proxy 的
编译位于 Dockerfile.runtime（分层见 docker/app-runtime-base/Dockerfile 头注释）。
"""
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def prepare_context(root, destination):
    def ignored(directory, names):
        return [name for name in names if name in {'target', 'target-console', 'target-unstable', 'node_modules', '.git', 'reports', '__pycache__'} or name.startswith('.env')]
    for filename in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(root / filename, destination / filename)
    for directory in ['crates', 'tests-e2e']:
        shutil.copytree(root / directory, destination / directory, ignore=ignored)


def main():
    arguments = sys.argv[1:]
    also_runtime = '--also-runtime' in arguments
    positional = [value for value in arguments if not value.startswith('--')]
    if len(positional) != 1:
        print('usage: build-app-runtime.py <runtime-dir> [--also-runtime]', file=sys.stderr)
        return 2
    runtime = Path(positional[0]).resolve()
    with tempfile.TemporaryDirectory(prefix='rcoder-runtime-source-') as temporary:
        source = Path(temporary)
        prepare_context(ROOT, source)
        size = sum(path.stat().st_size for path in source.rglob('*') if path.is_file())
        print(f'Cargo source context: {size / 1024 / 1024:.1f} MiB', flush=True)
        status = subprocess.call(['docker', 'build', '--build-context', f'rcoder={source}',
                                  '--build-arg', 'PINGAP_VERSION=0.14.3',
                                  '--build-arg', 'PINGAP_COMMIT=cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8',
                                  '-t', 'dev-app-runtime-base:latest', '-f', str(runtime / 'Dockerfile'), str(runtime)], cwd=ROOT)
        if status != 0 or not also_runtime:
            return status
        return subprocess.call(['docker', 'build', '--build-context', f'rcoder={source}',
                                '--build-arg', 'BASE_IMAGE=dev-app-runtime-base:latest',
                                '-t', 'dev-app-runtime:latest', '-f', str(runtime / 'Dockerfile.runtime'), str(runtime)], cwd=ROOT)


if __name__ == '__main__':
    sys.exit(main())
