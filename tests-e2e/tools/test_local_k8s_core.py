"""Strict harness protocol/safety behavior only; never contacts Kubernetes/HTTP."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location('local_core', Path(__file__).with_name('local_k8s_core.py'))
core = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(core)


class AdmissionTests(unittest.TestCase):
    def fixture(self, root):
        config = root / 'kubeconfig'
        config.write_text('opaque private credential fixture; must never be read')
        identity = {'root': str(root), 'run_id': 'b'*32, 'namespace': 'rcoder-owned-bbbbbbbb', 'context': 'orbstack',
                    'cluster_api': 'https://127.0.0.1:26443', 'kubeconfig': str(config), 'apps': ['corebbbbbbbb']}
        (root / 'identity.json').write_text(json.dumps(identity))
        return identity

    def test_wrong_targets_fail_before_any_http_or_kubectl(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            identity = self.fixture(root)
            with patch.object(core.subprocess, 'run', side_effect=AssertionError('no process permitted')):
                self.assertEqual(core.validate_inputs(root, 'http://127.0.0.1:18297', 'corebbbbbbbb')[1]['namespace'], identity['namespace'])
                for change in [{'namespace': 'rcoder-dev'}, {'namespace': 'default'}, {'context': 'remote'},
                               {'apps': []}, {'cluster_api': 'https://remote.invalid:443'}, {'root': str(root/'other')}]:
                    with self.subTest(change=change):
                        (root/'identity.json').write_text(json.dumps({**identity, **change}))
                        with self.assertRaises(core.ContractFailure):
                            core.validate_inputs(root, 'http://127.0.0.1:18297', 'corebbbbbbbb')
                (root/'identity.json').write_text(json.dumps(identity))
                for url in ['http://remote.invalid:18297', 'http://u:p@127.0.0.1:18297', 'http://127.0.0.1:18297/query']:
                    with self.assertRaises(core.ContractFailure):core.validate_inputs(root, url, 'corebbbbbbbb')
                with self.assertRaises(core.ContractFailure):core.validate_inputs(root, 'http://127.0.0.1:18297', 'c'*23)


class ContractTests(unittest.TestCase):
    def record(self):
        return {'app_id': 'coreapp', 'lifecycle_id': 'life-original', 'operation_id': 'op-original', 'scope': 'Dev',
                'action': 'stop', 'state': 'succeeded', 'stage': 'completed', 'revision': 4,
                'status_url': '/computer/pod/operations/coreapp/op-original'}

    def test_terminal_requires_original_identity_and_real_completed_stage(self):
        record = self.record()
        self.assertTrue(core.validate_operation(record, 'coreapp', 'life-original', 'op-original', 'stop'))
        for change in [{'app_id': 'other'}, {'lifecycle_id': 'new-life'}, {'operation_id': 'other-op'}, {'scope': 'Prod'},
                       {'action': 'restart'}, {'stage': 'stopping'}, {'state': 'recovery_required'}, {'state': 'failed'},
                       {'state': 'superseded'}, {'state': 'unknown'}]:
            with self.subTest(change=change), self.assertRaises(core.ContractFailure):
                core.validate_operation({**record, **change}, 'coreapp', 'life-original', 'op-original', 'stop')
        self.assertFalse(core.validate_operation({**record, 'state': 'running', 'stage': 'stopping'}, 'coreapp', 'life-original', 'op-original', 'stop'))

    def resource(self, pvc=False):
        return {'metadata': {'name': 'rcoder-app-builder-coreapp-workspace' if pvc else 'captured-resource', 'namespace': 'rcoder-owned', 'uid': 'actual-uid',
                'labels': {'app.kubernetes.io/managed-by': 'rcoder-runtime', 'rcoder.io/identifier': 'coreapp',
                           'service_type' if pvc else 'rcoder.io/service-type': 'user-app-builder'}}}

    def test_resource_scope_and_pvc_family_are_not_inferred_from_names(self):
        for pvc in [False, True]:
            valid = self.resource(pvc)
            core.validate_owned_resource(valid, 'rcoder-owned', 'coreapp', pvc=pvc)
            for key, value in [('namespace', 'rcoder-dev'), ('uid', None)]:
                changed = copy.deepcopy(valid);changed['metadata'][key] = value
                with self.assertRaises(core.ContractFailure):core.validate_owned_resource(changed, 'rcoder-owned', 'coreapp', pvc=pvc)
            for key, value in [('app.kubernetes.io/managed-by', 'other'), ('rcoder.io/identifier', 'other'),
                               ('service_type' if pvc else 'rcoder.io/service-type', 'user-app')]:
                changed = copy.deepcopy(valid);changed['metadata']['labels'][key] = value
                with self.assertRaises(core.ContractFailure):core.validate_owned_resource(changed, 'rcoder-owned', 'coreapp', pvc=pvc)

    def actual_pvc(self):
        pvc = self.resource(pvc=True)
        pvc['metadata']['labels'].pop('rcoder.io/identifier')
        pvc['metadata']['labels']['app'] = 'rcoder'
        pvc['spec'] = {'volumeName': 'captured-pv'}
        pvc['status'] = {'phase': 'Bound'}
        return pvc

    def test_actual_unlabelled_pvc_requires_the_declared_name_and_runtime_family(self):
        pvc = self.actual_pvc()
        core.validate_owned_resource(pvc, 'rcoder-owned', 'coreapp', pvc=True)
        for change in [{'name': 'rcoder-app-builder-other-workspace'}, {'name': 'arbitrary-volume'},
                       {'namespace': 'rcoder-dev'}, {'uid': None}]:
            changed = copy.deepcopy(pvc);changed['metadata'].update(change)
            with self.subTest(change=change), self.assertRaises(core.ContractFailure):
                core.validate_owned_resource(changed, 'rcoder-owned', 'coreapp', pvc=True)
        for label, value in [('app.kubernetes.io/managed-by', 'other'), ('service_type', 'user-app'), ('rcoder.io/identifier', 'other')]:
            changed = copy.deepcopy(pvc);changed['metadata']['labels'][label] = value
            with self.subTest(label=label), self.assertRaises(core.ContractFailure):
                core.validate_owned_resource(changed, 'rcoder-owned', 'coreapp', pvc=True)

    def test_fresh_pvc_lookup_reads_the_exact_name_even_without_identifier_label(self):
        harness = core.Harness.__new__(core.Harness);harness.app = 'coreapp';harness.namespace = 'rcoder-owned'
        harness.ownership = Mock(return_value={'uid': 'captured-namespace-uid'})
        pvc = self.actual_pvc()
        expected = ['get', 'pvc', pvc['metadata']['name'], '--ignore-not-found=true', '-o', 'json']
        def actual_kube(args, json_output=True):
            self.assertEqual(args, expected)
            self.assertFalse(json_output)
            return json.dumps(pvc)
        harness.kube = Mock(side_effect=actual_kube)
        self.assertEqual(harness.selected('pvc'), [pvc])
        harness.ownership.assert_called_once()
        harness.kube = Mock(return_value='')
        self.assertEqual(harness.selected('pvc'), [])

    def physical_fixture(self, running=True):
        pvc = self.actual_pvc()
        sts = self.resource();sts['metadata'].update(name='captured-sts', uid='captured-sts-uid')
        volume = {'name': 'workspace', 'persistentVolumeClaim': {'claimName': pvc['metadata']['name']}}
        sts['spec'] = {'replicas': 1 if running else 0, 'template': {'spec': {'volumes': [copy.deepcopy(volume)]}}}
        pod = self.resource();pod['metadata'].update(name='captured-pod', uid='captured-pod-uid',
            ownerReferences=[{'kind': 'StatefulSet', 'uid': sts['metadata']['uid']}])
        pod['spec'] = {'volumes': [copy.deepcopy(volume)]}
        pod['status'] = {'phase': 'Running', 'containerStatuses': [{'name': 'agent', 'ready': True}]}
        rows = {'sts': [sts], 'pods': [pod] if running else [], 'pvc': [pvc]}
        harness = core.Harness.__new__(core.Harness);harness.app = 'coreapp';harness.namespace = 'rcoder-owned';harness.original = None
        harness.ownership = Mock(return_value={'uid': 'captured-namespace-uid'})
        harness.selected = Mock(side_effect=lambda kind: rows[kind])
        return harness, rows

    def test_running_and_stopped_compute_require_real_claim_and_captured_volume_identity(self):
        harness, rows = self.physical_fixture()
        original = harness.physical(True)
        self.assertEqual(original['pvc']['uid'], rows['pvc'][0]['metadata']['uid'])
        self.assertEqual(original['pvc']['volume_name'], 'captured-pv')
        harness.original = original
        for target in ['sts', 'pods']:
            resource = rows[target][0]
            volumes = resource['spec']['template']['spec']['volumes'] if target == 'sts' else resource['spec']['volumes']
            volumes[0]['persistentVolumeClaim']['claimName'] = 'rcoder-app-builder-other-workspace'
            with self.subTest(target=target), self.assertRaises(core.ContractFailure):harness.physical(True)
            volumes[0]['persistentVolumeClaim']['claimName'] = original['pvc']['name']
        rows['pods'] = [];rows['sts'][0]['spec']['replicas'] = 0
        self.assertEqual(harness.physical(False)['pvc']['uid'], original['pvc']['uid'])
        for target, field, value in [('metadata', 'uid', 'replacement-uid'), ('spec', 'volumeName', 'replacement-pv')]:
            old = rows['pvc'][0][target][field];rows['pvc'][0][target][field] = value
            with self.subTest(field=field), self.assertRaises(core.ContractFailure):harness.physical(False)
            rows['pvc'][0][target][field] = old
        rows['sts'][0]['spec']['template']['spec']['volumes'][0]['persistentVolumeClaim']['claimName'] = 'foreign-claim'
        with self.assertRaises(core.ContractFailure):harness.physical(False)
        self.assertGreater(harness.ownership.call_count, 0)

    def test_control_uses_documented_body_and_cannot_fake_synchronous_success(self):
        harness = core.Harness.__new__(core.Harness);harness.app = 'coreapp';harness.lifecycle_id = 'life-original'
        record = self.record();harness.http = Mock(return_value=(202, {'data': record, 'operation_id': record['operation_id']}))
        result = harness.control('stop', 'stable-request')
        self.assertEqual(result['id'], 'op-original')
        self.assertEqual(harness.http.call_args.args, ('POST', '/computer/pod/stop',
            {'app_id': 'coreapp', 'app_stage': 'dev', 'service_type': 'userapp', 'lifecycle_id': 'life-original', 'request_id': 'stable-request'}))
        self.assertTrue(harness.http.call_args.kwargs['mutation'])
        for status, body in [(200, {'data': record, 'operation_id': 'op-original'}),
                             (202, {'data': {**record, 'status_url': '/some/other'}, 'operation_id': 'op-original'}),
                             (202, {'data': record, 'operation_id': 'fake-parent'})]:
            harness.http.return_value = (status, body)
            with self.assertRaises(core.ContractFailure):harness.control('stop', 'stable-request')

    def test_failure_keeps_volumes_and_only_reads_original_operation(self):
        harness = core.Harness.__new__(core.Harness);harness.report = {};harness.last_operation = {'id': 'original', 'status_url': '/computer/pod/operations/coreapp/original'}
        harness.pending_compute_request = None
        harness.deadline = time.monotonic()+10;harness.persist = Mock();harness.selected = Mock(return_value=[])
        harness.http = Mock(return_value=(200, {'data': {'operation_id': 'original', 'state': 'recovery_required'}}))
        harness.control = Mock(side_effect=AssertionError('failure must never issue Stop/Restart'))
        harness.failure_evidence(RuntimeError('original write remains unknown'))
        harness.control.assert_not_called()
        self.assertEqual(harness.http.call_args.args, ('GET', '/computer/pod/operations/coreapp/original'))
        self.assertEqual(harness.report['failure_original_operation']['state'], 'recovery_required')

    def test_dispatched_restart_disconnect_keeps_new_request_separate_from_previous_stop(self):
        harness = core.Harness.__new__(core.Harness);harness.app = 'coreapp';harness.lifecycle_id = 'life-original'
        old = {'id': 'previous-stop', 'action': 'stop', 'request_id': 'old-request', 'status_url': '/computer/pod/operations/coreapp/previous-stop'}
        harness.last_operation = old.copy()
        def disconnected(method, route, payload, mutation=False):
            harness.pending_compute_request.update(dispatched=True, accepted_result_unknown=True)
            raise ConnectionError('response lost after original request dispatch')
        harness.http = Mock(side_effect=disconnected)
        with self.assertRaises(ConnectionError):harness.control('restart', 'stable-new-restart')
        self.assertEqual(harness.last_operation, old)
        self.assertEqual(harness.pending_compute_request['request_id'], 'stable-new-restart')
        self.assertEqual(harness.pending_compute_request['lifecycle_id'], 'life-original')
        self.assertTrue(harness.pending_compute_request['accepted_result_unknown'])
        self.assertNotIn('operation_id', harness.pending_compute_request)
        self.assertEqual(harness.http.call_count, 1)

    def test_missing_or_wrong_source_build_identity_cannot_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            harness = core.Harness.__new__(core.Harness);harness.root = Path(temporary)
            path = harness.root/'host-build-after-identity.json'
            for source in [None, 'not-a-sha', 'a'*64]:
                path.write_text(json.dumps({'source_inputs_sha256': source}))
                with patch.object(core, 'source_snapshot', return_value={'source_inputs_sha256': 'b'*64}), self.assertRaises(core.ContractFailure):
                    harness.input_proof()

    def test_diagnostic_privacy_preserves_original_identity(self):
        data = {'operation_id': 'actual-operation', 'password': 'private-password', 'detail':
                'postgres://user:private-uri@127.0.0.1/db password=private-inline Authorization: Bearer private-bearer'}
        output = core.safe(data);text = json.dumps(output)
        for secret in ['private-password', 'private-uri', 'private-inline', 'private-bearer']:
            self.assertNotIn(secret, text)
        self.assertEqual(output['operation_id'], 'actual-operation')

    def test_namespace_replacement_is_detected_before_a_mutating_request(self):
        harness = core.Harness.__new__(core.Harness);harness.namespace = 'rcoder-owned';harness.namespace_uid = 'original-namespace-uid';harness.identity = {'run_id': 'b'*32}
        harness.kube = Mock(return_value={'metadata': {'name': 'rcoder-owned', 'uid': 'new-namespace-uid', 'labels': {'rcoder.e2e.owner': 'b'*32}}})
        with self.assertRaises(core.ContractFailure):harness.ownership()


if __name__ == '__main__':
    unittest.main()
