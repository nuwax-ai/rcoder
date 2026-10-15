#!/usr/bin/env python3
"""Refuse a stale master runtime base before compiling application binaries."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
LABEL = 'io.rcoder.master-base.source-sha'
# Dockerfile.base uses the repository root build context and COPY .npmrc.
SOURCE_FILES = ('docker/rcoder-master/Dockerfile.base', '.npmrc')


def source_fingerprint(root=ROOT):
    digest = hashlib.sha256()
    for name in SOURCE_FILES:
        digest.update(name.encode() + b'\0')
        digest.update((root / name).read_bytes())
        digest.update(b'\0')
    return digest.hexdigest()


def check_labels(labels, expected):
    if (labels or {}).get(LABEL) != expected:
        raise ValueError('Master base is stale or has no source provenance. '
                         'Run make docker-build-master-base explicitly, then rerun the build. '
                         'The existing base image was not modified.')


def main():
    if sys.argv[1:] == ['fingerprint']:
        print(source_fingerprint())
        return 0
    if len(sys.argv) != 3 or sys.argv[1] != 'check':
        raise ValueError('usage: master-base-contract.py fingerprint | check IMAGE')
    output = subprocess.check_output(
        ['docker', 'image', 'inspect', '--format', '{{json .Config.Labels}}', sys.argv[2]],
        text=True, timeout=30,
    )
    check_labels(json.loads(output), source_fingerprint())
    print('Master base source provenance matches the current runtime definition.')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
