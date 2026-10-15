"""Local safety/report gates for the real-cluster launcher; no external I/O."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch

import k8s_userapp as acceptance


def holder_view(state='Running'):
    return {'operation_id': 'original-holder', 'kind': 'StartDeployment', 'state': state,
            'step': 'hot_execution', 'scope': 'Prod'}


def busy_response():
    return {'code': 'ERR_OPERATION_IN_PROGRESS', 'success': False, 'message': '不解析文案',
            'blocker': holder_view(), 'data': {
                'holder_operation_id': 'original-holder', 'holder_kind': 'start_deployment',
                'holder_traffic_wake': False, 'holder_state': 'running', 'holder_step': 'hot_execution',
                'retryable': True, 'retry_after_seconds': 45}}


class BusyContractTests(unittest.TestCase):
    def test_busy_data_matches_same_snapshot_and_original_holder(self):
        response = busy_response()
        self.assertTrue(acceptance.busy_matches_holder(200, response, holder_view()))
        # The live view can progress; the response's blocker remains its snapshot.
        later = {**holder_view(), 'state': 'Succeeded', 'step': 'completed'}
        self.assertTrue(acceptance.busy_matches_holder(200, response, later))
        self.assertFalse(acceptance.busy_matches_holder(200, response, {**later, 'operation_id': 'foreign'}))
        self.assertFalse(acceptance.busy_matches_holder(409, response, holder_view()))

    def test_missing_or_ambiguous_busy_fields_do_not_pass(self):
        for field in busy_response()['data']:
            with self.subTest(field=field):
                response = busy_response()
                del response['data'][field]
                self.assertFalse(acceptance.valid_busy_response(200, response))
        for field, value in [('holder_operation_id', 'foreign'), ('holder_kind', 'hot_deploy'),
                             ('holder_state', 'pending'), ('holder_step', 'another_step')]:
            with self.subTest(field=field):
                response = busy_response()
                response['data'][field] = value
                self.assertFalse(acceptance.valid_busy_response(200, response))

    def test_retry_preserves_request_identity_and_input(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.request = Mock(side_effect=[(200, busy_response()),
                                       (200, {'code': '0000', 'success': True, 'data': 'deleted'})])
        body = {'user_id': 'owner', 'purge': True}
        with patch.object(acceptance.time, 'sleep') as sleep:
            self.assertEqual(run.api_retry_operation_busy('/delete', body), 'deleted')
        first, second = [call.args[1] for call in run.request.call_args_list]
        self.assertEqual(first, second)
        self.assertTrue(first['request_id'])
        self.assertEqual(body, {'user_id': 'owner', 'purge': True})
        sleep.assert_called_once_with(45)

    def test_real_conflicts_and_runtime_failures_are_never_retried(self):
        for code in ('ERR_CONFLICT', 'ERR_RUNTIME_UNAVAILABLE', 'ERR_OPERATION_OUTCOME_UNKNOWN'):
            for reason in ('lifecycle conflict', 'resource version CAS', 'request fingerprint changed'):
                with self.subTest(code=code, reason=reason):
                    run = acceptance.Run.__new__(acceptance.Run)
                    run.request = Mock(return_value=(200, {'code': code, 'success': False, 'message': reason}))
                    with patch.object(acceptance.time, 'sleep') as sleep, self.assertRaises(AssertionError):
                        run.api_retry_operation_busy('/delete', {'request_id': 'same'})
                    run.request.assert_called_once()
                    sleep.assert_not_called()

    def test_unknown_protected_or_already_accepted_holder_is_not_retried(self):
        unknown = busy_response()
        unknown['data'] = {**unknown['data'], 'holder_operation_id': None, 'holder_kind': None,
                           'holder_state': None, 'holder_step': None, 'retryable': False, 'retry_after_seconds': 0}
        protected = busy_response()
        protected['blocker']['state'] = 'RecoveryRequired'
        protected['data'].update(holder_state='recovery_required', retryable=False, retry_after_seconds=0)
        accepted = busy_response()
        accepted['operation_id'] = 'own-accepted-operation'
        mismatch = busy_response()
        mismatch['data']['holder_operation_id'] = 'foreign'
        for response in (unknown, protected, accepted, mismatch):
            with self.subTest(response=response):
                run = acceptance.Run.__new__(acceptance.Run)
                run.request = Mock(return_value=(200, response))
                with patch.object(acceptance.time, 'sleep') as sleep, self.assertRaises(AssertionError):
                    run.api_retry_operation_busy('/delete', {'request_id': 'same'})
                run.request.assert_called_once()
                sleep.assert_not_called()

    def test_busy_retry_uses_one_total_deadline(self):
        clock = [0.0]
        run = acceptance.Run.__new__(acceptance.Run)
        run.request = Mock(return_value=(200, busy_response()))
        with patch.object(acceptance.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(acceptance.time, 'sleep', side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)), \
                self.assertRaises(AssertionError):
            run.api_retry_operation_busy('/delete', {'request_id': 'same'}, attempts=4, interval=10)
        self.assertEqual(clock[0], 40)
        run.request.assert_called_once()
        self.assertEqual(run.request.call_args.kwargs['timeout'], 40)


class RestartAdmissionTests(unittest.TestCase):
    def own(self):
        return {'operation_id': 'own-restart', 'kind': 'RestartDeployment', 'scope': 'Prod', 'state': 'Succeeded'}

    def test_budget_exhausted_busy_requires_no_accepted_operation(self):
        self.assertTrue(acceptance.restart_result_matches(200, busy_response(), holder_view(), holder_view(), None, 30.2))
        self.assertFalse(acceptance.restart_result_matches(200, busy_response(), holder_view(), holder_view(), None, .03))
        self.assertFalse(acceptance.restart_result_matches(200, busy_response(), holder_view(), holder_view(), self.own(), 30.2))
        self.assertFalse(acceptance.restart_result_matches(200, busy_response(), holder_view(), holder_view(), None, 80))

    def test_success_requires_distinct_original_request_operation_after_holder(self):
        response = {'code': '0000', 'success': True, 'operation_id': 'own-restart'}
        self.assertTrue(acceptance.restart_result_matches(200, response, holder_view(), holder_view('Succeeded'), self.own(), 50))
        self.assertFalse(acceptance.restart_result_matches(200, response, holder_view(), holder_view(), self.own(), 50))
        self.assertFalse(acceptance.restart_result_matches(200, response, holder_view(), holder_view('Succeeded'),
                                                        {**self.own(), 'operation_id': 'original-holder'}, 50))
        self.assertFalse(acceptance.restart_result_matches(200, response, holder_view(), holder_view('Succeeded'),
                                                        {**self.own(), 'state': 'Failed'}, 50))
        # /restart without URL still belongs to the deployment controller. An
        # independent compute-control Restart receipt cannot prove this request.
        self.assertFalse(acceptance.restart_result_matches(200, response, holder_view(), holder_view('Succeeded'),
                                                        {**self.own(), 'kind': 'Restart'}, 50))

    def test_admission_monitor_rejects_overlap_and_holder_id_reuse(self):
        self.assertTrue(acceptance.restart_admission_after_holder(None, holder_view()))
        self.assertFalse(acceptance.restart_admission_after_holder(self.own(), holder_view()))
        self.assertTrue(acceptance.restart_admission_after_holder(self.own(), holder_view('Succeeded')))
        self.assertFalse(acceptance.restart_admission_after_holder(
            {**self.own(), 'operation_id': 'original-holder'}, holder_view('Succeeded')))


class RestObservationTests(unittest.TestCase):
    def run_fixture(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.user, run.id = 'app', 'life', 'user', 'run'
        run.entries = ['http://first', 'http://second']
        run.args = Mock(proxy_url='http://proxy')
        return run

    def operation(self, **changes):
        return {'operation_id': 'operation', 'app_id': 'app', 'lifecycle_id': 'life',
                'request_id': 'request', 'kind': 'Stop', 'scope': 'Prod',
                'state': 'Failed', 'error_code': 'ERR_OPERATION_IN_PROGRESS', **changes}

    def pod(self, **changes):
        return {'kind': 'Pod', 'name': 'rcoder-app-app-new', 'uid': 'new', 'phase': 'Running',
                'labels': {'rcoder.io/app-id': 'app', 'app.kubernetes.io/instance': 'app',
                           'app.kubernetes.io/managed-by': 'rcoder-app-manager'},
                'annotations': {'rcoder.io/restart-operation': 'operation'},
                'conditions': [{'type': 'Ready', 'status': 'True'}],
                'images': [{'ready': True}], 'deletionTimestamp': None, **changes}

    def test_by_request_absence_requires_explicit_successful_null(self):
        run = self.run_fixture()
        run.request = Mock(return_value=(200, {'code': '0000', 'success': True, 'data': None}))
        self.assertIsNone(run.operation_view('request'))
        for status, response in [(200, {'code': '0000', 'success': True}),
                                 (200, {'code': 'ERR_NOT_FOUND', 'success': False, 'data': None}),
                                 (404, {'code': 'ERR_NOT_FOUND', 'success': False, 'data': None}),
                                 (200, {'code': '0000', 'success': False, 'data': None}),
                                 (200, {'code': '0000', 'success': True, 'data': []})]:
            with self.subTest(status=status, response=response):
                run.request = Mock(return_value=(status, response))
                with self.assertRaises(AssertionError):
                    run.operation_view('request')
        for field in ('app_id', 'lifecycle_id', 'request_id'):
            run.request = Mock(return_value=(200, {'code': '0000', 'success': True,
                                                 'data': self.operation(**{field: 'foreign'})}))
            with self.subTest(field=field), self.assertRaises(AssertionError):
                run.operation_view('request')
        run.request = Mock(side_effect=OSError('read failed'))
        with self.assertRaises(OSError):
            run.operation_view('request')

    def test_only_original_rejected_kind_and_scope_prove_no_execution(self):
        self.assertTrue(acceptance.rejected_without_execution(None, 'Stop'))
        self.assertTrue(acceptance.rejected_without_execution(self.operation(), 'Stop'))
        self.assertTrue(acceptance.rejected_without_execution(self.operation(kind='DeleteCompute'), 'DeleteCompute'))
        for changes in [{'kind': 'DeleteCompute'}, {'scope': 'Dev'}, {'state': 'Running'},
                        {'state': 'Succeeded'}, {'state': 'RecoveryRequired'},
                        {'error_code': 'ERR_CONFLICT'}]:
            with self.subTest(changes=changes):
                self.assertFalse(acceptance.rejected_without_execution(self.operation(**changes), 'Stop'))

    def test_rollout_requires_new_ready_pod_from_original_restart(self):
        self.assertTrue(acceptance.restarted_pod_ready(self.pod(), 'app', 'old', 'operation'))
        for changes in [{'uid': 'old'}, {'phase': 'Pending'}, {'conditions': []}, {'images': []},
                        {'images': [{'ready': False}]}, {'deletionTimestamp': 'deleting'},
                        {'labels': {'rcoder.io/app-id': 'foreign'}},
                        {'annotations': {'rcoder.io/restart-operation': 'foreign'}}]:
            with self.subTest(changes=changes):
                self.assertFalse(acceptance.restarted_pod_ready(self.pod(**changes), 'app', 'old', 'operation'))

    def test_rollout_and_content_share_original_deadline(self):
        run = self.run_fixture()
        clock = [0.0]
        run.inventory = Mock(side_effect=[[self.pod(uid='old')],
                                          [self.pod(uid='old'), self.pod()],
                                          [self.pod()], [self.pod()]])
        run.request = Mock(side_effect=[(200, b'run-A'), (200, b'run-B')])
        with patch.object(acceptance.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(acceptance.time, 'sleep', side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)):
            observed = run.observe_restart_rollout('old', 'operation', 1)
        self.assertEqual(observed['uid'], 'new')
        self.assertGreater(clock[0], 0)
        self.assertLess(clock[0], 1)
        self.assertTrue(all(call.kwargs['deadline'] == 1 for call in run.inventory.call_args_list))
        self.assertTrue(all(0 < call.kwargs['timeout'] <= 1 for call in run.request.call_args_list))

    def test_exhausted_deadline_or_read_errors_never_fall_back(self):
        run = self.run_fixture()
        run.inventory, run.request = Mock(), Mock()
        with patch.object(acceptance.time, 'monotonic', return_value=1), self.assertRaises(TimeoutError):
            run.observe_restart_rollout('old', 'operation', 1)
        run.inventory.assert_not_called()
        run.request.assert_not_called()
        for method, error in [('inventory', OSError('inventory failed')), ('request', OSError('backend failed'))]:
            run.inventory = Mock(return_value=[self.pod()])
            run.request = Mock(return_value=(503, b'backend error'))
            getattr(run, method).side_effect = error
            with patch.object(acceptance.time, 'monotonic', return_value=0), self.subTest(method=method), self.assertRaises(OSError):
                run.observe_restart_rollout('old', 'operation', 1)
        run.inventory = Mock(return_value=[self.pod()])
        run.request = Mock(return_value=(503, b'backend error'))
        with patch.object(acceptance.time, 'monotonic', return_value=0), self.assertRaises(AssertionError):
            run.observe_restart_rollout('old', 'operation', 1)

    def test_read_that_crosses_deadline_cannot_report_success(self):
        run = self.run_fixture()
        clock = [0.0]
        run.inventory = Mock(return_value=[self.pod()])
        def late_content(*_args, **_kwargs):
            clock[0] = 2
            return 200, b'run-B'
        run.request = Mock(side_effect=late_content)
        with patch.object(acceptance.time, 'monotonic', side_effect=lambda: clock[0]), self.assertRaises(TimeoutError):
            run.observe_restart_rollout('old', 'operation', 1)
        clock[0] = 0
        run.request = Mock(side_effect=lambda *_args, **_kwargs: (clock.__setitem__(0, 2),
                           (200, {'code': '0000', 'success': True, 'data': None}))[1])
        with patch.object(acceptance.time, 'monotonic', side_effect=lambda: clock[0]), self.assertRaises(TimeoutError):
            run.operation_view('request', deadline=1)

    def test_replay_deployment_preserves_original_identity_and_generation(self):
        run = self.run_fixture()
        deployment = {'kind': 'Deployment', 'name': 'rcoder-app-app', 'uid': 'deployment', 'generation': 3,
                      'labels': {'app.kubernetes.io/instance': 'app',
                                 'app.kubernetes.io/managed-by': 'rcoder-app-manager'},
                      'annotations': {'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'life'}}
        run.inventory = Mock(side_effect=[[deployment], [{**deployment, 'generation': 4}]])
        with patch.object(acceptance.time, 'monotonic', return_value=0):
            before = run.replay_deployment_view(1)
            after = run.replay_deployment_view(1)
        self.assertNotEqual(before, after, 'another rollout cannot pass while the previous Pod is still Ready')
        for changed in [{**deployment, 'annotations': {'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'foreign'}},
                        {**deployment, 'uid': ''}, {**deployment, 'generation': None}]:
            run.inventory = Mock(return_value=[changed])
            with patch.object(acceptance.time, 'monotonic', return_value=0), self.assertRaises(AssertionError):
                run.replay_deployment_view(1)

    def test_ssh_deadline_propagates_without_changing_arguments(self):
        run = self.run_fixture()
        run.args.context, run.args.namespace, run.args.ssh = 'default', 'nuwax-k8s-test', 'test-user@192.0.2.10'
        run.local = Mock(return_value='inventory')
        self.assertEqual(run.kube('get', 'pods', deadline=10), 'inventory')
        self.assertEqual(run.local.call_args.args, ('ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
                                                  'test-user@192.0.2.10', 'kubectl --context default -n nuwax-k8s-test get pods'))
        self.assertEqual(run.local.call_args.kwargs, {'deadline': 10})
        with patch.object(acceptance.subprocess, 'check_output', return_value='result') as output, \
                patch.object(acceptance.time, 'monotonic', return_value=3):
            self.assertEqual(acceptance.Run.local(run, 'ssh', deadline=10), 'result')
        self.assertEqual(output.call_args.kwargs['timeout'], 7)


class ReportTests(unittest.TestCase):
    def report(self, root):
        report = acceptance.Run.__new__(acceptance.Run)
        report.root = Path(root)
        report.report_lock = threading.RLock()
        report.assertions = []
        return report

    def test_resource_evidence_excludes_pod_environment(self):
        evidence = acceptance.compact_resource({
            'kind': 'Pod', 'metadata': {'name': 'app', 'uid': 'uid'},
            'spec': {'containers': [{'env': [{'name': 'TOKEN', 'value': 'private'}]}]},
            'status': {'containerStatuses': [{'name': 'app', 'imageID': 'sha256:actual', 'ready': True}]},
        })
        self.assertNotIn('private', json.dumps(evidence))
        self.assertEqual(evidence['images'][0]['imageID'], 'sha256:actual')

    def test_reports_redact_nested_credentials(self):
        with tempfile.TemporaryDirectory() as root:
            report = self.report(root)
            report.save('record.json', {'data': {'env': {'APP_CLI_TOKEN': 'private', 'PORT': '3010'}, 'password': 'private'}})
            data = json.loads((Path(root) / 'record.json').read_text())
            self.assertNotIn('private', json.dumps(data))
            self.assertEqual(data['data']['env']['PORT'], '3010')

    def test_nonfatal_failure_is_retained_while_later_steps_continue(self):
        with tempfile.TemporaryDirectory() as root, contextlib.redirect_stdout(io.StringIO()):
            report = self.report(root)
            report.check('concurrent_ensure', False, fatal=False)
            report.check('ensure_retry', True)
            rows = json.loads((Path(root) / 'assertions.json').read_text())
            self.assertEqual([row['ok'] for row in rows], [False, True])

    def test_empty_scenario_cannot_pass(self):
        class EmptyRun(acceptance.Run):
            def local(self, *_args):
                return 'test-head'
            def prepare(self):
                pass
            def workspace(self):
                pass
            def build(self, _version):
                return {}, ''
            def deploy(self, *_args):
                pass
            def cleanup(self):
                self.check('cleanup', True)
        with tempfile.TemporaryDirectory() as root, \
                patch.object(acceptance, 'ROOT', Path(root)), \
                patch.object(acceptance, 'Run', EmptyRun), \
                patch('run.source_fingerprint', return_value='test-fingerprint'), \
                patch('signal.signal'), \
                patch('sys.argv', ['k8s_userapp.py', '--ssh', 'test', '--url', 'http://test', '--proxy-url', 'http://test']), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(acceptance.main(), 1)
            summary = json.loads(next(Path(root).glob('reports/*/summary.json')).read_text())
            self.assertEqual(summary['verdict'], 'fail')
            self.assertIn('cold_deploy', summary['missing'])


if __name__ == '__main__':
    unittest.main()
