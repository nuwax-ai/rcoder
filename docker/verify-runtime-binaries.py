#!/usr/bin/env python3
"""Execute both shipped programs in the final image without starting any services."""
from pathlib import Path
import subprocess
import sys

BINARY_NAMES = ('rcoder', 'agent_runner')


def verify(directory, timeout=10):
    failures = []
    for name in BINARY_NAMES:
        try:
            result = subprocess.run([str(directory / name), '--version'],
                                    capture_output=True, text=True, timeout=timeout)
            if result.returncode != 0:
                failures.append(f'{name} exited {result.returncode}: {result.stderr.strip()}')
            elif not result.stdout.strip().startswith(name + ' '):
                failures.append(f'{name} did not report its version')
        except (OSError, subprocess.TimeoutExpired) as error:
            failures.append(f'{name} could not execute: {error}')
    if failures:
        raise RuntimeError('\n'.join(failures) + '\nRuntime binary validation failed. '
                           'For ABI/GLIBC errors run make docker-build-master-base, '
                           'then rebuild the master image; dev-hot does not repair embedded binaries.')


if __name__ == '__main__':
    try:
        verify(Path(sys.argv[1]))
    except (IndexError, RuntimeError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
    print('rcoder and agent_runner both execute successfully in the runtime image.')
