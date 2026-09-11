import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from cleanup import owned, cleanup_case

class OwnershipTests(unittest.TestCase):
    def container(self, name, cid='new', labels=None):
        return {'Id': cid, 'Name': '/' + name, 'Config': {'Labels': labels}}

    def test_other_case_and_unrelated_family_are_preserved(self):
        self.assertFalse(owned(self.container('rcoder-app-old-app'), 'abcdef123456', 'run', {}))
        self.assertFalse(owned(self.container('database-abcdef1234'), 'abcdef123456', 'run', {}))
        self.assertFalse(owned(self.container('rcoder-app-any'), '', 'run', {}))

    def test_receipt_requires_exact_id_and_name(self):
        receipt = {'old': 'dev-master-rcoder-generated'}
        self.assertTrue(owned(self.container('dev-master-rcoder-generated', 'old'), 'abcdef123456', 'run', receipt))
        self.assertFalse(owned(self.container('dev-master-rcoder-generated', 'replacement'), 'abcdef123456', 'run', receipt))
        self.assertFalse(owned(self.container('another', 'old'), 'abcdef123456', 'run', receipt))

    def test_preexisting_identity_cannot_be_deleted_even_with_matching_label(self):
        container = self.container('rcoder-review-existing', 'existing', {'rcoder.e2e.run': 'run'})
        with tempfile.TemporaryDirectory() as temp:
            with patch('cleanup.command', side_effect=['existing', json.dumps([container])]) as api:
                errors = cleanup_case('abcdef123456', 'run', Path(temp), ['existing'])
                self.assertTrue(any('refusing cleanup' in error for error in errors))
                self.assertEqual(api.call_count, 2)

    def test_reserved_case_and_explicit_run_label(self):
        self.assertTrue(owned(self.container('rcoder-app-abcdef1234-test'), 'abcdef123456', 'run', {}))
        self.assertTrue(owned(self.container('rcoder-review-id', labels={'rcoder.e2e.run': 'run'}), 'abcdef123456', 'run', {}))
        self.assertFalse(owned(self.container('rcoder-review-id', labels={'rcoder.e2e.run': 'other'}), 'abcdef123456', 'run', {}))

if __name__ == '__main__':
    unittest.main()
