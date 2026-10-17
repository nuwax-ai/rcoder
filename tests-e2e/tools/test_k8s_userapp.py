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


class DevReadonlyLocatorTests(unittest.TestCase):
    """Harness guards with fake clients, never evidence of real K8s success."""

    def run_fixture(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.user, run.id = 'app', 'life', 'user', 'a' * 32
        run.entries = ['http://first', 'http://second']
        run.save = Mock()
        return run

    def operation(self, **changes):
        return {'app_id': 'app', 'lifecycle_id': 'life', 'scope': 'Dev', 'action': 'stop',
                'operation_id': 'operation', 'state': 'succeeded', 'stage': 'completed', **changes}

    def controller(self):
        return {'kind': 'StatefulSet', 'metadata': {'name': 'rcoder-app-builder-app',
                'uid': 'controller', 'resourceVersion': '1',
                'labels': {'rcoder.io/identifier': 'app', 'rcoder.io/service-type': 'user-app-builder'},
                'annotations': {'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'life',
                                'unrelated.example/keep': 'preserved'}}, 'spec': {'replicas': 0}}

    def stopped_logs(self):
        return {'code': 'ERR_BACKEND_ERROR', 'success': False,
                'message': 'locate development logs: Development container for app app is not running; its file logs are currently unavailable'}

    def annotation_run(self):
        run = self.run_fixture()
        run.dev_readonly_annotation = {'uid': 'controller', 'key': 'rcoder.e2e/readonly-test', 'values': set()}
        return run

    def test_compute_views_reject_all_unexpected_states_and_identity_changes(self):
        self.assertTrue(acceptance.dev_compute_view(self.operation(), 'app', 'life', 'stop', 'operation'))
        for state in ('pending', 'running'):
            self.assertFalse(acceptance.dev_compute_view(self.operation(state=state), 'app', 'life', 'stop', 'operation'))
        for state in ('failed', 'superseded', 'recovery_required', 'future_state', None):
            with self.subTest(state=state), self.assertRaises(AssertionError):
                acceptance.dev_compute_view(self.operation(state=state), 'app', 'life', 'stop', 'operation')
        for key in ('app_id', 'lifecycle_id', 'scope', 'action', 'operation_id', 'stage'):
            with self.subTest(key=key), self.assertRaises(AssertionError):
                acceptance.dev_compute_view(self.operation(**{key: 'foreign'}), 'app', 'life', 'stop', 'operation')

    def test_compute_admission_and_poll_keep_original_identity(self):
        for status in (200, 202):
            run = self.run_fixture()
            run.request = Mock(return_value=(status, {'code': '0000', 'success': True, 'operation_id': 'operation',
                               'data': self.operation(state='pending')}))
            run.api = Mock(return_value=self.operation())
            with patch.object(acceptance.time, 'sleep'):
                self.assertEqual(run.dev_compute('stop', 120)['operation_id'], 'operation')
            path, body, entry = run.request.call_args.args
            self.assertEqual((path, entry), ('/computer/pod/stop', 'http://first'))
            self.assertEqual(body['app_stage'], 'dev')
            self.assertEqual(body['service_type'], 'userapp')
            self.assertEqual(body['lifecycle_id'], 'life')
            self.assertTrue(body['request_id'])
            self.assertEqual(run.api.call_args.args, ('/computer/pod/operations/app/operation',))
            self.assertEqual(run.api.call_args.kwargs['base'], 'http://second')

    def test_late_or_foreign_compute_success_is_not_accepted(self):
        run = self.run_fixture()
        clock = [0.0]
        def late(*_args, **_kwargs):
            clock[0] = 121
            return 202, {'code': '0000', 'success': True, 'operation_id': 'operation', 'data': self.operation()}
        run.request = Mock(side_effect=late)
        with patch.object(acceptance.time, 'monotonic', side_effect=lambda: clock[0]), self.assertRaises(TimeoutError):
            run.dev_compute('stop', 120)
        run.request = Mock(return_value=(202, {'code': '0000', 'success': True, 'operation_id': 'foreign', 'data': self.operation()}))
        with self.assertRaises(AssertionError):
            run.dev_compute('stop', 120)
        run.request.reset_mock()
        with self.assertRaises(ValueError):
            run.dev_compute('unknown', 120)
        run.request.assert_not_called()

    def test_controller_guard_requires_names_uids_ownership_and_lifecycle(self):
        self.assertEqual(acceptance.dev_controller_identity(self.controller(), 'app', 'life', 'controller')['uid'], 'controller')
        for section, key, value in [('metadata', 'uid', 'replacement'), ('metadata', 'name', 'foreign'),
                                    ('metadata', 'resourceVersion', ''), ('metadata', 'deletionTimestamp', 'now'),
                                    ('labels', 'rcoder.io/identifier', 'foreign'),
                                    ('labels', 'rcoder.io/service-type', 'user-app'),
                                    ('annotations', 'rcoder.io/lifecycle-id', 'foreign')]:
            row = self.controller()
            (row['metadata'] if section == 'metadata' else row['metadata'][section])[key] = value
            with self.subTest(key=key), self.assertRaises(AssertionError):
                acceptance.dev_controller_identity(row, 'app', 'life', 'controller')

    def test_annotation_patch_has_uid_rv_and_only_owned_key(self):
        run = self.annotation_run()
        before, after = self.controller(), self.controller()
        after['metadata']['resourceVersion'] = '2'
        after['metadata']['annotations']['rcoder.e2e/readonly-test'] = 'issued'
        run.dev_controller = Mock(return_value=before)
        run.kube = Mock(return_value=json.dumps(after))
        self.assertEqual(run.patch_readonly_annotation('controller', 'issued', None), '2')
        args = run.kube.call_args.args
        patch = json.loads(args[args.index('-p') + 1])
        self.assertEqual(patch, {'metadata': {'uid': 'controller', 'resourceVersion': '1',
                         'annotations': {'rcoder.e2e/readonly-test': 'issued'}}})
        self.assertNotIn('spec', patch)
        self.assertEqual(before['metadata']['annotations']['unrelated.example/keep'], 'preserved')

    def test_unknown_patch_result_remains_eligible_for_exact_cleanup(self):
        run = self.annotation_run()
        run.dev_controller = Mock(return_value=self.controller())
        run.kube = Mock(side_effect=OSError('transport outcome unknown'))
        with self.assertRaises(OSError):
            run.patch_readonly_annotation('controller', 'issued', None)
        self.assertEqual(run.dev_readonly_annotation['values'], {'issued'})

    def test_annotation_removal_tests_uid_rv_and_current_value(self):
        run = self.annotation_run()
        before, after = self.controller(), self.controller()
        before['metadata']['annotations']['rcoder.e2e/readonly-test'] = 'issued'
        run.dev_readonly_annotation['values'].add('issued')
        run.dev_controller = Mock(return_value=before)
        run.kube = Mock(return_value=json.dumps(after))
        run.remove_readonly_annotation()
        args = run.kube.call_args.args
        patch = json.loads(args[args.index('-p') + 1])
        self.assertEqual(patch, [{'op': 'test', 'path': '/metadata/uid', 'value': 'controller'},
                 {'op': 'test', 'path': '/metadata/resourceVersion', 'value': '1'},
                 {'op': 'test', 'path': '/metadata/annotations/rcoder.e2e~1readonly-test', 'value': 'issued'},
                 {'op': 'remove', 'path': '/metadata/annotations/rcoder.e2e~1readonly-test'}])
        self.assertIsNone(run.dev_readonly_annotation)

    def test_foreign_annotation_is_never_replaced_or_removed(self):
        for action in ('patch', 'remove'):
            run = self.annotation_run()
            before = self.controller()
            before['metadata']['annotations']['rcoder.e2e/readonly-test'] = 'foreign'
            run.dev_controller, run.kube = Mock(return_value=before), Mock()
            with self.subTest(action=action), self.assertRaises(AssertionError):
                if action == 'patch':
                    run.patch_readonly_annotation('controller', 'issued', None)
                else:
                    run.remove_readonly_annotation()
            run.kube.assert_not_called()

    def test_stopped_view_rejects_any_replica_or_remaining_pod(self):
        run = self.run_fixture()
        run.dev_controller = Mock(return_value=self.controller())
        run.inventory = Mock(return_value=[{'kind': 'PersistentVolumeClaim', 'name': 'workspace', 'uid': 'claim'}])
        self.assertEqual(run.stopped_dev_view('controller', None)['claims'], [('workspace', 'claim')])
        row = self.controller()
        row['spec']['replicas'] = 1
        run.dev_controller.return_value = row
        with self.assertRaises(AssertionError):
            run.stopped_dev_view('controller', None)
        run.dev_controller.return_value = self.controller()
        run.inventory.return_value += [{'kind': 'Pod', 'name': 'rcoder-app-builder-app-0', 'uid': 'replacement'}]
        with self.assertRaises(AssertionError):
            run.stopped_dev_view('controller', None)

    def test_stopped_pair_requires_filtered_nonorphan_and_actual_log_diagnostic(self):
        run = self.run_fixture()
        run.api = Mock(return_value={'items': [{'app_id': 'app', 'exists': True, 'is_orphan': False}]})
        run.request = Mock(return_value=(200, self.stopped_logs()))
        self.assertTrue(run.readonly_dev_pair('http://first', None)['storage_nonorphan'])
        self.assertEqual(run.api.call_args.args, ('/api/v1/userapp/storage/dev/query',
                         {'page': 1, 'page_size': 10, 'filters': {'app_ids': ['app']}}, 'http://first'))
        for items in ([], [{'app_id': 'foreign', 'exists': True, 'is_orphan': False}],
                      [{'app_id': 'app', 'exists': True, 'is_orphan': True}]):
            run.api.return_value = {'items': items}
            with self.subTest(items=items), self.assertRaises(AssertionError):
                run.readonly_dev_pair('http://first', None)
        for change in ({'code': '0000'}, {'success': True}, {'message': 'Builder changed while verifying missing address'}):
            self.assertFalse(acceptance.stopped_dev_logs(200, {**self.stopped_logs(), **change}))
        self.assertFalse(acceptance.stopped_dev_logs(500, self.stopped_logs()))

    def scenario_fixture(self):
        run = self.run_fixture()
        run.builder_uid = 'old-pod'
        run.request = Mock(return_value=(200, {'success': True}))
        run.dev_git = Mock(return_value={'commit': 'b' * 40})
        run.dev_data_proof = Mock(return_value={'sentinel': 'actual-test-client-proof'})
        run.dev_controller = Mock(return_value=self.controller())
        run.inventory = Mock(return_value=[{'kind': 'PersistentVolumeClaim', 'name': 'workspace', 'uid': 'claim'}])
        run.check, run.dev_compute = Mock(), Mock(return_value=self.operation())
        run.stopped_dev_view = Mock(return_value={'claims': [('workspace', 'claim')]})
        run.patch_readonly_annotation = Mock(return_value='2')
        run.remove_readonly_annotation = Mock()
        run.readonly_dev_pair = Mock(side_effect=OSError('read failed'))
        return run

    def restart_scenario_fixture(self):
        run = self.scenario_fixture()
        run.assertions = []
        run.check = acceptance.Run.check.__get__(run)
        run.dev_controller.return_value['spec']['replicas'] = 1
        run.dev_compute.side_effect = [self.operation(), self.operation(action='restart')]
        run.readonly_dev_pair.side_effect = lambda entry, deadline: {
            'entry': run.entries.index(entry), 'storage_nonorphan': True, 'logs_stopped': True}
        run.patch_readonly_annotation.side_effect = ['2', '3', '4', '5']
        run.api = Mock(return_value={'lifecycle_id': 'life', 'lifecycle_epoch': 1, 'state': 'Active'})
        return run

    def restarting_pod(self, ready=False, uid='new-pod'):
        return {'kind': 'Pod', 'name': 'rcoder-app-builder-app-0', 'uid': uid,
                'phase': 'Running', 'deletionTimestamp': None,
                'conditions': [{'type': 'Ready', 'status': 'True' if ready else 'False'}]}

    def test_succeeded_restart_waits_for_new_pod_ready_before_asserting_identity(self):
        run = self.restart_scenario_fixture()
        claim = {'kind': 'PersistentVolumeClaim', 'name': 'workspace', 'uid': 'claim'}
        snapshots = iter([[claim], [claim, self.restarting_pod()], [claim, self.restarting_pod(True)]])
        run.inventory.side_effect = lambda **kwargs: next(snapshots, [claim, self.restarting_pod(True)])
        with patch.object(acceptance.time, 'sleep'):
            run.dev_readonly_locator()
        self.assertGreaterEqual(run.inventory.call_count, 4)
        self.assertEqual(run.builder_uid, 'new-pod')
        self.assertEqual(run.dev_data_proof.call_count, 2)
        self.assertTrue(next(row['ok'] for row in run.assertions if row['id'] == 'dev_readonly_data_preserved'))

    def restart_observer_fixture(self):
        run = self.run_fixture()
        run.builder_uid = 'old-pod'
        row = self.controller()
        row['spec']['replicas'] = 1
        run.kube = Mock(return_value=json.dumps(row))
        return run

    def test_compute_uses_external_deadline_for_dispatch_and_original_poll(self):
        run = self.run_fixture()
        clock = [10.0]
        def dispatch(*args, **kwargs):
            clock[0] = 20
            return 202, {'code': '0000', 'success': True, 'operation_id': 'operation',
                         'data': self.operation(action='restart', state='pending')}
        def completed(*args, **kwargs):
            clock[0] = 30
            return self.operation(action='restart')
        run.request, run.api = Mock(side_effect=dispatch), Mock(side_effect=completed)
        with patch.object(acceptance.time, 'monotonic', side_effect=lambda: clock[0]), patch.object(acceptance.time, 'sleep'):
            self.assertEqual(run.dev_compute('restart', 999, deadline=40)['state'], 'succeeded')
        self.assertEqual(run.request.call_args.kwargs['timeout'], 30)
        self.assertEqual(run.api.call_args.kwargs['timeout'], 20)

    def test_restart_ready_observations_and_compute_share_one_scenario_deadline(self):
        run = self.restart_scenario_fixture()
        claim = {'kind': 'PersistentVolumeClaim', 'name': 'workspace', 'uid': 'claim'}
        snapshots = iter([[claim], [claim, self.restarting_pod(True)]])
        run.inventory.side_effect = lambda **kwargs: next(snapshots, [claim, self.restarting_pod(True)])
        with patch.object(acceptance.time, 'monotonic', return_value=10):
            run.dev_readonly_locator()
        self.assertEqual(run.dev_compute.call_args.kwargs, {'deadline': 310})
        self.assertEqual(run.inventory.call_args_list[1].kwargs, {'deadline': 310})
        self.assertEqual(run.dev_controller.call_args.kwargs, {})
        self.assertEqual(run.dev_controller.call_args.args, ('controller', 310))

    def test_restart_observer_waits_only_for_missing_pending_or_not_ready_pod(self):
        run = self.restart_observer_fixture()
        claim = {'kind': 'PersistentVolumeClaim', 'name': 'workspace', 'uid': 'claim'}
        pending = {**self.restarting_pod(), 'phase': 'Pending'}
        run.inventory = Mock(side_effect=[[claim], [claim, pending],
                             [claim, self.restarting_pod()], [claim, self.restarting_pod(True)]])
        with patch.object(acceptance.time, 'sleep') as sleep:
            controller, pods = run.observe_restarted_dev('controller', [('workspace', 'claim')],
                                                         acceptance.time.monotonic() + 10)
        self.assertEqual((controller['metadata']['uid'], pods[0]['uid']), ('controller', 'new-pod'))
        self.assertEqual(sleep.call_count, 3)
        self.assertEqual(run.save.call_count, 4)

    def test_restart_observer_rejects_late_ready_after_query_returns(self):
        run = self.restart_observer_fixture()
        claim = {'kind': 'PersistentVolumeClaim', 'name': 'workspace', 'uid': 'claim'}
        clock = [0.0]
        views = iter([(0.6, False), (1.1, True)])
        def inventory(**kwargs):
            clock[0], ready = next(views)
            return [claim, self.restarting_pod(ready)]
        run.inventory = Mock(side_effect=inventory)
        with patch.object(acceptance.time, 'monotonic', side_effect=lambda: clock[0]), \
             patch.object(acceptance.time, 'sleep', side_effect=lambda duration: clock.__setitem__(0, clock[0] + duration)), \
             self.assertRaisesRegex(TimeoutError, 'last observation'):
            run.observe_restarted_dev('controller', [('workspace', 'claim')], 1)
        self.assertEqual(run.inventory.call_count, 2)
        self.assertEqual(run.save.call_args.args[1]['pods'][0]['conditions'][0]['status'], 'True')

    def test_restart_observer_expired_budget_does_no_io(self):
        run = self.restart_observer_fixture()
        run.inventory = Mock()
        with patch.object(acceptance.time, 'monotonic', return_value=10), self.assertRaises(TimeoutError):
            run.observe_restarted_dev('controller', [('workspace', 'claim')], 10)
        run.kube.assert_not_called()
        run.inventory.assert_not_called()

    def test_restart_observer_fails_on_controller_pod_or_volume_identity_changes(self):
        claim = {'kind': 'PersistentVolumeClaim', 'name': 'workspace', 'uid': 'claim'}
        cases = {
            'old-pod': [[claim, self.restarting_pod(True, 'old-pod')]],
            'multiple-pods': [[claim, self.restarting_pod(), self.restarting_pod(True, 'other')]],
            'replacement-during-observation': [[claim, self.restarting_pod()],
                                              [claim, self.restarting_pod(True, 'other')]],
            'changed-pvc': [[{**claim, 'uid': 'other'}, self.restarting_pod(True)]],
            'deleting-pod': [[claim, {**self.restarting_pod(True), 'deletionTimestamp': 'now'}]],
            'unknown-phase': [[claim, {**self.restarting_pod(True), 'phase': 'Unknown'}]],
        }
        for name, observations in cases.items():
            run = self.restart_observer_fixture()
            run.inventory = Mock(side_effect=observations)
            with self.subTest(name=name), patch.object(acceptance.time, 'sleep'), self.assertRaises(AssertionError):
                run.observe_restarted_dev('controller', [('workspace', 'claim')], acceptance.time.monotonic() + 10)
        for field in ('uid', 'lifecycle', 'replicas'):
            run = self.restart_observer_fixture()
            row = self.controller()
            row['spec']['replicas'] = 2 if field == 'replicas' else 1
            if field == 'uid':
                row['metadata']['uid'] = 'replacement'
            elif field == 'lifecycle':
                row['metadata']['annotations']['rcoder.io/lifecycle-id'] = 'replacement'
            run.kube.return_value = json.dumps(row)
            run.inventory = Mock(return_value=[claim, self.restarting_pod(True)])
            with self.subTest(field=field), self.assertRaises(AssertionError):
                run.observe_restarted_dev('controller', [('workspace', 'claim')], acceptance.time.monotonic() + 10)

    def test_read_failure_cleans_annotation_before_outer_cleanup_and_never_restarts(self):
        run = self.scenario_fixture()
        with self.assertRaisesRegex(OSError, 'read failed'):
            run.dev_readonly_locator()
        run.remove_readonly_annotation.assert_called_once_with()
        self.assertEqual(run.dev_compute.call_args_list[0].args, ('stop', 120))
        self.assertEqual(run.dev_compute.call_count, 1)
        self.assertFalse(any(call.args[0] == 'dev_readonly_data_preserved' for call in run.check.call_args_list))

    def test_scenario_and_cleanup_errors_keep_both_causes(self):
        run = self.scenario_fixture()
        run.remove_readonly_annotation.side_effect = RuntimeError('conditional cleanup failed')
        with self.assertRaisesRegex(RuntimeError, 'conditional cleanup failed') as caught:
            run.dev_readonly_locator()
        self.assertIsInstance(caught.exception.__cause__, OSError)
        report = run.save.call_args.args[1]
        self.assertIn('read failed', report['scenario_error'])
        self.assertIn('conditional cleanup failed', report['annotation_cleanup_error'])

    def test_two_replicas_and_lifecycle_are_required_before_any_mutation(self):
        for entries, lifecycle in [([], 'life'), (['http://same', 'http://same'], 'life'),
                                   (['http://first', 'http://second'], None)]:
            run = self.run_fixture()
            run.entries, run.lifecycle_id, run.request = entries, lifecycle, Mock()
            with self.subTest(entries=entries), self.assertRaises(AssertionError):
                run.dev_readonly_locator()
            run.request.assert_not_called()

    def test_additional_ready_replicas_do_not_fail_existing_launcher_contract(self):
        run = self.scenario_fixture()
        run.entries.append('http://third')
        with self.assertRaisesRegex(OSError, 'read failed'):
            run.dev_readonly_locator()
        self.assertEqual({call.args[0] for call in run.readonly_dev_pair.call_args_list}, {'http://first', 'http://second'})
        run.remove_readonly_annotation.assert_called_once_with()


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
