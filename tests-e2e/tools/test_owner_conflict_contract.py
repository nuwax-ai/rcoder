"""Pure tool counterexamples. These are not real Docker/K8s acceptance."""
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest
import urllib.error
from types import SimpleNamespace
from unittest.mock import patch

import owner_conflict_contract as contract


def envelope(trace='a' * 32):
    return {'status': 409, 'trace_id': trace, 'body': {
        'code': contract.CODE, 'success': False, 'data': None, 'tid': trace,
        'message': '当前会话关联的运行账号与本次请求不一致，消息未发送。',
        'error_detail': {'reason_code': contract.REASON, 'stage': contract.STAGE,
                         'retryable': False, 'detail': '当前会话关联的运行账号与本次请求不一致，消息未发送。',
                         'hint': '请核对运行账号；不要自动重试。'}}}


def owner_row(trace, ids):
    return {'trace_id': trace, 'owner_conflict': True, 'chat_forward': False,
            'fields': {'dimension': 'user', 'reason_code': contract.REASON,
                       'dispatched': False, 'project_id': ids['project'],
                       'registered_user_id': 'Some(' + json.dumps(ids['user_a']) + ')',
                       'requested_user_id': 'Some(' + json.dumps(ids['user_b']) + ')'}}


class WireTests(unittest.TestCase):
    def test_valid_typed_error_and_exact_trace(self):
        contract.conflict(envelope(), 'unique-owner-a', 'unique-owner-b')

    def test_http200_business_error_is_not_passed_as_409(self):
        value = envelope(); value['status'] = 200
        with self.assertRaises(AssertionError):
            contract.conflict(value, 'unique-a', 'unique-b')

    def test_reason_stage_retry_task_and_ui_identity_regressions_fail(self):
        changes = [
            ('code', 'ERR_INTERNAL_SERVER_ERROR'), ('success', True), ('data', {'session_id': 'accepted'}),
            ('tid', None), ('tid', 'different-trace'), ('operation_id', 'accepted'),
            ('message', 'registered-owner-a'),
        ]
        for key, value in changes:
            wire = envelope(); wire['body'][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(AssertionError):
                contract.conflict(wire, 'registered-owner-a', 'requester-b')
        for key, value in [('reason_code', 'COMPUTER_OWNER_POD_MISMATCH'),
                           ('stage', 'after_dispatch'), ('retryable', True),
                           ('retryable', 0), ('hint', ''), ('detail', 'other')]:
            wire = envelope(); wire['body']['error_detail'][key] = value
            with self.subTest(key=key), self.assertRaises(AssertionError):
                contract.conflict(wire, 'registered-owner-a', 'requester-b')

    def test_owner_ids_in_any_ui_field_are_rejected(self):
        wire = envelope(); wire['body']['error_detail']['hint'] = 'registered-owner-a'
        with self.assertRaises(AssertionError):
            contract.conflict(wire, 'registered-owner-a', 'requester-b')

    def test_no_real_model_configuration_is_loaded(self):
        ids = contract.identities()
        body = contract.chat_payload(ids['user_b'], ids['project'])
        self.assertEqual(body['model_provider']['base_url'], 'http://127.0.0.1:1/v1')
        self.assertNotEqual(ids['project'], ids['cleanup_project_b'])
        self.assertNotEqual(ids['user_a'], ids['user_b'])
        self.assertLess(len(ids['user_a']), 30)

    def test_http_error_status_and_actual_envelope_are_preserved_locally(self):
        body = envelope()['body']
        response = urllib.error.HTTPError('http://localhost/computer/chat', 409, 'Conflict', {},
                                         io.BytesIO(json.dumps(body).encode()))
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(contract.urllib.request, 'urlopen', side_effect=response):
            api = contract.Http('http://localhost:8090', directory, 30)
            got = api.call('POST', '/computer/chat', {})
            self.assertEqual(got['status'], 409)
            self.assertEqual(got['body'], body)
            path = Path(api.calls[0]['response'])
            self.assertEqual(json.loads(path.read_text()), got)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)


class ProofTests(unittest.TestCase):
    def test_k8s_controller_selection_uses_only_current_main_and_namespace_instance(self):
        args = SimpleNamespace(namespace='nuwax-k8s-test', ssh='test-host',
                               context='test-context', server=None, controller_id=None)
        main = {'metadata': {'namespace': args.namespace, 'uid': 'current-main-uid',
                            'name': 'main-0', 'labels': {'app.kubernetes.io/component': 'rcoder-main',
                             'app.kubernetes.io/instance': args.namespace}}}
        with patch.object(contract, 'command', return_value=json.dumps({'items': [main]})) as run:
            observer = contract.Observer(args)
        self.assertEqual(observer.controllers, {'current-main-uid': 'main-0'})
        invocation = run.call_args.args[0][-1]
        self.assertIn('app.kubernetes.io/component=rcoder-main,app.kubernetes.io/instance=nuwax-k8s-test', invocation)
        self.assertNotIn('deployment', invocation)
        main['metadata']['labels']['app.kubernetes.io/instance'] = 'other-release'
        with patch.object(contract, 'command', return_value=json.dumps({'items': [main]})), \
                self.assertRaises(AssertionError):
            contract.Observer(args)

    def test_positive_owner_evidence_is_required_not_just_absent_forward(self):
        ids = contract.identities()
        with self.assertRaises(AssertionError):
            contract.log_proof([], ['trace'], ids)
        contract.log_proof([owner_row('trace', ids)], ['trace'], ids)

    def test_wrong_owner_or_later_chat_forward_fails(self):
        ids = contract.identities()
        row = owner_row('trace', ids)
        row['fields']['registered_user_id'] = 'Some("foreign")'
        with self.assertRaises(AssertionError):
            contract.log_proof([row], ['trace'], ids)
        row = owner_row('trace', ids)
        with self.assertRaises(AssertionError):
            contract.log_proof([row, {'trace_id': 'trace', 'owner_conflict': False,
                                     'chat_forward': True}], ['trace'], ids)


class FakeObserver:
    """Only an offline call-order fixture; no real container claims."""
    def __init__(self):
        self.resources = {}
        self.pvcs = {'preexisting': {'uid': 'pvc-original', 'pv': 'pv-original'}}
        self.retained_rows = []
        self.traces = []
        self.ids = None

    def capture(self, user):
        return copy.deepcopy(self.resources.get(user, []))

    def claims(self):
        return copy.deepcopy(self.pvcs)

    def existing(self):
        return {'daily-cid': {'image': 'daily-image', 'running': True}}

    def retained(self, rows):
        self.retained_rows.extend(rows)

    def logs(self, traces):
        return [owner_row(trace, self.ids) for trace in traces]


class FakeHttp:
    def __init__(self, observer, fail_chat=False):
        self.observer, self.fail_chat, self.calls, self.stops = observer, fail_chat, [], []

    def status(self, user, cleanup=False):
        rows = self.observer.capture(user)
        return {'status': 'running' if rows else 'not_found', 'container_id': rows[0]['uid'] if rows else None}

    def call(self, method, route, body=None, cleanup=False):
        trace = str(len(self.calls))
        self.calls.append({'method': method, 'route': route, 'trace_id': trace})
        if method == 'GET':
            from urllib.parse import parse_qs, urlsplit
            user = parse_qs(urlsplit(route).query)['user_id'][0]
            return {'status': 200, 'body': {'code': '0000', 'success': True, 'data': self.status(user)}}
        user = body['user_id']
        if route == '/computer/pod/stop':
            self.stops.append(body)
            self.observer.resources.pop(user, None)
            return {'status': 200, 'body': {'code': '0000', 'success': True, 'data': {'was_existing': True}}}
        self.observer.resources.setdefault(user, [{'kind': 'Container', 'name': 'test-' + user, 'uid': user + '-cid', 'mounts': []}])
        if user == self.observer.ids['user_b']:
            value = envelope(trace)
            if route == '/computer/chat' and self.fail_chat:
                value['status'] = 500
            return value
        return {'status': 200, 'body': {'code': '0000', 'success': True,
                'data': {'container_info': {'container_id': user + '-cid'}}}}


class CleanupTests(unittest.TestCase):
    def test_cleanup_uses_independent_b_project_and_preserves_a_until_its_stop(self):
        observer = FakeObserver(); ids = contract.identities(); observer.ids = ids
        api = FakeHttp(observer)
        for key in ['user_a', 'user_b']:
            observer.resources[ids[key]] = [{'kind': 'Container', 'name': key, 'uid': key, 'mounts': []}]
        captured = copy.deepcopy(observer.resources)
        result = contract.cleanup(api, observer, ids, captured, observer.claims())
        self.assertTrue(result['success'])
        self.assertEqual(api.stops, [contract.payload(ids['user_b'], ids['cleanup_project_b']),
                                     contract.payload(ids['user_a'], ids['project'])])
        self.assertFalse(result['volumes_removed'])

    def test_changed_physical_instance_is_not_stopped_by_logical_name(self):
        observer = FakeObserver(); ids = contract.identities(); observer.ids = ids
        observer.resources[ids['user_a']] = [{'kind': 'Container', 'name': 'same', 'uid': 'new', 'mounts': []}]
        api = FakeHttp(observer)
        result = contract.cleanup(api, observer, ids,
            {ids['user_a']: [{'kind': 'Container', 'name': 'same', 'uid': 'old', 'mounts': []}]}, observer.claims())
        self.assertFalse(result['success'])
        self.assertNotIn(ids['user_a'], [b['user_id'] for b in api.stops])

    def test_existing_pvc_drift_is_not_masked_by_new_snapshot_and_compute_is_cleaned(self):
        observer = FakeObserver(); ids = contract.identities(); observer.ids = ids
        previous = observer.claims()
        observer.pvcs['preexisting']['uid'] = 'replaced'
        api = FakeHttp(observer)
        result = contract.cleanup(api, observer, ids, {}, previous)
        self.assertFalse(result['success'])
        self.assertIn('claim_observation_error', result)
        self.assertIn('data_preservation_error', result)
        self.assertEqual(len(api.stops), 2)

    def test_actual_contract_control_flow_cleans_b_after_failed_chat_assertion(self):
        observer = FakeObserver(); ids = contract.identities(); observer.ids = ids
        api = FakeHttp(observer, fail_chat=True)
        with tempfile.TemporaryDirectory() as directory:
            report = contract.exercise(api, observer, ids, directory)
        self.assertFalse(report['success'])
        self.assertTrue(report['cleanup']['success'])
        self.assertEqual(len(api.stops), 2)
        self.assertNotIn('/computer/agent/stop', [c['route'] for c in api.calls])

    def test_success_requires_real_style_positive_log_proof_and_cleanup(self):
        observer = FakeObserver(); ids = contract.identities(); observer.ids = ids
        api = FakeHttp(observer)
        with tempfile.TemporaryDirectory() as directory:
            report = contract.exercise(api, observer, ids, directory)
            evidence = json.loads((Path(directory) / 'owner-trace-evidence.json').read_text())
        self.assertTrue(report['success'])
        self.assertEqual(len(evidence), 2)
        self.assertTrue(report['preexisting_resources_preserved'])
        self.assertFalse(report['all_rpc_zero_claimed'])


if __name__ == '__main__':
    unittest.main()
