#!/usr/bin/env python3
"""Build runtime using a bounded, credential-free named Cargo context."""
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
    runtime = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix='rcoder-runtime-source-') as temporary:
        source = Path(temporary)
        prepare_context(ROOT, source)
        size = sum(path.stat().st_size for path in source.rglob('*') if path.is_file())
        print(f'Cargo source context: {size / 1024 / 1024:.1f} MiB', flush=True)
        return subprocess.call(['docker', 'build', '--build-context', f'rcoder={source}',
                                '--build-arg', 'PINGAP_VERSION=0.14.3',
                                '--build-arg', 'PINGAP_COMMIT=cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8',
                                '-t', 'dev-app-runtime-base:latest', '-f', str(runtime / 'Dockerfile'), str(runtime)], cwd=ROOT)


if __name__ == '__main__':
    sys.exit(main())
