"""Exact crash receipt validation, without claiming real Docker recovery."""
import copy
import json
import unittest
from unittest.mock import patch
import docker_crash_contract as contract


class CrashReceiptTests(unittest.TestCase):
    def setUp(self):
        self.original = dict(operation_id='original', app_id='owned', lifecycle_id='life',
                             request_id=None, kind='EnsureBuilder', state='Running')
        self.restored = dict(self.original, state='Failed')
        self.persisted = dict(self.restored, error_code='ERR_CONFLICT',
            error_message='original failure; physical state verified definite by recovery scanner',
            checkpoint={'fence_released_evidence': {'kind': 'live_builder', 'workload': {'uid': 'a' * 64}}})

    def test_exact_live_builder_settlement_and_original_unknown_write_are_distinct(self):
        contract.validate_recovered_operation(self.original, self.restored, self.persisted, 'after_start', ['a' * 64])
        restored = dict(self.restored, state='RecoveryRequired')
        persisted = dict(self.persisted, state='RecoveryRequired', checkpoint={'old': 'kept'})
        contract.validate_recovered_operation(self.original, restored, persisted, 'before_create', [])

    def test_other_operation_uid_missing_diagnostic_or_unsupported_terminal_is_rejected(self):
        variants = [dict(operation_id='other'), dict(lifecycle_id='other'), dict(state='Succeeded'),
                    dict(error_message='unverified'), dict(error_code=None), dict(checkpoint={}),
                    dict(checkpoint={'fence_released_evidence': {'kind': 'live_builder', 'workload': {'uid': 'b' * 64}}})]
        for changes in variants:
            persisted = dict(copy.deepcopy(self.persisted), **changes)
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                contract.validate_recovered_operation(self.original, self.restored, persisted, 'after_start', ['a' * 64])
        with self.assertRaises(ValueError):
            contract.validate_recovered_operation(self.original, self.restored, self.persisted, 'before_create', [])

    def test_original_history_query_survives_empty_current_listing(self):
        class Response:
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def read(self): return json.dumps({'code': '0000', 'data': self_body}).encode()
        self_body = self.restored
        with patch.object(contract.urllib.request, 'urlopen', return_value=Response()) as request:
            actual = contract.db_operation('http://owned', 'owned', 'user', 'original')
        self.assertEqual(actual, self.restored)
        self.assertIn('/operations/original?', request.call_args.args[0])


if __name__ == '__main__':
    unittest.main()
