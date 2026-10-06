"""仅验证验收工具及真实 Python fixture 契约，不代替 Docker 业务验收。"""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
import urllib.request

import userapp_migration_advisory as harness


class MigrationFixtureContracts(unittest.TestCase):
    def test_private_environment_file_is_0600_before_content_is_written(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'private.env'
            harness.write_private_file(path, 'POSTGRES_PASSWORD=controlled-fixture\n')
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(path.read_text(), 'POSTGRES_PASSWORD=controlled-fixture\n')
            with self.assertRaises(FileExistsError):
                harness.write_private_file(path, 'replacement')

    def test_business_executes_http_and_writes_one_real_newline_launch_marker(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'scenario.txt').write_text('nonzero')
            with socket.socket() as reservation:
                reservation.bind(('127.0.0.1', 0))
                port = reservation.getsockname()[1]
            process = subprocess.Popen([sys.executable, '-c', harness.BUSINESS], cwd=root,
                                       env={**os.environ, 'PORT': str(port)},
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                deadline = time.monotonic() + 5
                while True:
                    try:
                        with opener.open(f'http://127.0.0.1:{port}/', timeout=1) as response:
                            body = response.read().decode()
                            break
                    except OSError:
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(0.02)
                self.assertEqual(body, 'migration-v2-nonzero')
                self.assertEqual((root / 'business-launches.log').read_text().splitlines(), ['nonzero'])
            finally:
                process.terminate()
                process.communicate(timeout=5)

    def test_nonzero_script_executes_and_reads_only_original_operation_id(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / 'state'
            state.mkdir()
            private = 'controlled-fixture-password-never-a-real-secret'
            receipt = state / '.deploy-operation.json'
            receipt.write_text(json.dumps({'operation': {'operation_id': 'original-operation'},
                                           'request': {
                                                       'run_pg': {'username': 'dev', 'password': private}}}))
            receipt.chmod(0o600)
            (root / 'scenario.txt').write_text('nonzero')
            executed = subprocess.run([sys.executable, '-c', harness.MIGRATION], cwd=root,
                                      env={**os.environ, 'APP_CLI_STATE_ROOT': str(state),
                                           'POSTGRES_PASSWORD': private}, capture_output=True,
                                      text=True, timeout=5, check=False)
            self.assertEqual(executed.returncode, 1)
            self.assertIn('MIGRATION-STDOUT-BEGIN', executed.stdout)
            self.assertIn('MIGRATION-STDERR-BEGIN' + 'x' * 24576 + 'MIGRATION-STDERR-END', executed.stderr)
            proof = json.loads((root / 'migration-process.json').read_text())
            self.assertEqual(proof['operation_id'], 'original-operation')
            self.assertEqual(proof['scenario'], 'nonzero')
            self.assertTrue(proof['members'][0]['pid'])
            self.assertNotIn(private, json.dumps(proof))

    def test_display_report_redacts_nested_actual_fixture_credentials_without_truncation(self):
        private = 'fixture-private'
        original = {'events': [{'line': 'x' * 24000 + private + '-tail'}],
                    'request': {'run_pg': {'password': private}},
                    'error': 'postgresql://user:password@localhost/db'}
        safe = harness.safe_report(original, [private])
        self.assertNotIn(private, json.dumps(safe))
        self.assertEqual(safe['request']['run_pg']['password'], '[REDACTED]')
        self.assertTrue(safe['events'][0]['line'].endswith('-tail'))
        self.assertEqual(len(safe['events'][0]['line'].split('[REDACTED]')[0]), 24000)
        self.assertEqual(original['request']['run_pg']['password'], private)

    def test_manifest_routes_migrate_independently_of_devrun(self):
        # Keep the harness's actual generated TOML under a contract check; a
        # missing run.migrate must never become a no-op timeout acceptance.
        import ast
        source = ast.parse(Path(harness.__file__).read_text())
        main = next(node for node in source.body if isinstance(node, ast.FunctionDef) and node.name == 'main')
        configure = next(node for node in main.body if isinstance(node, ast.FunctionDef) and node.name == 'configure')
        assignment = next(node for node in configure.body if isinstance(node, ast.Assign)
                          and isinstance(node.targets[0], ast.Name) and node.targets[0].id == 'manifest')
        code = compile(ast.Expression(assignment.value), harness.__file__, 'eval')
        for argv in ('["python3", "migrate.py"]', '["/definitely-missing/migration-executable"]'):
            manifest = tomllib.loads(eval(code, {'migrate': argv}))
            self.assertEqual(manifest['run']['migrate'], json.loads(argv))
            self.assertEqual(manifest['run']['command'], ['python3', 'main.py'])
            self.assertEqual(manifest['devrun']['command'], ['python3', 'main.py'])
            self.assertNotIn('migrate', manifest['devrun'])


if __name__ == '__main__':
    unittest.main()
