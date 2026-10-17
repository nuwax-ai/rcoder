"""Pure tool counterexamples. These are not real Docker/K8s acceptance."""
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest
import urllib.error
from types import SimpleNamespace
from unittest.mock import Mock, patch

import owner_conflict_contract as contract


def envelope(trace='a' * 32, accept_language=None):
    message, hint = (
        ('当前会话关联的运行账号与本次请求不一致，消息未发送。',
         '请联系管理员核对会话关联的运行账号与本次请求账号，修正后再发送；不要自动重试。')
        if accept_language == 'zh-CN' else
        ('The runtime account linked to this conversation does not match this request. No message was sent.',
         'Ask an administrator to check the conversation runtime account against the request account before sending again. Do not retry automatically.'))
    return {'status': 409, 'trace_id': trace, 'request_accept_language': accept_language, 'body': {
        'code': contract.CODE, 'success': False, 'data': None, 'tid': trace,
        'message': message,
        'error_detail': {'reason_code': contract.REASON, 'stage': contract.STAGE,
                         'retryable': False, 'detail': message, 'hint': hint}}}


def owner_row(trace, ids):
    return {'trace_id': trace, 'owner_conflict': True, 'chat_forward': False,
            'fields': {'dimension': 'user', 'reason_code': contract.REASON,
                       'dispatched': False, 'project_id': ids['project'],
                       'registered_user_id': 'Some(' + json.dumps(ids['user_a']) + ')',
                       'requested_user_id': 'Some(' + json.dumps(ids['user_b']) + ')'}}


class WireTests(unittest.TestCase):
    def test_valid_typed_error_and_exact_trace(self):
        contract.conflict(envelope(), 'unique-owner-a', 'unique-owner-b')

    def test_both_approved_languages_require_their_message_and_hint(self):
        for language in (None, 'zh-CN'):
            with self.subTest(language=language):
                contract.conflict(envelope(accept_language=language), 'unique-a', 'unique-b', language)
                other = envelope(accept_language='zh-CN' if language is None else None)
                for field in ('message', 'hint'):
                    wrong = envelope(accept_language=language)
                    if field == 'message':
                        wrong['body']['message'] = other['body']['message']
                        wrong['body']['error_detail']['detail'] = other['body']['message']
                    else:
                        wrong['body']['error_detail']['hint'] = other['body']['error_detail']['hint']
                    with self.subTest(field=field), self.assertRaises(AssertionError):
                        contract.conflict(wrong, 'unique-a', 'unique-b', language)

    def test_absent_language_evidence_must_not_be_explicit_english_or_empty(self):
        for language in ('en-US', '', 'zh-CN'):
            wire = envelope(); wire['request_accept_language'] = language
            with self.subTest(language=language), self.assertRaises(AssertionError):
                contract.conflict(wire, 'unique-a', 'unique-b')
        wire = envelope(); wire.pop('request_accept_language')
        with self.assertRaises(AssertionError):
            contract.conflict(wire, 'unique-a', 'unique-b')

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

    def test_actual_request_omits_default_header_and_records_explicit_chinese(self):
        observed = []
        def response(request, **kwargs):
            observed.append(request)
            trace = request.get_header('Traceparent').split('-')[1]
            body = envelope(trace, request.get_header('Accept-language'))['body']
            raise urllib.error.HTTPError(request.full_url, 409, 'Conflict', {},
                                         io.BytesIO(json.dumps(body).encode()))
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(contract.urllib.request, 'urlopen', side_effect=response):
            api = contract.Http('http://localhost:8090', directory, 30)
            for language in (None, 'zh-CN'):
                for route in ('/computer/chat', '/computer/pod/ensure'):
                    got = api.call('POST', route, {}, accept_language=language)
                    contract.conflict(got, 'unique-a', 'unique-b', language)
            self.assertEqual([r['request_accept_language'] for r in api.calls], [None, None, 'zh-CN', 'zh-CN'])
            self.assertEqual(len({r['trace_id'] for r in api.calls}), 4)
            for index in (0, 1):
                self.assertNotIn('accept-language', {key.lower() for key, _ in observed[index].header_items()})
            for index in (2, 3):
                self.assertEqual(observed[index].get_header('Accept-language'), 'zh-CN')
            self.assertEqual(len(list((Path(directory) / 'responses').glob('*.json'))), 4)


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

    def test_four_language_requests_each_need_positive_owner_and_no_forward(self):
        ids = contract.identities()
        traces = ['default-chat', 'default-ensure', 'chinese-chat', 'chinese-ensure']
        rows = [owner_row(trace, ids) for trace in traces]
        contract.log_proof(rows, traces, ids)
        with self.assertRaises(AssertionError):
            contract.log_proof(rows[:-1], traces, ids)
        with self.assertRaises(AssertionError):
            contract.log_proof(rows + [{'trace_id': traces[-1], 'owner_conflict': False,
                                       'chat_forward': True}], traces, ids)


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
        self.traces = list(traces)
        return [owner_row(trace, self.ids) for trace in traces]


class FakeHttp:
    def __init__(self, observer, fail_chat=False):
        self.observer, self.fail_chat, self.calls, self.stops = observer, fail_chat, [], []

    def status(self, user, cleanup=False):
        rows = self.observer.capture(user)
        return {'status': 'running' if rows else 'not_found', 'container_id': rows[0]['uid'] if rows else None}

    def call(self, method, route, body=None, cleanup=False, accept_language=None):
        trace = str(len(self.calls))
        self.calls.append({'method': method, 'route': route, 'trace_id': trace,
                           'request_accept_language': accept_language})
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
            value = envelope(trace, accept_language)
            if route == '/computer/chat' and self.fail_chat:
                value['status'] = 500
            return value
        return {'status': 200, 'body': {'code': '0000', 'success': True,
                'data': {'container_info': {'container_id': user + '-cid'}}}}


class CleanupTests(unittest.TestCase):
    def strict_fixture(self, root):
        observer = FakeObserver()
        ids = contract.identities()
        observer.ids = ids
        controller = 'a' * 64
        observer.controllers = {controller: '/original-controller'}
        baseline = {controller: {'image': 'image', 'running': True}}
        observer.existing = Mock(return_value=baseline)
        receipt = {'version': 1, 'root': str(Path(root).resolve()), 'run_id': 'b' * 32,
                   'case_id': 'c' * 32, 'ids': ids, 'url': 'http://127.0.0.1:8090',
                   'controllers': observer.controllers, 'observer': {'namespace': None},
                   'initial_absence': {ids['user_a']: True, ids['user_b']: True},
                   'baseline': baseline, 'claims': observer.claims(), 'captured': {},
                   'touched': True, 'inflight': None}
        return observer, FakeHttp(observer), receipt

    def test_strict_receipt_is_persisted_before_creation_and_preserves_no_secret(self):
        observer = FakeObserver(); ids = contract.identities(); observer.ids = ids
        api = FakeHttp(observer)
        with tempfile.TemporaryDirectory() as directory:
            receipt = {'version': 1, 'root': directory, 'run_id': 'b' * 32, 'case_id': 'c' * 32}
            original = api.call
            def observe(method, route, body=None, **kwargs):
                if method == 'POST' and route != '/computer/pod/stop':
                    saved = json.loads((Path(directory) / 'ownership.json').read_text())
                    self.assertTrue(saved['touched'])
                    self.assertEqual(saved['initial_absence'], {ids['user_a']: True, ids['user_b']: True})
                    self.assertEqual(saved['inflight'], {'route': route, 'user_id': body['user_id'],
                                                        'project_id': body['project_id']})
                    self.assertNotIn('api_key', json.dumps(saved))
                return original(method, route, body, **kwargs)
            api.call = observe
            result = contract.exercise(api, observer, ids, directory, receipt)
            saved = json.loads((Path(directory) / 'ownership.json').read_text())
            self.assertEqual((Path(directory) / 'ownership.json').stat().st_mode & 0o777, 0o600)
            self.assertIsNone(saved['inflight'])
            self.assertTrue(result['success'])
            self.assertTrue(saved['cleanup']['success'])

    def test_interrupted_receipt_write_keeps_previous_complete_proof(self):
        with tempfile.TemporaryDirectory() as directory:
            contract.save_ownership(directory, {'touched': False})
            with patch.object(contract.json, 'dump', side_effect=OSError('interrupted write')), self.assertRaises(OSError):
                contract.save_ownership(directory, {'touched': True})
            self.assertEqual(json.loads((Path(directory) / 'ownership.json').read_text()), {'touched': False})
            self.assertEqual(list(Path(directory).glob('.ownership-*')), [])

    def test_fallback_uses_original_controller_and_only_two_proven_random_accounts(self):
        with tempfile.TemporaryDirectory() as directory:
            observer, api, receipt = self.strict_fixture(directory)
            for user in (receipt['ids']['user_a'], receipt['ids']['user_b']):
                observer.resources[user] = [{'kind': 'Container', 'name': user, 'uid': user + '-cid', 'mounts': []}]
            # This simulates an ensure response lost before capture was persisted.
            contract.save_ownership(directory, receipt)
            with patch.object(contract, 'Observer', return_value=observer), \
                    patch.object(contract, 'Http', return_value=api):
                result = contract.cleanup_fixture(directory, 'b' * 32, 'c' * 32)
            self.assertTrue(result['success'])
            self.assertEqual({row['user_id'] for row in api.stops},
                             {receipt['ids']['user_a'], receipt['ids']['user_b']})
            self.assertFalse(result['volumes_removed'])
            self.assertTrue(result['preexisting_resources_preserved'])

    def test_fallback_rejects_foreign_run_initial_absence_controller_and_baseline_uid(self):
        for mutation in ('run', 'ids', 'absence', 'controller', 'baseline_uid'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                observer, api, receipt = self.strict_fixture(directory)
                if mutation == 'run':
                    receipt['run_id'] = 'd' * 32
                elif mutation == 'ids':
                    receipt['ids']['user_a'] = 'existing-user'
                elif mutation == 'absence':
                    receipt['initial_absence'].pop(receipt['ids']['user_a'])
                elif mutation == 'controller':
                    observer.controllers = {'d' * 64: '/replacement-controller'}
                else:
                    observer.resources[receipt['ids']['user_a']] = [{'kind': 'Container', 'name': 'owned-name',
                        'uid': 'a' * 64, 'mounts': []}]
                contract.save_ownership(directory, receipt)
                with patch.object(contract, 'Observer', return_value=observer), \
                        patch.object(contract, 'Http', return_value=api), self.assertRaises(ValueError):
                    contract.cleanup_fixture(directory, 'b' * 32, 'c' * 32)
                self.assertEqual(api.stops, [])

    def test_empty_inventory_does_not_settle_interrupted_post(self):
        with tempfile.TemporaryDirectory() as directory:
            observer, api, receipt = self.strict_fixture(directory)
            receipt['inflight'] = {'route': '/computer/pod/ensure', 'user_id': receipt['ids']['user_a'],
                                    'project_id': receipt['ids']['project']}
            contract.save_ownership(directory, receipt)
            with patch.object(contract, 'Observer', return_value=observer), \
                    patch.object(contract, 'Http', return_value=api):
                result = contract.cleanup_fixture(directory, 'b' * 32, 'c' * 32)
            self.assertFalse(result['success'])
            self.assertEqual(result['unsettled_request'], receipt['inflight'])

    def test_strict_cleanup_routes_owned_directory_without_generic_docker_removal(self):
        import cleanup as outer_cleanup
        with tempfile.TemporaryDirectory() as directory:
            case = Path(directory)
            root = case / 'owner-conflict-contract'
            root.mkdir()
            (root / 'ownership.json').write_text(json.dumps({'captured': {
                'random-user': [{'name': '/owned-agent', 'uid': 'captured'}]}}))
            remaining = {'Id': 'captured', 'Name': '/owned-agent',
                         'Config': {'Labels': {'rcoder.e2e.run': 'run'}}}
            with patch.object(contract, 'cleanup_fixture', return_value={'success': False}) as reclaim, \
                    patch.object(outer_cleanup, 'command', side_effect=['captured', json.dumps([remaining])]) as docker:
                errors = outer_cleanup.cleanup_case('case', 'run', case)
            reclaim.assert_called_once_with(root, 'run', 'case', ())
            self.assertTrue(any('Owner conflict fixture cleanup incomplete' in error for error in errors))
            self.assertEqual(docker.call_count, 2, 'no generic bare Docker removal after creation-aware failure')

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
        self.assertEqual(len(evidence), 4)
        self.assertEqual(len(set(observer.traces)), 4)
        self.assertEqual(set(report['checks']), contract.STEPS)
        self.assertEqual(len(report['checks']), len(contract.STEPS))
        self.assertEqual(report['required_steps'], sorted(contract.STEPS))
        self.assertEqual(report['language_contract'], {'default_no_accept_language': 'en-US',
                         'explicit_accept_language': 'zh-CN', 'required_conflict_responses': 4})
        conflicts = [r for r in api.calls if r['trace_id'] in observer.traces]
        self.assertEqual([(r['route'], r['request_accept_language']) for r in conflicts],
                         [('/computer/chat', None), ('/computer/pod/ensure', None),
                          ('/computer/chat', 'zh-CN'), ('/computer/pod/ensure', 'zh-CN')])
        self.assertTrue(report['preexisting_resources_preserved'])
        self.assertFalse(report['all_rpc_zero_claimed'])


if __name__ == '__main__':
    unittest.main()
