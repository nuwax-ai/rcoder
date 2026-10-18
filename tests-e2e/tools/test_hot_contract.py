import os
import io
import json
import tomllib
import zipfile
from pathlib import Path
import runpy
import subprocess
import tempfile
import traceback
import unittest
import http.server
import threading
import urllib.error
import urllib.request
from unittest.mock import patch


class DockerDiagnosticTests(unittest.TestCase):
    def test_real_artifact_upstream_fails_twice_then_serves_original_B(self):
        with tempfile.TemporaryDirectory() as report:
            with patch.dict(os.environ, {'E2E_REPORT_DIR': report, 'E2E_RUN_ID': 'real-retry-upstream'}):
                module = runpy.run_path(str(Path(__file__).with_name('hot_contract.py')))
            module['RELEASE'].set()
            server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), module['Handler'])
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            statuses, bodies = [], []
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            try:
                for _ in range(3):
                    try:
                        with opener.open(f'http://127.0.0.1:{server.server_port}/slow', timeout=2) as response:
                            statuses.append(response.status)
                            bodies.append(response.read())
                    except urllib.error.HTTPError as error:
                        statuses.append(error.code)
                        error.close()
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)
            self.assertEqual(statuses, [503, 503, 200])
            self.assertEqual(bodies, [module['B']])
            self.assertEqual([event['status'] for event in module['RETRY_HTTP_EVENTS']], statuses)

    def test_command_failure_and_timeout_do_not_expose_deploy_token(self):
        with tempfile.TemporaryDirectory() as report:
            with patch.dict(os.environ, {'E2E_REPORT_DIR': report, 'E2E_RUN_ID': 'diagnostic-test'}):
                module = runpy.run_path(str(Path(__file__).with_name('hot_contract.py')))
            token = module['TOKEN']
            command = ['docker', 'run', '-e', 'APP_CLI_DEPLOY_TOKEN=' + token]
            failures = [
                subprocess.CalledProcessError(125, command, output='daemon rejected token=' + token),
                subprocess.TimeoutExpired(command, timeout=90),
            ]
            for failure in failures:
                with self.subTest(failure=type(failure).__name__):
                    with patch('subprocess.check_output', side_effect=failure):
                        try:
                            module['docker'](*command[1:])
                        except RuntimeError as error:
                            diagnostic = ''.join(traceback.format_exception(error))
                        else:
                            self.fail('Docker failure must propagate')
                    self.assertNotIn(token, diagnostic)
                    self.assertIn('Docker command', diagnostic)
            self.assertIn('timed out', diagnostic)


class DeploymentIdentityTests(unittest.TestCase):
    def test_artifact_identity_matches_the_current_source_authority(self):
        with tempfile.TemporaryDirectory() as report:
            with patch.dict(os.environ, {'E2E_REPORT_DIR': report, 'E2E_RUN_ID': 'pingap-identity-test'}):
                module = runpy.run_path(str(Path(__file__).with_name('hot_contract.py')))
            artifact = module['artifact']('identity-A', 'real fixture body')
            with zipfile.ZipFile(io.BytesIO(artifact)) as archive:
                lock = tomllib.loads(archive.read('release.lock.toml').decode())
            self.assertEqual(lock['pingap']['version'], module['PINGAP_IDENTITY']['version'])
            self.assertEqual(lock['pingap']['commit'], module['PINGAP_IDENTITY']['commit'])

    def test_success_requires_complete_matching_operation(self):
        with tempfile.TemporaryDirectory() as report:
            with patch.dict(os.environ, {'E2E_REPORT_DIR': report, 'E2E_RUN_ID': 'identity-test'}):
                module = runpy.run_path(str(Path(__file__).with_name('hot_contract.py')))
            operation = {
                'operation_id': 'new-op', 'request_release_id': 'request-new-op',
                'artifact_release_id': 'manifest-B',
                'deployment_generation_id': module['GENERATION'],
                'deploy_stage': 'succeeded', 'persisted': True, 'phase': 'running', 'error': None,
            }
            predicate = module['successful_identity']
            self.assertTrue(predicate(operation, 'new-op', 'manifest-B'))
            for field, value in [('operation_id', 'old-op'), ('request_release_id', 'old-request'),
                                 ('artifact_release_id', 'manifest-A'), ('deployment_generation_id', 'old-generation'),
                                 ('deploy_stage', 'pending'), ('persisted', False), ('phase', 'failed'),
                                 ('error', 'configuration convergence failed')]:
                with self.subTest(field=field):
                    self.assertFalse(predicate(dict(operation, **{field: value}), 'new-op', 'manifest-B'))
            for field in operation.keys() - {'error'}:
                with self.subTest(missing=field):
                    incomplete = dict(operation)
                    del incomplete[field]
                    self.assertFalse(predicate(incomplete, 'new-op', 'manifest-B'))

class ExplicitStartupResponseTests(unittest.TestCase):
    def setUp(self):
        self.report = tempfile.TemporaryDirectory()
        self.addCleanup(self.report.cleanup)
        with patch.dict(os.environ, {'E2E_REPORT_DIR': self.report.name, 'E2E_RUN_ID': 'startup-response-test'}):
            self.module = runpy.run_path(str(Path(__file__).with_name('hot_contract.py')))

    def response(self, status, envelope):
        return self.module['initializing_response'](status, json.dumps(envelope).encode())

    def test_declared_initialization_for_deploy_and_runtime_endpoints_can_wait(self):
        deploy = {'code': 'ERR_INITIALIZING', 'success': False, 'message': 'runtime owner is initializing', 'data': None, 'tid': None}
        runtime = {'code': 'ERR_INITIALIZING', 'success': False, 'message': 'startup recovery has not completed'}
        self.assertTrue(self.response(503, deploy))
        self.assertTrue(self.response(503, runtime))

    def test_generic_or_old_503_codes_are_not_initialization(self):
        for code in ('ERR_INVALID_STATE', 'ERR_RECOVERY_REQUIRED', 'ERR_BACKEND_ERROR', 'ERR_FORBIDDEN', ''):
            with self.subTest(code=code):
                self.assertFalse(self.response(503, {'code': code, 'success': False, 'message': 'initializing'}))

    def test_other_status_and_invalid_envelope_cannot_be_ignored(self):
        for status in (200, 400, 409, 500):
            with self.subTest(status=status):
                self.assertFalse(self.response(status, {'code': 'ERR_INITIALIZING', 'success': False}))
        for payload in ({'code': 'ERR_INITIALIZING'}, {'code': 'ERR_INITIALIZING', 'success': True},
                        {'code': 'ERR_INITIALIZING', 'success': 0}, ['ERR_INITIALIZING']):
            with self.subTest(payload=payload):
                self.assertFalse(self.response(503, payload))
        self.assertFalse(self.module['initializing_response'](503, b'not JSON'))
        self.assertFalse(self.module['initializing_response'](503, b'\xff'))

    def test_readonly_response_evidence_preserves_status_and_redacts_fixture_token(self):
        token = self.module['TOKEN']
        self.module['record_readonly_response']('initial-A', '/v1/deploy/status', 503,
            json.dumps({'code': 'ERR_INITIALIZING', 'success': False, 'message': 'fixture token=' + token}).encode())
        path = self.module['REPORT'] / 'readonly-responses.jsonl'
        recorded = json.loads(path.read_text())
        self.assertEqual(recorded['status'], 503)
        self.assertEqual(recorded['response']['code'], 'ERR_INITIALIZING')
        self.assertNotIn(token, path.read_text())


if __name__ == '__main__':
    unittest.main()
