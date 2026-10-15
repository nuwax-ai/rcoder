import json
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import patch
from build_context_contract import cleanup, run, artifact_stage


class BuildContextOwnership(unittest.TestCase):
    def test_missing_artifacts_stage_is_rejected_before_docker_build(self):
        with self.assertRaisesRegex(ValueError, 'scratch artifacts stage'):
            artifact_stage('FROM debian:12 AS builder\nRUN cargo build --release\n')

    def test_production_stage_is_preserved_verbatim(self):
        stage = 'FROM scratch AS artifacts\nCOPY --from=builder /binary /binary\nCMD ["/binary"]\n'
        self.assertEqual(artifact_stage('FROM debian:12 AS builder\nRUN cargo build --release\n' + stage), stage)

    def receipt(self, root):
        token = 'a' * 32
        receipt = {'token': token, 'run_id': 'run', 'case_id': 'case', 'container_name': 'rcoder-context-' + token, 'image_tag': 'rcoder-context:' + token, 'creation_state': 'not_started'}
        (root / 'ownership.json').write_text(json.dumps(receipt))
        return receipt

    def test_foreign_receipt_never_queries_or_deletes_docker(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.receipt(root)
            with patch('build_context_contract.docker') as api:
                with self.assertRaises(ValueError):
                    cleanup(root, 'foreign-run', 'case')
                api.assert_not_called()

    def test_preexisting_container_is_preserved_even_if_labels_match(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            receipt = self.receipt(root)
            info = {'Name': '/' + receipt['container_name'], 'Config': {'Labels': {'rcoder.e2e.run': 'run', 'rcoder.e2e.case': 'case', 'rcoder.e2e.context': receipt['token']}}}
            with patch('build_context_contract.docker', side_effect=['existing', json.dumps([info])]) as api:
                with self.assertRaises(ValueError):
                    cleanup(root, 'run', 'case', ['existing'])
                self.assertEqual(api.call_count, 2)
                self.assertFalse(json.loads((root / 'cleanup.json').read_text())['ok'])

    def test_pending_creation_with_empty_inventory_is_uncertain(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            receipt = self.receipt(root)
            receipt['creation_state'] = 'pending'
            (root / 'ownership.json').write_text(json.dumps(receipt))
            with patch('build_context_contract.docker', return_value='') as api:
                with self.assertRaisesRegex(ValueError, 'outcome is uncertain'):
                    cleanup(root, 'run', 'case')
                self.assertEqual(api.call_count, 1)
            evidence = json.loads((root / 'cleanup.json').read_text())
            self.assertFalse(evidence['ok'])
            self.assertEqual(evidence['outcome'], 'uncertain')
            self.assertEqual(json.loads((root / 'ownership.json').read_text())['creation_state'], 'pending')

    def test_late_owned_container_closes_pending_create_before_removal(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            receipt = self.receipt(root)
            receipt['creation_state'] = 'pending'
            (root / 'ownership.json').write_text(json.dumps(receipt))
            info = {'Name': '/' + receipt['container_name'], 'Config': {'Labels': {'rcoder.e2e.run': 'run', 'rcoder.e2e.case': 'case', 'rcoder.e2e.context': receipt['token']}}}
            def api(*args):
                if args[0] == 'ps':
                    return 'late-id'
                if args[0] == 'inspect':
                    return json.dumps([info])
                if args[0] == 'rm':
                    saved = json.loads((root / 'ownership.json').read_text())
                    self.assertEqual(saved['container_id'], 'late-id')
                    self.assertEqual(saved['creation_state'], 'completed')
                    self.assertEqual(args, ('rm', 'late-id'))
                    return ''
                self.assertEqual(args[:2], ('image', 'ls'))
                return ''
            with patch('build_context_contract.docker', side_effect=api):
                cleanup(root, 'run', 'case')
            self.assertTrue(json.loads((root / 'cleanup.json').read_text())['ok'])

    def test_create_timeout_keeps_pending_receipt_and_uncertain_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            def api(*args, **kwargs):
                if args[0] == 'build':
                    return 'built'
                if args[:2] == ('image', 'inspect'):
                    return 'sha256:fixture'
                if args[0] == 'create':
                    receipt = json.loads((root / 'ownership.json').read_text())
                    self.assertEqual(receipt['creation_state'], 'pending')
                    self.assertNotIn('container_id', receipt)
                    raise subprocess.TimeoutExpired(['docker', 'create'], 60)
                self.assertEqual(args[0], 'ps')
                return ''
            with patch('build_context_contract.docker', side_effect=api):
                rows = run(root, 'run', 'case', b'')
            self.assertFalse(rows[-1]['ok'])
            self.assertEqual(json.loads((root / 'ownership.json').read_text())['creation_state'], 'pending')
            self.assertEqual(json.loads((root / 'cleanup.json').read_text())['outcome'], 'uncertain')

    def test_build_failure_still_cleans_after_precreation_receipt(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            def command(*args, **kwargs):
                self.assertTrue((root / 'ownership.json').exists())
                if args[0] == 'build':
                    raise TimeoutError('injected build timeout')
                self.assertIn(args[0], ('ps', 'image'))
                return ''
            with patch('build_context_contract.docker', side_effect=command):
                rows = run(root, 'run', 'case', b'')
                self.assertTrue(any(not row['ok'] for row in rows))
                self.assertTrue(rows[-1]['ok'])
                self.assertTrue(json.loads((root / 'cleanup.json').read_text())['ok'])


if __name__ == '__main__':
    unittest.main()
