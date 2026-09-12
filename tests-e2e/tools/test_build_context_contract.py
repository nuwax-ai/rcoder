import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from build_context_contract import cleanup, run


class BuildContextOwnership(unittest.TestCase):
    def receipt(self, root):
        token = 'a' * 32
        receipt = {'token': token, 'run_id': 'run', 'case_id': 'case', 'container_name': 'rcoder-context-' + token, 'image_tag': 'rcoder-context:' + token}
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
