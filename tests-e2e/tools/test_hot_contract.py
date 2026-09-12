import os
from pathlib import Path
import runpy
import subprocess
import tempfile
import traceback
import unittest
from unittest.mock import patch


class DockerDiagnosticTests(unittest.TestCase):
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


if __name__ == '__main__':
    unittest.main()
