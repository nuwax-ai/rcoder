import json
from pathlib import Path
import tempfile
import unittest
from run import validate_reports

class ReportTests(unittest.TestCase):
    def test_failed_assertion_cannot_be_hidden_by_pass_terminal(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            events = [{'kind': 'assert', 'level': 'hard', 'ok': False}, {'kind': 'scenario_end', 'verdict': 'pass'}]
            (root / 'a.jsonl').write_text('\n'.join(map(json.dumps, events)))
            self.assertTrue(validate_reports(root))

    def test_missing_reports_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            self.assertTrue(validate_reports(Path(temp)))

    def test_all_report_files_must_finish(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            good = [{'kind': 'scenario_begin'}, {'kind': 'assert', 'level': 'hard', 'ok': True}, {'kind': 'scenario_end', 'verdict': 'pass', 'hard_pass': 1, 'hard_fail': 0}]
            (root / 'a.jsonl').write_text('\n'.join(map(json.dumps, good)))
            self.assertFalse(validate_reports(root))
            for verdict in ['skip', 'aborted', 'fail']:
                (root / 'b.jsonl').write_text(json.dumps({'kind': 'scenario_end', 'verdict': verdict}))
                self.assertTrue(validate_reports(root))
            (root / 'b.jsonl').write_text(json.dumps({'kind': 'scenario_begin'}))
            self.assertTrue(validate_reports(root))

    def test_early_success_cannot_hide_required_steps(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rows = [{'kind': 'scenario_begin'}, {'kind': 'assert', 'name': 'A accepted', 'level': 'hard', 'ok': True}, {'kind': 'scenario_end', 'verdict': 'pass', 'hard_pass': 1, 'hard_fail': 0}]
            (root / 'a.jsonl').write_text('\n'.join(map(json.dumps, rows)))
            self.assertTrue(validate_reports(root, 'userapp_hot_deployment_builtin_contract'))

    def test_zero_assertions_and_duplicate_terminal_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'a.jsonl'
            end = json.dumps({'kind': 'scenario_end', 'verdict': 'pass'})
            path.write_text(end)
            self.assertTrue(validate_reports(path.parent))
            path.write_text(end + '\n' + end)
            self.assertTrue(validate_reports(path.parent))

if __name__ == '__main__':
    unittest.main()
