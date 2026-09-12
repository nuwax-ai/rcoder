import json
import os
import subprocess
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

from cleanup_diagnostics import PurgeRejected, secrets_from, transport_failure
from cleanup import cleanup_case


class CleanupDiagnostics(unittest.TestCase):
    def test_process_failure_retains_exit_code_and_scrubs_combined_stderr(self):
        error = subprocess.CalledProcessError(28, ['curl', 'private-key'], output='timed out private-key')
        result = transport_failure(error, ['private-key'])
        self.assertEqual(result['exit_code'], 28)
        self.assertEqual(result['stderr'], 'timed out [REDACTED]')
        self.assertNotIn('private-key', json.dumps(result))

    def test_business_failure_keeps_code_and_message_without_key(self):
        error = PurgeRejected({'code': 'ERR_CONFLICT', 'message': 'pending private-key'}, ['private-key'])
        self.assertEqual(error.diagnostic['code'], 'ERR_CONFLICT')
        self.assertEqual(error.diagnostic['message'], 'pending [REDACTED]')

    def test_timeout_does_not_serialize_secret_bearing_command(self):
        error = subprocess.TimeoutExpired(['curl', 'private-key'], 60)
        result = transport_failure(error, ['private-key'])
        self.assertEqual(result['transport'], 'timeout')
        self.assertEqual(result['timeout_seconds'], 60)
        self.assertNotIn('private-key', json.dumps(result))

    def test_redaction_inputs_include_host_and_container_credentials(self):
        with patch.dict(os.environ, {'RCODER_API_KEY': 'host-key'}):
            values = secrets_from({'Config': {'Env': ['APP_CLI_DEPLOY_TOKEN=container-key']}})
        self.assertIn('host-key', values)
        self.assertIn('container-key', values)

    def test_fallback_report_retains_redacted_transport_failure(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            container = {'Id': 'owned', 'Name': '/rcoder-app-builder-case123456-owned', 'Image': 'image', 'State': {}, 'Config': {'Labels': {'rcoder.e2e.run': 'run'}, 'Env': ['DEPLOY_TOKEN=private-key']}}
            with patch('cleanup.command', side_effect=['owned', json.dumps([container]), subprocess.CalledProcessError(1, ['docker', 'logs'], output='failed private-key')]), patch('cleanup.urllib.request.urlopen', side_effect=TimeoutError('timeout private-key')):
                errors = cleanup_case('case123456-rest', 'run', root)
            data = json.loads((root / 'resources/rcoder-app-builder-case123456-owned-fallback-cleanup.json').read_text())
            self.assertFalse(data['ok'])
            self.assertEqual(data['diagnostic']['transport'], 'timeout')
            self.assertNotIn('private-key', json.dumps(errors))
            log_error = json.loads((root / 'resources/rcoder-app-builder-case123456-owned-diagnostic-failure.json').read_text())
            self.assertEqual(log_error['exit_code'], 1)
            self.assertEqual(log_error['stderr'], 'failed [REDACTED]')


if __name__ == '__main__':
    unittest.main()
