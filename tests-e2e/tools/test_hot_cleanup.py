import json
from pathlib import Path
import subprocess
import tempfile
import unittest

from hot_cleanup import cleanup, run_owned


class HotCleanupTests(unittest.TestCase):
    def reserve(self, root):
        token = 'a' * 32
        receipt = {'run_id': 'run', 'case_id': 'case', 'ownership_token': token,
                   'name': 'rcoder-review-' + token, 'creation_state': 'pending', 'engine': 'builtin'}
        (root / 'ownership.json').write_text(json.dumps(receipt))
        return receipt

    def test_empty_inventory_keeps_pending_and_reports_uncertain(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.reserve(root)
            calls = []
            def api(*args):
                calls.append(args)
                return ''
            result = cleanup(root, 'run', 'case', docker_fn=api)
            self.assertFalse(result['ok'])
            self.assertEqual(result['outcome'], 'uncertain')
            self.assertEqual(len(calls), 1)
            self.assertEqual(json.loads((root / 'ownership.json').read_text())['creation_state'], 'pending')

    def test_late_owned_resource_settles_before_delete_and_redacts_logs(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            receipt = self.reserve(root)
            info = {'Id': 'late', 'Name': '/' + receipt['name'], 'Config': {'Labels': {'rcoder.e2e.run': 'run', 'rcoder.e2e.case': 'case', 'rcoder.e2e.hot': receipt['ownership_token']}, 'Env': ['APP_CLI_DEPLOY_TOKEN=private-deploy-token']}}
            calls = []
            def api(*args):
                calls.append(args)
                if args[0] == 'ps': return 'late'
                if args[0] == 'inspect': return json.dumps([info])
                if args[0] == 'logs': return 'private-deploy-token'
                self.assertEqual(args, ('rm', '-f', 'late'))
                saved = json.loads((root / 'ownership.json').read_text())
                self.assertEqual(saved['creation_state'], 'completed')
                self.assertEqual(saved['container_id'], 'late')
                return ''
            result = cleanup(root, 'run', 'case', docker_fn=api)
            self.assertTrue(result['ok'])
            self.assertEqual(calls[-1], ('rm', '-f', 'late'))
            self.assertEqual((root / 'container.log').read_text(), '<redacted>')
            self.assertNotIn('private-deploy-token', (root / 'ownership.json').read_text())

    def test_foreign_owner_is_never_logged_or_deleted(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            receipt = self.reserve(root)
            calls = []
            def api(*args):
                calls.append(args)
                if args[0] == 'ps': return 'foreign'
                self.assertEqual(args, ('inspect', 'foreign'))
                return json.dumps([{'Id': 'foreign', 'Name': '/' + receipt['name'], 'Config': {'Labels': {'rcoder.e2e.run': 'other'}}}])
            result = cleanup(root, 'run', 'case', docker_fn=api)
            self.assertFalse(result['ok'])
            self.assertEqual(len(calls), 2)
            self.assertEqual(json.loads((root / 'ownership.json').read_text())['creation_state'], 'pending')

    def test_run_timeout_has_receipt_before_call_without_secret(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            def api(*args):
                receipt = json.loads((root / 'ownership.json').read_text())
                self.assertEqual(receipt['creation_state'], 'pending')
                self.assertEqual(receipt['run_id'], 'run')
                self.assertEqual(receipt['case_id'], 'case')
                self.assertIn('rcoder.e2e.hot=' + receipt['ownership_token'], args)
                self.assertNotIn('private-token', json.dumps(receipt))
                raise subprocess.TimeoutExpired(['docker', 'run'], 90)
            with self.assertRaises(subprocess.TimeoutExpired):
                run_owned(root, 'run', 'case', ['-e', 'APP_CLI_DEPLOY_TOKEN=private-token', 'image'], 'builtin', docker_fn=api)
            self.assertEqual(json.loads((root / 'ownership.json').read_text())['creation_state'], 'pending')

    def test_diagnostic_failure_is_reported_but_owned_container_removed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            receipt = self.reserve(root)
            calls = []
            def api(*args):
                calls.append(args)
                if args[0] == 'ps': return 'owned'
                if args[0] == 'inspect':
                    return json.dumps([{'Id': 'owned', 'Name': '/' + receipt['name'], 'Config': {'Labels': {'rcoder.e2e.run': 'run', 'rcoder.e2e.case': 'case', 'rcoder.e2e.hot': receipt['ownership_token']}}}])
                if args[0] == 'logs': raise RuntimeError('private-token')
                self.assertEqual(args, ('rm', '-f', 'owned'))
                return ''
            result = cleanup(root, 'run', 'case', docker_fn=api)
            self.assertFalse(result['ok'])
            self.assertEqual(calls[-1], ('rm', '-f', 'owned'))
            self.assertNotIn('private-token', json.dumps(result))


if __name__ == '__main__':
    unittest.main()
