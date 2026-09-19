#!/usr/bin/env python3
"""Explicit real PostgreSQL readiness regression. Requires Docker and cargo-nextest.

Example (reuse an installed PG16 app-runtime-base image):
  python3 tools/test_pg_readiness_real.py --image YOUR_PG16_IMAGE

Creates/removes only one uniquely named temporary container. Does not touch
Compose/K8s or existing databases. Missing tools/image fail, never count as pass.
The ordinary Rust suite intentionally ignores this real-environment test.
"""
import argparse
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import threading
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True, help='Local PG16 image with psql and /usr/lib/postgresql/16/bin')
    args = parser.parse_args()
    repository = Path(__file__).resolve().parents[1]
    name = 'rcoder-pg-readiness-' + uuid.uuid4().hex[:12]
    created = False
    cancelled = threading.Event()
    creator_errors = []
    creator = None
    with tempfile.TemporaryDirectory(prefix='rcoder-pg-readiness-') as directory:
        fixture = Path(directory)
        marker = fixture / 'first-probe'
        program = fixture / 'psql'
        try:
            subprocess.run(['docker', 'image', 'inspect', args.image], check=True, stdout=subprocess.DEVNULL)
            subprocess.run([
                'docker', 'run', '-d', '--name', name, '--user', 'postgres', '--entrypoint', 'sh', args.image,
                '-c', 'mkdir -p /tmp/probedata && '
                '/usr/lib/postgresql/16/bin/initdb -D /tmp/probedata -U fixture --auth=trust >/tmp/init.log && '
                'exec /usr/lib/postgresql/16/bin/postgres -D /tmp/probedata -p 5549 -k /tmp',
            ], check=True, stdout=subprocess.DEVNULL)
            created = True
            for _ in range(100):
                ready = subprocess.run(['docker', 'exec', name, 'psql', '-h', '/tmp', '-p', '5549',
                                        '-U', 'fixture', '-d', 'postgres', '-Atc', 'SELECT 1'], capture_output=True)
                if ready.returncode == 0:
                    break
                time.sleep(.2)
            else:
                raise RuntimeError('PostgreSQL fixture did not start')
            # Native helper invokes real psql in the isolated container. Forward
            # env names rather than values so no password can enter the argv.
            variables = ['PGHOST', 'PGPORT', 'PGUSER', 'PGPASSWORD', 'PGDATABASE', 'PGOPTIONS', 'PGCONNECT_TIMEOUT']
            program.write_text('#!/bin/sh\ntouch ' + shlex.quote(str(marker)) + '\nexec docker exec '
                               + ' '.join('-e ' + variable for variable in variables)
                               + ' ' + shlex.quote(name) + ' psql "$@"\n')
            program.chmod(0o700)

            def create_database_later():
                while not cancelled.is_set() and not marker.exists():
                    cancelled.wait(.05)
                if cancelled.wait(4):
                    return
                try:
                    subprocess.run(['docker', 'exec', name, 'createdb', '-h', '/tmp', '-p', '5549',
                                    '-U', 'fixture', 'delayeddb'], check=True)
                except Exception as error:
                    creator_errors.append(type(error).__name__)

            creator = threading.Thread(target=create_database_later)
            creator.start()
            environment = dict(os.environ, PG_READINESS_FIXTURE_PROGRAM=str(program))
            subprocess.run([
                'cargo', 'nextest', 'run', '--manifest-path', 'crates/app-cli/Cargo.toml',
                '--no-fail-fast', '-j2', '--run-ignored', 'ignored-only',
                '-E', 'test(actual_pg_delayed_database_uri)',
            ], cwd=repository, env=environment, check=True)
            if creator_errors:
                raise RuntimeError('Fixture database creation failed')
            print('PASS: real PostgreSQL URI login waited for delayed database creation')
        finally:
            cancelled.set()
            if creator is not None:
                creator.join(timeout=10)
            if created:
                subprocess.run(['docker', 'rm', '-f', name], check=True, stdout=subprocess.DEVNULL)


if __name__ == '__main__':
    main()
