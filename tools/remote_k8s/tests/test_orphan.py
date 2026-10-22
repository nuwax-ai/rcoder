import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import main
import orphan


class OrphanContractTests(unittest.TestCase):
    def bare(self):
        suite = orphan.Suite.__new__(orphan.Suite)
        suite.run_id = 'a' * 16
        suite.c = Mock(ns='rcoder-e2e-fixture', id='fixture')
        suite.guard = Mock()
        suite.deadline = time.monotonic() + 5
        suite.resources, suite.seeds, suite.intents, suite.errors = [], [], [], []
        suite.labels = {'rcoder.dev/orphan-run': suite.run_id, 'rcoder.dev/environment': 'fixture'}
        suite.claims, suite.checks, suite.baseline_pvcs = {}, {}, {}
        suite.save = Mock()
        suite.config = {'kubernetes_config': {'services': {'computer-agent-runner': {}, 'user-app-builder': {}}}}
        return suite

    def test_wrong_fixture_profile_fails_before_any_cluster_action(self):
        c = Mock(ns='rcoder-e2e-fixture')
        c.get.return_value = 'full'
        with self.assertRaisesRegex(ValueError, 'PROFILE=orphan'):
            orphan.Suite(c, {'fixture_profile': 'full'}, {})
        c.kube.assert_not_called()

    def test_complete_verdict_requires_all_cases_and_assertions(self):
        checks = {name: dict.fromkeys(required, True) for name, required in orphan.CASE_REQUIRED.items()}
        self.assertTrue(orphan.complete_cases(checks))
        del checks['canonical_bad_json']['bad_json_retained']
        self.assertFalse(orphan.complete_cases(checks))
        checks['canonical_bad_json']['bad_json_retained'] = False
        self.assertFalse(orphan.complete_cases(checks))

    def test_fault_case_names_do_not_alias_same_registration(self):
        suite = self.bare()
        cases = ['canonical_container_without_project', 'canonical_missing_uid', 'canonical_bad_json']
        self.assertEqual(len({suite.key(case)[1] for case in cases}), 3)

    def test_cleanup_refuses_a_same_name_successor_without_delete(self):
        suite = self.bare()
        suite.get = Mock(return_value={'metadata': {'uid': 'new', 'labels': {'rcoder.dev/orphan-run': suite.run_id}}})
        suite.kube = Mock()
        with self.assertRaisesRegex(RuntimeError, 'identity changed'):
            suite.exact_delete({'kind': 'Service', 'name': 'original', 'uid': 'old'})
        suite.kube.assert_not_called()

    def test_cleanup_does_not_delete_pvc_or_replay_unknown_create(self):
        suite = self.bare()
        suite.get = Mock(return_value=None)
        suite.kube = Mock(side_effect=TimeoutError('create reply lost'))
        with self.assertRaises(TimeoutError):
            suite.create({'apiVersion': 'v1', 'kind': 'Service', 'metadata': {'name': 'unknown'}})
        self.assertEqual(len(suite.intents), 1)
        self.assertNotIn('uid', suite.intents[0])
        suite.cleanup()
        self.assertFalse(suite.checks['cleanup']['exact_owned_compute_gone'])
        self.assertEqual(suite.kube.call_count, 1)
        with self.assertRaises(KeyError):
            orphan.deletion_path('PersistentVolumeClaim', suite.c.ns, 'data')

    def test_unknown_delete_is_only_observed_not_sent_twice(self):
        suite = self.bare()
        target = {'kind': 'Service', 'name': 'original', 'uid': 'old'}
        suite.get = Mock(return_value={'metadata': {'uid': 'old', 'labels': {'rcoder.dev/orphan-run': suite.run_id}}})
        suite.kube = Mock(side_effect=TimeoutError('delete reply lost'))
        with self.assertRaises(TimeoutError):
            suite.exact_delete(target)
        suite.wait = Mock(side_effect=TimeoutError('still present'))
        with self.assertRaises(TimeoutError):
            suite.exact_delete(target)
        self.assertEqual(suite.kube.call_count, 1)

    def test_observation_io_failure_is_not_treated_as_absent(self):
        suite = self.bare()
        with self.assertRaisesRegex(RuntimeError, 'forbidden'):
            suite.wait(Mock(side_effect=RuntimeError('forbidden')))

    def test_formal_suite_executes_its_frozen_driver_without_chat_filter(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            driver = root / 'tools/remote_k8s/orphan.py'
            driver.parent.mkdir(parents=True)
            driver.write_text('def run(c, receipt, context, guard):\n context["orphan_driver_ran"] = True\n')
            context = {'snapshot_path': root, 'snapshot_record': {}, 'report_dir': root}
            with patch.object(main.test_snapshot, 'verify'), patch.object(main, 'run_chat_suite') as chat, \
                    patch.object(main, 'run_userapp_suite') as userapp:
                main.execute_suites(Mock(), {}, context, 'orphan')
            self.assertTrue(context['orphan_driver_ran'])
            chat.assert_not_called()
            userapp.assert_not_called()


if __name__ == '__main__':
    unittest.main()
