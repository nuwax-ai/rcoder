"""Real process lifecycle tests with executable fixtures, not PostgreSQL acceptance."""
import os
from pathlib import Path
import shlex
import signal
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


class PostgresSupervisorTests(unittest.TestCase):
    def test_postgres_exit_cancels_bootstrap(self):
        for image in ('app-runtime-base', 'rcoder-agent-runner'):
            with self.subTest(image=image), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / 'PG_VERSION').write_text('16')
                postgres = root / 'postgres'
                postgres.write_text('#!/bin/sh\nsleep 0.2\nexit 7\n')
                postgres.chmod(0o700)
                helper = root / 'identity.sh'
                helper.write_text('pg_admin_identity_load() { PG_ADMIN_USER=admin; }\n'
                                  'pg_bootstrap_database() { sleep 1; touch "$PGDATA/late-write"; }\n')
                source = ROOT / 'docker' / image
                entry = (source / 'pg-supervisor-entry.sh').read_text()
                entry = entry.replace('PG_BIN=/usr/lib/postgresql/16/bin', 'PG_BIN=' + shlex.quote(directory))
                entry = entry.replace('/usr/local/bin/pg-admin-identity.sh', str(helper))
                entry = entry.replace('/usr/local/bin/pg-supervise.py', str(source / 'pg-supervise.py'))
                # The helper location is passed explicitly to the manager.
                env = dict(os.environ, PGDATA=directory)
                result = subprocess.run(['sh', '-c', entry], env=env, capture_output=True,
                                        text=True, timeout=8)
                time.sleep(1.1)
                self.assertEqual(result.returncode, 7, result.stderr)
                self.assertFalse((root / 'late-write').exists(), 'bootstrap outlived its PostgreSQL')

    def test_bootstrap_failure_and_external_stop_reap_owned_children(self):
        for image in ('app-runtime-base', 'rcoder-agent-runner'):
            for mode in ('failed', 'external', 'success'):
                with self.subTest(image=image, mode=mode), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    (root / 'PG_VERSION').write_text('16')
                    postgres = root / 'postgres'
                    postgres.write_text("""#!/usr/bin/python3
import os, signal, time
from pathlib import Path
root = Path(os.environ['PGDATA'])
def stop(sig, frame):
    (root / 'pg-stopped').write_text(str(sig))
    raise SystemExit(0)
signal.signal(signal.SIGINT, stop)
(root / 'pg-pid').write_text(str(os.getpid()))
while True: time.sleep(0.05)
""")
                    postgres.chmod(0o700)
                    helper = root / 'identity.sh'
                    helper.write_text('pg_admin_identity_load() { PG_ADMIN_USER=admin; }\n'
                                      'pg_bootstrap_database() { '
                                      'while [ ! -f "$PGDATA/pg-pid" ]; do sleep 0.05; done; '
                                      + ('return 1; }\n' if mode == 'failed' else
                                         'touch "$PGDATA/bootstrap-done"; return 0; }\n' if mode == 'success' else
                                         'sleep 2; touch "$PGDATA/late-write"; }\n'))
                    source = ROOT / 'docker' / image
                    entry = (source / 'pg-supervisor-entry.sh').read_text()
                    entry = entry.replace('PG_BIN=/usr/lib/postgresql/16/bin', 'PG_BIN=' + shlex.quote(directory))
                    entry = entry.replace('/usr/local/bin/pg-admin-identity.sh', str(helper))
                    entry = entry.replace('/usr/local/bin/pg-supervise.py', str(source / 'pg-supervise.py'))
                    child = subprocess.Popen(['sh', '-c', entry], env=dict(os.environ, PGDATA=directory),
                                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                    try:
                        if mode in ('external', 'success'):
                            deadline = time.monotonic() + 4
                            while not (root / 'pg-pid').exists() and time.monotonic() < deadline:
                                time.sleep(0.02)
                            self.assertTrue((root / 'pg-pid').exists())
                            if mode == 'success':
                                while not (root / 'bootstrap-done').exists() and time.monotonic() < deadline:
                                    time.sleep(0.02)
                                self.assertTrue((root / 'bootstrap-done').exists())
                                time.sleep(0.15)
                                self.assertIsNone(child.poll(), 'bootstrap success stopped PostgreSQL')
                                os.kill(int((root / 'pg-pid').read_text()), 0)
                            child.send_signal(signal.SIGINT)
                        _, error = child.communicate(timeout=8)
                        self.assertEqual(child.returncode, 1 if mode == 'failed' else 0, error)
                        self.assertEqual((root / 'pg-stopped').read_text(), str(signal.SIGINT))
                        pg_pid = int((root / 'pg-pid').read_text())
                        with self.assertRaises(ProcessLookupError):
                            os.kill(pg_pid, 0)
                        time.sleep(2.1)
                        self.assertFalse((root / 'late-write').exists())
                    finally:
                        if child.poll() is None:
                            child.terminate()
                            child.wait(timeout=30)


if __name__ == '__main__':
    unittest.main()
