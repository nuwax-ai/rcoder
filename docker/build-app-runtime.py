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
        return [name for name in names if name in {'target', 'target-console', 'node_modules', '.git', 'reports', '__pycache__'} or name.startswith('.env')]
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
                                '--build-arg', 'PINGAP_VERSION=0.14.1',
                                '--build-arg', 'PINGAP_COMMIT=c74e4eaa44e64958cffa18c33e8bbf5995b6844f',
                                '-t', 'dev-app-runtime-base:latest', '-f', str(runtime / 'Dockerfile'), str(runtime)], cwd=ROOT)


if __name__ == '__main__':
    sys.exit(main())
