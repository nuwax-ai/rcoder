import json
from pathlib import Path
import tempfile
import subprocess
import sys
import time
import unittest
import signal
from unittest.mock import patch
import run
from run import validate_reports, terminate_process_group
from contracts import REQUIRED

class ReportTests(unittest.TestCase):
    def test_first_cancellation_during_settle_or_cleanup_is_deferred(self):
        for phase in ('settle', 'cleanup'):
            for signum in (signal.SIGINT, signal.SIGTERM):
                with self.subTest(phase=phase, signal=signum):
                    events = []
                    original = signal.getsignal(signum)
                    def settle(_process):
                        events.append('settle begin')
                        if phase == 'settle':
                            signal.raise_signal(signum)
                        events.append('settle end')
                    def cleanup():
                        events.append('cleanup begin')
                        if phase == 'cleanup':
                            signal.raise_signal(signum)
                        events.append('cleanup end')
                        return ['retained cleanup evidence']
                    with patch('run.terminate_process_group', side_effect=settle):
                        errors, exit_code, interrupted = run.settle_case(object(), cleanup)
                    self.assertEqual(events, ['settle begin', 'settle end', 'cleanup begin', 'cleanup end'])
                    self.assertEqual(errors, ['retained cleanup evidence'])
                    self.assertEqual(exit_code, 130)
                    self.assertTrue(interrupted)
                    self.assertEqual(signal.getsignal(signum), original)

    def test_cancellation_at_process_handoff_and_wait_exit_cannot_bypass_cleanup(self):
        for phase in ('launch', 'wait exit'):
            for signum in (signal.SIGINT, signal.SIGTERM):
                with self.subTest(phase=phase, signal=signum):
                    events = []
                    class Process:
                        def wait(self, timeout):
                            events.append('wait')
                            if phase == 'wait exit':
                                signal.raise_signal(signum)
                            return 0
                    process = Process()
                    def launch(*args, **kwargs):
                        events.append('launch')
                        if phase == 'launch':
                            signal.raise_signal(signum)
                        return process
                    def cleanup():
                        events.append('cleanup')
                        return []
                    with patch('run.subprocess.Popen', side_effect=launch), patch('run.terminate_process_group', side_effect=lambda p: events.append('settle')):
                        errors, status, cancelled = run.execute_case([], {}, None, cleanup)
                    self.assertEqual(status, 130)
                    self.assertTrue(cancelled)
                    self.assertEqual(errors, [])
                    self.assertEqual(events[-2:], ['settle', 'cleanup'])

    def test_wait_error_still_runs_owned_cleanup(self):
        class Process:
            def wait(self, timeout):
                raise OSError('wait failed')
        with patch('run.subprocess.Popen', return_value=Process()), patch('run.terminate_process_group') as settle:
            cleanup = unittest.mock.Mock(return_value=[])
            errors, status, interrupted = run.execute_case([], {}, None, cleanup)
        settle.assert_called_once()
        cleanup.assert_called_once()
        self.assertNotEqual(status, 0)
        self.assertFalse(interrupted)
        self.assertTrue(errors)

    def test_settle_without_cancellation_preserves_failure_and_cleanup(self):
        with patch('run.terminate_process_group') as settle:
            errors, exit_code, interrupted = run.settle_case(object(), lambda: ['cleanup failed'], 124)
        settle.assert_called_once()
        self.assertEqual(errors, ['cleanup failed'])
        self.assertEqual(exit_code, 124)
        self.assertFalse(interrupted)

    def test_cancellation_waits_for_cleanup_child_after_parent_exit(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            child = root / 'child.py'
            child.write_text("import signal,time,pathlib,sys\ndef cleanup(*args):\n time.sleep(0.2)\n pathlib.Path(sys.argv[2]).touch()\n raise SystemExit(0)\nsignal.signal(signal.SIGTERM,cleanup)\npathlib.Path(sys.argv[1]).touch()\nwhile True: time.sleep(0.1)\n")
            ready, done = root / 'ready', root / 'done'
            parent = subprocess.Popen([sys.executable, '-c', 'import subprocess,sys; subprocess.Popen(sys.argv[1:])', sys.executable, str(child), str(ready), str(done)], start_new_session=True)
            try:
                deadline = time.monotonic() + 5
                while not ready.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(ready.exists())
                parent.wait(timeout=5)
                terminate_process_group(parent, grace=5)
                self.assertTrue(done.exists(), 'cleanup child must finish before fallback cleanup')
            finally:
                terminate_process_group(parent, grace=1)

    def test_failed_assertion_cannot_be_hidden_by_pass_terminal(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            events = [{'kind': 'assert', 'level': 'hard', 'ok': False}, {'kind': 'scenario_end', 'verdict': 'pass'}]
            (root / 'a.jsonl').write_text('\n'.join(map(json.dumps, events)))
            self.assertTrue(validate_reports(root))

    def test_unregistered_scenario_fails_even_with_passing_assertion(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rows = [{'kind': 'scenario_begin'}, {'kind': 'assert', 'name': 'anything', 'level': 'hard', 'ok': True}, {'kind': 'scenario_end', 'verdict': 'pass', 'hard_pass': 1, 'hard_fail': 0}]
            (root / 'a.jsonl').write_text('\n'.join(map(json.dumps, rows)))
            self.assertTrue(validate_reports(root, 'unregistered_new_scenario'))

    def test_passing_steps_from_wrong_scenario_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scenario = 'pg_storage_lifecycle_contract'
            rows = [{'kind': 'scenario_begin', 'scenario': 'wrong-scenario', 'backend': 'wrong-backend'}]
            rows.extend({'kind': 'assert', 'name': name, 'level': 'hard', 'ok': True} for name in REQUIRED[scenario])
            rows.append({'kind': 'scenario_end', 'verdict': 'pass', 'hard_pass': len(REQUIRED[scenario]), 'hard_fail': 0})
            (root / 'a.jsonl').write_text('\n'.join(map(json.dumps, rows)))
            self.assertTrue(validate_reports(root, scenario))

    def test_run_case_and_canonical_identity_must_match_every_row(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scenario = 'pg_storage_lifecycle_contract'
            rows = [{'kind': 'scenario_begin', 'scenario': scenario, 'backend': 'postgres17'}]
            rows.extend({'kind': 'assert', 'name': name, 'level': 'hard', 'ok': True} for name in REQUIRED[scenario])
            rows.append({'kind': 'scenario_end', 'verdict': 'pass', 'hard_pass': len(REQUIRED[scenario]), 'hard_fail': 0})
            for row in rows:
                row.update(run_id='run', case_id='case', test_name=scenario)
            path = root / 'a.jsonl'
            def write():
                path.write_text('\n'.join(map(json.dumps, rows)))
            write()
            self.assertFalse(validate_reports(root, scenario, 'run', 'case'))
            for field in ('run_id', 'case_id', 'test_name'):
                value = rows[1].pop(field)
                write()
                self.assertTrue(validate_reports(root, scenario, 'run', 'case'))
                rows[1][field] = value

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
