"""Local safety/report gates for the real-cluster launcher; no external I/O."""
import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
import urllib.error
from unittest.mock import Mock, patch
from types import SimpleNamespace

import k8s_userapp as acceptance


class ClusterTransportTests(unittest.TestCase):
    def test_explicit_server_keeps_normal_tls_verification(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.args = SimpleNamespace(context='test-context', kube_server='https://cluster.example:6443',
                                   namespace='rcoder-e2e-example', ssh='test-host')
        run.local = Mock(return_value='{}')
        run.kube('get', 'pods', '-o', 'json')
        command = run.local.call_args.args[-1]
        self.assertIn('--server https://cluster.example:6443', command)
        self.assertIn('--context test-context', command)
        self.assertNotIn('insecure-skip-tls-verify', command)


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


class RecreatedLifecycleTests(unittest.TestCase):
    def claim(self, **changes):
        return {'kind': 'PersistentVolumeClaim', 'name': 'rcoder-app-builder-app-workspace',
                'uid': 'new-claim', 'labels': {'app.kubernetes.io/managed-by': 'rcoder-runtime',
                                              'service_type': 'user-app-builder'},
                'annotations': {'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'life-two'},
                **changes}

    def operation(self, action='stop', lifecycle='life-one', operation_id=None):
        return {'operation_id': operation_id or 'original-' + action, 'app_id': 'app',
                'lifecycle_id': lifecycle, 'scope': 'Dev', 'action': action,
                'state': 'succeeded', 'stage': 'completed', 'revision': 10,
                'error_code': 'ORIGINAL_DIAGNOSTIC', 'error_message': 'preserve original diagnostic'}

    def test_pvc_capture_requires_exact_new_userapp_lifecycle_and_family(self):
        run_claim = self.claim()
        self.assertEqual(acceptance.owned_userapp_pvc_identities([run_claim], 'app', 'life-two', {'baseline'}),
                         [('rcoder-app-builder-app-workspace', 'new-claim')])
        for row in [self.claim(name='agent-app-workspace'), self.claim(uid='baseline'), self.claim(uid=''),
                    self.claim(annotations={'rcoder.io/application-id': 'other', 'rcoder.io/lifecycle-id': 'life-two'}),
                    self.claim(annotations={'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'life-one'}),
                    self.claim(labels={'app.kubernetes.io/managed-by': 'rcoder-runtime',
                                       'service_type': 'computer-agent-runner'})]:
            with self.subTest(row=row), self.assertRaises(AssertionError):
                acceptance.owned_userapp_pvc_identities([row], 'app', 'life-two', {'baseline'})
        with self.assertRaises(AssertionError):
            acceptance.owned_userapp_pvc_identities([run_claim, run_claim], 'app', 'life-two', set())

    def test_history_reads_original_ids_and_requires_entire_receipt_unchanged(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.entries = 'app', ['http://first', 'http://second']
        receipts = [self.operation(), self.operation('restart')]
        run.api = Mock(side_effect=receipts)
        self.assertEqual(run.compute_history(receipts, 'life-one'), receipts)
        self.assertEqual(run.api.call_args_list[0].args[0], '/computer/pod/operations/app/original-stop')
        self.assertEqual(run.api.call_args_list[0].kwargs['base'], 'http://second')
        # A normal terminal lease-bookkeeping write may advance only revision.
        advanced = {**receipts[0], 'revision': receipts[0]['revision'] + 1}
        run.api = Mock(return_value=advanced)
        self.assertEqual(run.compute_history(receipts[:1], 'life-one'), [advanced])
        self.assertEqual(receipts[0]['revision'], 10, 'keep the original raw response intact')
        for field, value in [('lifecycle_id', 'life-two'), ('operation_id', 'replacement'),
                             ('revision', 9), ('revision', True), ('error_message', None),
                             ('error_code', None), ('app_id', 'foreign'), ('action', 'restart'),
                             ('state', 'superseded'), ('stage', 'stopped')]:
            run.api = Mock(return_value={**receipts[0], field: value})
            with self.subTest(field=field), self.assertRaises(AssertionError):
                run.compute_history(receipts[:1], 'life-one')

    def test_history_revision_bookkeeping_preserves_all_other_public_fields(self):
        original = self.operation()
        advanced = {**original, 'revision': original['revision'] + 1}
        self.assertTrue(acceptance.historical_compute_view_preserved(original, advanced))
        self.assertFalse(acceptance.historical_compute_view_preserved(advanced, original))
        self.assertFalse(acceptance.historical_compute_view_preserved(original, {**advanced, 'new_field': 'changed'}))
        missing = dict(advanced)
        missing.pop('error_message')
        self.assertFalse(acceptance.historical_compute_view_preserved(original, missing))
        self.assertEqual(original['revision'], 10)

    def test_foreign_or_unsettled_history_is_rejected_before_query(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.entries, run.api = 'app', ['http://first', 'http://second'], Mock()
        for field, value in [('app_id', 'foreign'), ('lifecycle_id', 'life-two'), ('operation_id', None),
                             ('scope', 'Prod'), ('action', 'unknown'), ('state', 'running')]:
            with self.subTest(field=field), self.assertRaises(AssertionError):
                run.compute_history([{**self.operation(), field: value}], 'life-one')
        run.api.assert_not_called()

    def test_l2_compute_uses_distinct_request_and_evidence_prefix(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.id = 'app', 'life-two', 'run'
        run.entries, run.save = ['http://first', 'http://second'], Mock()
        view = self.operation(lifecycle='life-two', operation_id='l2-stop')
        run.request = Mock(return_value=(202, {'code': '0000', 'success': True,
                                               'operation_id': 'l2-stop', 'data': view}))
        result = run.dev_compute('stop', 120, request_id='recreate-stop-run', evidence_prefix='recreate-dev')
        self.assertEqual(result, view)
        self.assertEqual(run.request.call_args.args[1]['lifecycle_id'], 'life-two')
        self.assertEqual(run.request.call_args.args[1]['request_id'], 'recreate-stop-run')
        self.assertEqual(run.save.call_args.args[0], 'recreate-dev-stop-operation.json')
        self.assertEqual(run.compute_receipts, [view])
        run.request.return_value[1]['data']['revision'] = 99
        self.assertEqual(run.compute_receipts[0]['revision'], 10)

    def test_stale_requests_require_conflict_and_no_accepted_operation(self):
        response = {'code': 'ERR_CONFLICT', 'success': False}
        self.assertTrue(acceptance.stale_lifecycle_rejected(409, response, compute=True))
        self.assertTrue(acceptance.stale_lifecycle_rejected(200, response))
        for status, change in [(200, {}), (500, {}), (409, {'success': True}),
                               (409, {'code': 'ERR_OPERATION_IN_PROGRESS'}), (409, {'operation_id': 'accepted'})]:
            with self.subTest(status=status, change=change):
                self.assertFalse(acceptance.stale_lifecycle_rejected(status, {**response, **change}, compute=True))

    def business_fixture(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.user, run.id = 'app', 'life-two', 'user', 'run'
        run.entries = ['http://first', 'http://second']
        def check(name, ok, detail=None):
            if not ok:
                raise AssertionError(name)
        run.save, run.check = Mock(), Mock(side_effect=check)
        pod = {'kind': 'Pod', 'name': 'rcoder-app-builder-app-0', 'uid': 'l2-builder'}
        claims = [('rcoder-app-builder-app-workspace', 'new-claim')]
        run.capture_recreated_resources = Mock(return_value=([pod, self.claim()], claims))
        run.dev_controller = Mock(return_value={'metadata': {'uid': 'l2-controller'}, 'spec': {'replicas': 1}})
        run.api = Mock(return_value={'operation_id': 'l2-deploy'})
        def request(path, *args, **kwargs):
            if path.startswith('/computer/pod/'):
                return 409, {'code': 'ERR_CONFLICT', 'success': False}
            if path.endswith('/start'):
                return 200, {'code': 'ERR_CONFLICT', 'success': False}
            return 200, {'success': True}
        run.request = Mock(side_effect=request)
        run.dev_git = Mock(return_value={'commit': 'c' * 40})
        run.dev_data_proof = Mock(return_value={'sentinel': 'retained'})
        run.build = Mock(return_value=({'url': 'http://artifact', 'sha256': 'a' * 64}, 'release-L2'))
        run.operation_view = Mock(return_value={'operation_id': 'l2-deploy', 'kind': 'StartDeployment',
                                                'scope': 'Prod', 'state': 'Succeeded'})
        run.content = Mock(return_value=True)
        run.recreated_runtime_identity = Mock(return_value={'resources': 'same L2 resource identities'})
        run.inventory = Mock(return_value=[self.claim()])
        run.dev_compute = Mock(side_effect=[self.operation(lifecycle='life-two', operation_id='l2-stop'),
                                             self.operation('restart', 'life-two', 'l2-restart')])
        run.stopped_dev_view = Mock(return_value={'claims': claims})
        run.observe_restarted_dev = Mock(return_value=(None, [{**pod, 'uid': 'l2-restarted-builder'}]))
        run.lifecycle = Mock(return_value={'lifecycle_id': 'life-two', 'state': 'Active'})
        history = [self.operation(), self.operation('restart')]
        run.compute_history = Mock(return_value=history)
        return run, history

    def test_recreated_business_reaches_build_http_compute_and_history(self):
        run, history = self.business_fixture()
        run.recreated_business('life-one', history, [('old-workspace', 'old-pvc')])
        run.build.assert_called_once_with('L2')
        self.assertEqual(run.api.call_args.args[1]['lifecycle_id'], 'life-two')
        self.assertEqual([call.args[1]['lifecycle_id'] for call in run.request.call_args_list
                          if call.args[0].startswith('/computer/pod/')], ['life-one', 'life-one'])
        self.assertEqual([call.kwargs['request_id'] for call in run.dev_compute.call_args_list],
                         ['recreate-stop-run', 'recreate-restart-run'])
        run.compute_history.assert_called_once_with(history, 'life-one')
        self.assertEqual(run.builder_uid, 'l2-restarted-builder')
        self.assertIn('recreate_history_preserved', [call.args[0] for call in run.check.call_args_list])

    def test_stale_l1_acceptance_or_physical_change_fails_before_l2_compute(self):
        for physical_change in (False, True):
            run, history = self.business_fixture()
            if physical_change:
                run.recreated_runtime_identity.side_effect = ['before', 'changed']
            else:
                original_request = run.request.side_effect
                run.request.side_effect = lambda path, *args, **kwargs: ((202, {
                    'code': '0000', 'success': True, 'operation_id': 'wrongly-admitted'})
                    if path.startswith('/computer/pod/') else original_request(path, *args, **kwargs))
            with self.subTest(physical_change=physical_change), self.assertRaises(AssertionError):
                run.recreated_business('life-one', history, [])
            run.dev_compute.assert_not_called()

    def test_recreated_public_history_must_exist_for_both_completed_actions(self):
        for history in ([], [self.operation()]):
            run, _original = self.business_fixture()
            run.compute_history.return_value = history
            with self.subTest(history=history), self.assertRaisesRegex(AssertionError, 'recreate_history_preserved'):
                run.recreated_business('life-one', history, [])

    def cleanup_fixture(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.user, run.id = 'app', 'life-one', 'user', 'run'
        run.created, run.baseline, run.compute_receipts = True, [], [self.operation(), self.operation('restart')]
        run.inventory = Mock(return_value=[])
        run.save, run.check, run.api_retry_operation_busy = Mock(), Mock(), Mock()
        run.lifecycle = Mock(return_value={'lifecycle_id': 'life-one', 'lifecycle_epoch': 1, 'state': 'Deleted'})
        run.request = Mock(return_value=(200, {'code': 'ERR_CONFLICT', 'success': False}))
        run.api = Mock(return_value={'lifecycle_id': 'life-two', 'lifecycle_epoch': 2, 'state': 'Active'})
        run.recreated_business, run.cleanup_recreated, run.preserve_baseline = Mock(), Mock(), Mock()
        return run

    def test_recreated_business_failure_still_cleans_l2_through_original_api(self):
        run = self.cleanup_fixture()
        observed = []
        run.recreated_business.side_effect = AssertionError('L2 build failed')
        run.cleanup_recreated.side_effect = lambda: observed.append(run.lifecycle_id)
        with self.assertRaisesRegex(AssertionError, 'L2 build failed'):
            run.recreate_lifecycle()
        run.cleanup()
        self.assertEqual(observed, ['life-two'])
        run.preserve_baseline.assert_called_once()
        self.assertEqual(run.recreated_business.call_args.args[0], 'life-one')
        self.assertEqual([row['operation_id'] for row in run.recreated_business.call_args.args[1]],
                         ['original-stop', 'original-restart'])

    def test_finally_cleanup_never_executes_recreate_business(self):
        run = self.cleanup_fixture()
        run.recreated, run.lifecycle_id = True, 'life-two'
        run.cleanup_recreated.side_effect = OSError('original API cleanup failed')
        with self.assertRaisesRegex(OSError, 'original API cleanup failed'):
            run.cleanup()
        run.recreated_business.assert_not_called()
        run.api.assert_not_called()
        run.preserve_baseline.assert_called_once()

    def test_recreated_cleanup_receipt_precedes_purge_and_binds_current_lifecycle(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.user, run.id = 'app', 'life-two', 'user', 'run'
        events = []
        run.capture_recreated_resources = Mock(side_effect=lambda filename: (
            events.append(('capture', filename)) or [{'kind': 'Deployment', 'name': 'rcoder-app-app'}], []))
        run.api_retry_operation_busy = Mock(side_effect=lambda path, body: events.append((path, body)))
        run.lifecycle = Mock(return_value={'state': 'Deleted', 'lifecycle_id': 'life-two'})
        run.poll, run.check = Mock(return_value=[]), Mock()
        run.cleanup_recreated()
        self.assertEqual(events[0], ('capture', 'recreate-before-cleanup.json'))
        self.assertEqual(events[1][0], '/api/v1/userapp/app/prod/delete')
        self.assertEqual(events[2][0], '/api/v1/userapp/app/delete/app')
        self.assertEqual({k: v for k, v in events[1][1].items() if k != 'request_id'},
                         {'user_id': 'user', 'lifecycle_id': 'life-two', 'purge': True})
        self.assertEqual({k: v for k, v in events[2][1].items() if k != 'request_id'},
                         {'user_id': 'user', 'lifecycle_id': 'life-two'})
        self.assertNotEqual(events[1][1]['request_id'], events[2][1]['request_id'])

    def test_delete_business_and_finally_reuse_requests_without_cross_lifecycle_alias(self):
        run = self.cleanup_fixture()
        rows = [{'kind': 'Deployment', 'name': 'rcoder-app-app'}]
        run.delete_owned_resources(rows)
        first = run.api_retry_operation_busy.call_args_list[-2:]
        run.delete_owned_resources(rows)
        self.assertEqual(first, run.api_retry_operation_busy.call_args_list[-2:])
        run.lifecycle_id = 'life-two'
        run.delete_owned_resources(rows)
        for old, current in zip(first, run.api_retry_operation_busy.call_args_list[-2:]):
            self.assertNotEqual(old.args[1]['request_id'], current.args[1]['request_id'])

    def test_unowned_recreated_pvc_prevents_destructive_cleanup(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.user, run.baseline = 'app', 'life-two', 'user', []
        run.inventory = Mock(return_value=[self.claim(annotations={'rcoder.io/lifecycle-id': 'life-one'})])
        run.save, run.api_retry_operation_busy = Mock(), Mock()
        with self.assertRaises(AssertionError):
            run.cleanup_recreated()
        run.api_retry_operation_busy.assert_not_called()
        run.save.assert_called_once()

    def test_recreated_cleanup_refuses_baseline_workload_uid_not_only_pvc(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.user = 'app', 'life-two', 'user'
        run.baseline = [{'uid': 'preexisting-controller'}]
        run.inventory = Mock(return_value=[{'kind': 'Deployment', 'name': 'rcoder-app-app',
                                           'uid': 'preexisting-controller'}])
        run.save, run.api_retry_operation_busy = Mock(), Mock()
        with self.assertRaisesRegex(AssertionError, 'preexisting resource'):
            run.cleanup_recreated()
        run.api_retry_operation_busy.assert_not_called()


class TrafficWakeTests(unittest.TestCase):
    def run_fixture(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.app, run.lifecycle_id, run.user, run.id = 'app', 'life-one', 'user', 'run'
        run.args, run.baseline = Mock(proxy_url='http://proxy'), []
        run.proxy_entries = ['http://replica-a', 'http://replica-b']
        run.dev_sentinel = ('sentinel', 'commit', {'sentinel': 'retained'})
        run.dev_data_proof = Mock(return_value={'sentinel': 'retained'})
        run.save, run.check, run.content = Mock(), Mock(), Mock(return_value=True)
        run.prod_pod = Mock(return_value={'uid': 'new-pod', 'phase': 'Running',
                                          'conditions': [{'type': 'Ready', 'status': 'True'}]})
        claim = RecreatedLifecycleTests().claim(annotations={
            'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'life-one'})
        run.inventory = Mock(return_value=[claim])
        return run, [(claim['name'], claim['uid'])]

    def strict_check(self, name, ok, detail=None):
        if not ok:
            raise AssertionError(name)

    def test_first_two_requests_are_business_traffic_before_explicit_start(self):
        run, claims = self.run_fixture()
        events, lock = [], threading.Lock()
        def request(path, entry, deadline):
            with lock:
                events.append(('traffic', path, entry))
            return 200, b'run-B'
        run.first_proxy_get = Mock(side_effect=request)
        run.api = Mock(side_effect=lambda path, body, **kwargs: events.append(('start', path, body)))
        result = run.traffic_wake({'uid': 'old-pod'}, claims, {'sentinel': 'retained'})
        self.assertEqual(result['uid'], 'new-pod')
        self.assertEqual([row[0] for row in events], ['traffic', 'traffic', 'start'])
        self.assertTrue(all(row[1] == '/api/v1/userapp/proxy/app/prod/user/app/' for row in events[:2]))
        self.assertEqual({row[2] for row in events[:2]}, {'http://replica-a', 'http://replica-b'})

    def test_first_request_failure_does_not_hidden_start_or_poll_until_green(self):
        run, claims = self.run_fixture()
        run.check = self.strict_check
        run.first_proxy_get = Mock(return_value=(503, b'not ready'))
        run.api = Mock()
        with self.assertRaisesRegex(AssertionError, 'traffic_wake_without_start'):
            run.traffic_wake({'uid': 'old-pod'}, claims, {'sentinel': 'retained'})
        self.assertEqual(run.first_proxy_get.call_count, 2)
        run.api.assert_not_called()

    def test_changed_pvc_or_source_data_fails_before_explicit_start(self):
        for changed_claims, changed_data in [(True, False), (False, True)]:
            run, claims = self.run_fixture()
            run.check = self.strict_check
            run.first_proxy_get = Mock(return_value=(200, b'run-B'))
            run.api = Mock()
            if changed_claims:
                claims = [(claims[0][0], 'old-pvc')]
            if changed_data:
                run.dev_data_proof.return_value = {'sentinel': 'changed'}
            with self.subTest(changed_claims=changed_claims), \
                    self.assertRaisesRegex(AssertionError, 'traffic_wake_pvc_data_preserved'):
                run.traffic_wake({'uid': 'old-pod'}, claims, {'sentinel': 'retained'})
            run.api.assert_not_called()

    def test_same_pod_or_explicit_start_replacement_cannot_pass(self):
        for traffic_uid, explicit_uid in [('old-pod', 'old-pod'), ('new-pod', 'replacement-pod')]:
            run, claims = self.run_fixture()
            run.check, run.api = self.strict_check, Mock()
            run.first_proxy_get = Mock(return_value=(200, b'run-B'))
            row = {'uid': traffic_uid, 'phase': 'Running', 'conditions': [{'type': 'Ready', 'status': 'True'}]}
            run.prod_pod.side_effect = [row, {**row, 'uid': explicit_uid}]
            expected = 'traffic_wake_single_instance' if traffic_uid == 'old-pod' else 'wake_explicit_start_idempotent'
            with self.subTest(traffic_uid=traffic_uid), self.assertRaisesRegex(AssertionError, expected):
                run.traffic_wake({'uid': 'old-pod'}, claims, {'sentinel': 'retained'})

    def test_missing_or_repeated_replica_entries_fail_before_traffic(self):
        for entries in ([], ['http://only'], ['http://same', 'http://same']):
            run, claims = self.run_fixture()
            run.proxy_entries, run.first_proxy_get, run.api = entries, Mock(), Mock()
            with self.subTest(entries=entries), self.assertRaisesRegex(AssertionError, 'distinct captured'):
                run.traffic_wake({'uid': 'old-pod'}, claims, {'sentinel': 'retained'})
            run.first_proxy_get.assert_not_called()
            run.api.assert_not_called()

    def test_first_get_preserves_redirect_status_without_proxy_follow_or_retry(self):
        run, _claims = self.run_fixture()
        run.trace, run.requests = 'a' * 32, []
        response = urllib.error.HTTPError('http://replica-a/path', 302, 'redirect',
                                          {'Location': 'http://elsewhere'}, io.BytesIO(b'redirect'))
        opener = Mock()
        opener.open.side_effect = response
        with patch.object(acceptance.urllib.request, 'build_opener', return_value=opener) as build:
            self.assertEqual(run.first_proxy_get('/path', 'http://replica-a', acceptance.time.monotonic() + 10),
                             (302, b'redirect'))
        self.assertEqual(build.call_args.args[0].proxies, {})
        self.assertIsInstance(build.call_args.args[1], acceptance.NoProxyRedirect)
        self.assertIsNone(build.call_args.args[1].redirect_request(None, None, 302, '', {}, 'http://elsewhere'))
        opener.open.assert_called_once()
        self.assertEqual(run.requests[0]['attempts'], 1)
        self.assertFalse(run.requests[0]['redirects_followed'])
        self.assertFalse(run.requests[0]['environment_proxy'])


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
        run.args.kube_server = None
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
    def claim(self, **changes):
        return {'kind': 'PersistentVolumeClaim', 'name': 'rcoder-app-builder-app-workspace',
                'uid': 'claim', 'labels': {'app.kubernetes.io/managed-by': 'rcoder-runtime',
                                          'service_type': 'user-app-builder'},
                'annotations': {'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'life'},
                **changes}
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
        run.inventory = Mock(return_value=[self.claim()])
        self.assertEqual(run.stopped_dev_view('controller', None)['claims'], [('rcoder-app-builder-app-workspace', 'claim')])
        row = self.controller()
        row['spec']['replicas'] = 1
        run.dev_controller.return_value = row
        with self.assertRaises(AssertionError):
            run.stopped_dev_view('controller', None)
        run.dev_controller.return_value = self.controller()
        run.inventory.return_value += [{'kind': 'Pod', 'name': 'rcoder-app-builder-app-0', 'uid': 'replacement'}]
        with self.assertRaises(AssertionError):
            run.stopped_dev_view('controller', None)

    def test_other_app_new_pvc_is_allowed_but_baseline_uid_and_pv_stay_protected(self):
        run = self.restart_observer_fixture()
        existing = {'kind': 'PersistentVolumeClaim', 'name': 'existing-workspace',
                    'uid': 'baseline-pvc', 'volumeName': 'pv-original'}
        run.baseline = [existing]
        new_other = {'kind': 'PersistentVolumeClaim', 'name': 'another-workspace',
                     'uid': 'other-new-pvc', 'volumeName': 'other-new-pv'}
        rows = [self.claim(), existing, new_other, self.restarting_pod(True)]
        run.inventory = Mock(return_value=rows)
        _controller, pods = run.observe_restarted_dev('controller',
            [('rcoder-app-builder-app-workspace', 'claim')], acceptance.time.monotonic() + 10)
        self.assertEqual(pods[0]['uid'], 'new-pod')
        for before in ([], [{**existing, 'uid': 'replacement'}],
                       [{**existing, 'volumeName': 'pv-rebound'}], [{**existing, 'deletionTimestamp': 'now'}]):
            run.inventory = Mock(return_value=[self.claim(), *before, new_other, self.restarting_pod(True)])
            with self.subTest(before=before), self.assertRaisesRegex(AssertionError, 'baseline PVC'):
                run.observe_restarted_dev('controller', [('rcoder-app-builder-app-workspace', 'claim')],
                                          acceptance.time.monotonic() + 10)

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
        run.inventory = Mock(return_value=[self.claim()])
        run.check, run.dev_compute = Mock(), Mock(return_value=self.operation())
        run.stopped_dev_view = Mock(return_value={'claims': [('rcoder-app-builder-app-workspace', 'claim')]})
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
        claim = self.claim()
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
        claim = self.claim()
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
        claim = self.claim()
        pending = {**self.restarting_pod(), 'phase': 'Pending'}
        run.inventory = Mock(side_effect=[[claim], [claim, pending],
                             [claim, self.restarting_pod()], [claim, self.restarting_pod(True)]])
        with patch.object(acceptance.time, 'sleep') as sleep:
            controller, pods = run.observe_restarted_dev('controller', [('rcoder-app-builder-app-workspace', 'claim')],
                                                         acceptance.time.monotonic() + 10)
        self.assertEqual((controller['metadata']['uid'], pods[0]['uid']), ('controller', 'new-pod'))
        self.assertEqual(sleep.call_count, 3)
        self.assertEqual(run.save.call_count, 4)

    def test_restart_observer_rejects_late_ready_after_query_returns(self):
        run = self.restart_observer_fixture()
        claim = self.claim()
        clock = [0.0]
        views = iter([(0.6, False), (1.1, True)])
        def inventory(**kwargs):
            clock[0], ready = next(views)
            return [claim, self.restarting_pod(ready)]
        run.inventory = Mock(side_effect=inventory)
        with patch.object(acceptance.time, 'monotonic', side_effect=lambda: clock[0]), \
             patch.object(acceptance.time, 'sleep', side_effect=lambda duration: clock.__setitem__(0, clock[0] + duration)), \
             self.assertRaisesRegex(TimeoutError, 'last observation'):
            run.observe_restarted_dev('controller', [('rcoder-app-builder-app-workspace', 'claim')], 1)
        self.assertEqual(run.inventory.call_count, 2)
        self.assertEqual(run.save.call_args.args[1]['pods'][0]['conditions'][0]['status'], 'True')

    def test_restart_observer_expired_budget_does_no_io(self):
        run = self.restart_observer_fixture()
        run.inventory = Mock()
        with patch.object(acceptance.time, 'monotonic', return_value=10), self.assertRaises(TimeoutError):
            run.observe_restarted_dev('controller', [('rcoder-app-builder-app-workspace', 'claim')], 10)
        run.kube.assert_not_called()
        run.inventory.assert_not_called()

    def test_restart_observer_fails_on_controller_pod_or_volume_identity_changes(self):
        claim = self.claim()
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
                run.observe_restarted_dev('controller', [('rcoder-app-builder-app-workspace', 'claim')], acceptance.time.monotonic() + 10)
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
                run.observe_restarted_dev('controller', [('rcoder-app-builder-app-workspace', 'claim')], acceptance.time.monotonic() + 10)

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


class IndependentRecreateScenarioTests(unittest.TestCase):
    def test_l1_uses_real_business_sequence_and_original_compute_history(self):
        fixture = RecreatedLifecycleTests()
        run, _history = fixture.business_fixture()
        operations = [fixture.operation(lifecycle='life-two', operation_id='l1-stop'),
                      fixture.operation('restart', 'life-two', 'l1-restart')]
        run.dev_compute.side_effect = operations
        run.compute_history.return_value = operations
        run.recreate_l1_business()
        run.build.assert_called_once_with('L1')
        self.assertEqual(run.api.call_args.args[1]['lifecycle_id'], 'life-two')
        self.assertEqual([call.kwargs['request_id'] for call in run.dev_compute.call_args_list],
                         ['recreate-l1-stop-run', 'recreate-l1-restart-run'])
        run.compute_history.assert_called_once_with(operations, 'life-two')
        run.content.assert_any_call('run-L1')
        self.assertIn('recreate_l1_data_preserved', [call.args[0] for call in run.check.call_args_list])

    def test_l1_missing_git_or_changed_data_cannot_reach_recreate(self):
        for changed_data in (False, True):
            fixture = RecreatedLifecycleTests()
            run, _history = fixture.business_fixture()
            operations = [fixture.operation(lifecycle='life-two', operation_id='l1-stop'),
                          fixture.operation('restart', 'life-two', 'l1-restart')]
            run.dev_compute.side_effect = operations
            run.compute_history.return_value = operations
            if changed_data:
                run.dev_data_proof.side_effect = [{'sentinel': 'before'}, {'sentinel': 'changed'}]
            else:
                run.dev_git.return_value = {'commit': 'not-an-actual-Git-hash'}
            run.recreate_lifecycle = Mock()
            with self.subTest(changed_data=changed_data), self.assertRaises(AssertionError):
                run.recreate_l1_business()
                run.recreate_lifecycle()
            run.recreate_lifecycle.assert_not_called()
            if not changed_data:
                run.build.assert_not_called()
                run.dev_compute.assert_not_called()

    def test_default_all_retains_81_required_steps(self):
        self.assertEqual(len(acceptance.REQUIRED), 81)
        self.assertEqual(acceptance.SCENARIO_REQUIRED['all'], acceptance.REQUIRED)
        self.assertIn('traffic_wake_without_start', acceptance.REQUIRED)
        self.assertIn('recreate_history_preserved', acceptance.REQUIRED)
        self.assertNotIn('traffic_wake_without_start', acceptance.RECREATE_REQUIRED)
        self.assertTrue({'build_L1', 'artifact_L1', 'recreate_l1_files_git',
                         'recreate_l1_deploy_operation', 'recreate_l1_content',
                         'recreate_l1_compute_history', 'recreate_l1_data_preserved',
                         'build_L2', 'recreate_compute_stop', 'recreate_compute_restart',
                         'recreate_old_lifecycle_rejected', 'recreate_history_preserved',
                         'recreate_deleted', 'cleanup', 'baseline_preserved'}
                        <= acceptance.RECREATE_REQUIRED)

    def scenario_fixture(self, scenario):
        run = acceptance.Run.__new__(acceptance.Run)
        run.scenario = scenario
        calls = []
        for name in ('prepare', 'workspace', 'recreate_l1_business', 'dev_readonly_locator',
                     'recreate_lifecycle', 'traffic_wake', 'deploy'):
            setattr(run, name, Mock(side_effect=lambda *_args, name=name: calls.append(name)))
        run.build = Mock(side_effect=lambda version: (calls.append('build-' + version) or ({}, version)))
        return run, calls

    def test_recreate_is_independent_of_traffic_lock_and_readonly_scenarios(self):
        run, calls = self.scenario_fixture('recreate')
        run.exercise()
        self.assertEqual(calls, ['prepare', 'workspace', 'recreate_l1_business', 'recreate_lifecycle'])
        run.deploy.assert_not_called()
        run.build.assert_not_called()
        run.traffic_wake.assert_not_called()
        run.dev_readonly_locator.assert_not_called()

    def test_all_keeps_original_scenarios_before_explicit_recreate(self):
        run, calls = self.scenario_fixture('all')
        run.exercise()
        self.assertEqual(calls, ['prepare', 'workspace', 'dev_readonly_locator', 'build-A',
                                 'build-B', 'deploy', 'recreate_lifecycle'])
        run.recreate_l1_business.assert_not_called()

    def test_all_original_business_failure_does_not_get_bypassed(self):
        run, _calls = self.scenario_fixture('all')
        run.deploy.side_effect = AssertionError('traffic_wake_without_start')
        with self.assertRaisesRegex(AssertionError, 'traffic_wake_without_start'):
            run.exercise()
        run.recreate_lifecycle.assert_not_called()

    def test_report_identity_and_plan_are_scenario_specific(self):
        class OfflineRun(acceptance.Run):
            def local(self, *_args):
                return 'test-head'
        with tempfile.TemporaryDirectory() as directory, \
                patch.dict(os.environ, {'E2E_RUN_ROOT': directory}), \
                patch('run.source_fingerprint', return_value='frozen-source'), \
                contextlib.redirect_stdout(io.StringIO()):
            for scenario in acceptance.SCENARIO_REQUIRED:
                args = SimpleNamespace(scenario=scenario, url='http://test', namespace='rcoder-e2e-unit', ssh='test')
                run = OfflineRun(args)
                self.assertEqual(run.summary['scenario'], scenario)
                self.assertEqual(run.summary['report_identity'], {
                    'scenario': acceptance.SCENARIO_IDENTITIES[scenario], 'backend': 'k8s'})
                self.assertEqual(run.summary['planned'], sorted(acceptance.SCENARIO_REQUIRED[scenario]))
                self.assertEqual(run.summary['required'], run.summary['planned'])
                self.assertTrue(run.app.startswith('e2e-k8s-'))
                self.assertEqual(len(run.app), 22)

    def test_l2_business_and_cleanup_errors_remain_separate_in_terminal_report(self):
        class FailedRun(acceptance.Run):
            def local(self, *_args):
                return 'test-head'
            def exercise(self):
                raise AssertionError('original L2 workspace failed')
            def cleanup_owned(self):
                raise OSError('original API cleanup failed')
            def preserve_baseline(self):
                self.check('baseline_preserved', True)
        with tempfile.TemporaryDirectory() as directory, \
                patch.dict(os.environ, {'E2E_RUN_ROOT': directory}), \
                patch.object(acceptance, 'Run', FailedRun), \
                patch('run.source_fingerprint', return_value='frozen-source'), \
                patch('signal.signal'), \
                patch('sys.argv', ['k8s_userapp.py', '--ssh', 'test', '--url', 'http://test',
                                   '--proxy-url', 'http://test', '--scenario', 'recreate']), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(acceptance.main(), 1)
            summary = json.loads(next(Path(directory).glob('*/summary.json')).read_text())
            self.assertIn('original L2 workspace failed', summary['error'])
            self.assertIn('original API cleanup failed', summary['cleanup_error'])
            self.assertNotIn('traffic_wake_without_start', summary['missing'])
            self.assertIn('recreate_workspace_usable', summary['missing'])
            self.assertEqual(summary['verdict'], 'fail')


if __name__ == '__main__':
    unittest.main()
