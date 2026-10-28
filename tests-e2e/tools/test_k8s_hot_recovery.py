"""Pure ownership/report contracts and local artifact server timing; no cluster I/O."""
import copy
import http.server
import json
from pathlib import Path
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
import urllib.error
import urllib.request
from unittest.mock import Mock, patch

import k8s_hot_fixture as fixture_server
import k8s_hot_recovery as recovery
import k8s_userapp as acceptance


def run_fixture():
    return SimpleNamespace(id='0123456789abcdef0123456789abcdef', app='e2e-k8s-0123456789abcd',
                           lifecycle_id='original-lifecycle', baseline=[],
                           args=SimpleNamespace(namespace='nuwax-k8s-test', ssh='test-host'), save=Mock())


def fixture_resource(run, kind='Pod', uid='owned-fixture'):
    return {'apiVersion': 'v1', 'kind': kind, 'metadata': {
        'name': 'rcoder-e2e-hot-' + run.id[:20], 'namespace': run.args.namespace,
        'uid': uid, 'resourceVersion': 'original-version',
        'labels': {recovery.FIXTURE_LABEL: run.id, recovery.FIXTURE_CASE_LABEL: recovery.FIXTURE_CASE,
                   'app.kubernetes.io/managed-by': 'rcoder-e2e'},
        'annotations': {'rcoder.io/application-id': run.app, 'rcoder.io/lifecycle-id': run.lifecycle_id}}}


def lease_fixture():
    return {'apiVersion': 'coordination.k8s.io/v1', 'kind': 'Lease', 'metadata': {
        'name': 'rcoder-operation-prod-app', 'uid': 'original-lease-uid', 'resourceVersion': 'original-rv',
        'creationTimestamp': '2026-01-01T00:00:00Z',
        'labels': {'rcoder.io/operation-app': 'app', 'rcoder.io/operation-family': 'user-app'},
        'annotations': {'rcoder.io/lifecycle-id': 'life', 'rcoder.io/operation-id': 'op',
                        'rcoder.io/executor-id': 'executor'}},
        'spec': {'holderIdentity': 'executor:op', 'leaseDurationSeconds': 60,
                 'renewTime': '2026-01-01T00:00:00.000000Z'}}


def association_fixture():
    return {'request_id': 'original-request', 'app_id': 'app', 'lifecycle_id': 'life',
            'lease_absent_before_request': True, 'prod_slot_empty_before_request': True,
            'request_started_at': '2026-01-01T00:00:00.500000+00:00',
            'operation_id': 'op', 'owner_operation_id': 'op', 'pod_uid': 'actual-original-pod'}


def legacy_lease_fixture():
    row = lease_fixture()
    row['metadata']['annotations'] = {'rcoder.io/legacy-operation-id': 'original-random-token'}
    row['spec']['holderIdentity'] = 'legacy:original-random-token'
    return row


class HotIdentityTests(unittest.TestCase):
    def test_lease_fault_requires_original_app_lifecycle_operation_executor_and_ttl(self):
        expected = recovery.original_lease(lease_fixture(), 'app', 'life', 'op', association_fixture())
        self.assertEqual(expected['uid'], 'original-lease-uid')
        changes = [(['metadata', 'name'], 'another-lease'),
                   (['metadata', 'uid'], ''), (['metadata', 'resourceVersion'], ''),
                   (['metadata', 'labels', 'rcoder.io/operation-app'], 'foreign'),
                   (['metadata', 'labels', 'rcoder.io/operation-family'], 'computer-agent-runner'),
                   (['metadata', 'annotations', 'rcoder.io/lifecycle-id'], 'new-life'),
                   (['metadata', 'annotations', 'rcoder.io/operation-id'], 'new-op'),
                   (['metadata', 'annotations', 'rcoder.io/executor-id'], 'new-executor'),
                   (['spec', 'holderIdentity'], 'new-executor:op'),
                   (['spec', 'leaseDurationSeconds'], 1), (['spec', 'renewTime'], None)]
        for keys, value in changes:
            row = lease_fixture()
            parent = row
            for key in keys[:-1]:
                parent = parent[key]
            parent[keys[-1]] = value
            with self.subTest(keys=keys), self.assertRaises(AssertionError):
                recovery.original_lease(row, 'app', 'life', 'op', association_fixture())

    def test_actual_legacy_lease_shape_is_associated_without_invented_execution_fields(self):
        row = legacy_lease_fixture()
        identity = recovery.original_lease(row, 'app', 'life', 'op', association_fixture())
        self.assertEqual(identity['shape'], 'fresh-legacy-before-admission')
        self.assertEqual(identity['holder_token'], 'original-random-token')
        self.assertIsNone(identity['execution_annotations']['rcoder.io/executor-id'])
        renewed = copy.deepcopy(row)
        renewed['metadata']['resourceVersion'] = 'renewed-rv'
        renewed['spec']['renewTime'] = '2026-01-01T00:00:20.000000Z'
        self.assertEqual(recovery.original_lease(renewed, 'app', 'life', 'op', association_fixture(), identity)['uid'], identity['uid'])
        self.assertEqual(row['metadata']['annotations'], {'rcoder.io/legacy-operation-id': 'original-random-token'})

    def test_legacy_token_contradictions_and_known_execution_fields_never_pass(self):
        for key, value in [('rcoder.io/operation-id', 'op'),
                           ('rcoder.io/lease-token', 'different-token'),
                           ('rcoder.io/lease-token', ''),
                           ('rcoder.io/application-id', 'foreign'),
                           ('rcoder.io/lifecycle-id', 'foreign-life'),
                           ('rcoder.io/operation-id', 'foreign-operation'),
                           ('rcoder.io/compute-lease', 'true')]:
            row = legacy_lease_fixture()
            row['metadata']['annotations'][key] = value
            with self.subTest(key=key), self.assertRaises(AssertionError):
                recovery.original_lease(row, 'app', 'life', 'op', association_fixture())
        row = legacy_lease_fixture()
        row['spec']['holderIdentity'] = 'legacy:another-holder'
        with self.assertRaises(AssertionError):
            recovery.original_lease(row, 'app', 'life', 'op', association_fixture())

    def test_no_before_absence_or_original_owner_association_grants_no_fault_authority(self):
        for key, value in [('lease_absent_before_request', False), ('prod_slot_empty_before_request', False),
                           ('app_id', 'foreign'), ('lifecycle_id', 'new-life'),
                           ('operation_id', 'other'), ('owner_operation_id', 'other'),
                           ('pod_uid', ''), ('request_id', ''), ('request_started_at', '')]:
            association = {**association_fixture(), key: value}
            with self.subTest(key=key), self.assertRaises(AssertionError):
                recovery.original_lease(legacy_lease_fixture(), 'app', 'life', 'op', association)

    def test_legacy_capture_never_follows_a_new_uid_token_or_changed_optional_execution(self):
        row = legacy_lease_fixture()
        row['metadata']['annotations']['rcoder.io/executor-id'] = 'captured-only-not-invented'
        captured = recovery.original_lease(row, 'app', 'life', 'op', association_fixture())
        for keys, value in [(['metadata', 'uid'], 'replacement'),
                            (['metadata', 'annotations', 'rcoder.io/legacy-operation-id'], 'other'),
                            (['metadata', 'annotations', 'rcoder.io/executor-id'], 'other'),
                            (['metadata', 'creationTimestamp'], '2026-01-01T01:00:00Z')]:
            changed = copy.deepcopy(row)
            parent = changed
            for key in keys[:-1]:
                parent = parent[key]
            parent[keys[-1]] = value
            with self.subTest(keys=keys), self.assertRaises(AssertionError):
                recovery.original_lease(changed, 'app', 'life', 'op', association_fixture(), captured)

    def test_fault_patch_anchors_exact_uid_and_opaque_resource_version(self):
        lease = lease_fixture()
        patch = recovery.exact_patch(lease, [{'op': 'replace', 'path': '/spec/renewTime', 'value': 'past'}])
        self.assertEqual(patch[:2], [
            {'op': 'test', 'path': '/metadata/uid', 'value': 'original-lease-uid'},
            {'op': 'test', 'path': '/metadata/resourceVersion', 'value': 'original-rv'}])
        self.assertEqual(patch[2:], [{'op': 'replace', 'path': '/spec/renewTime', 'value': 'past'}])
        self.assertEqual(lease['spec']['leaseDurationSeconds'], 60)
        self.assertNotIn('leaseDurationSeconds', json.dumps(patch))

    def test_fixture_must_be_exact_run_case_namespace_app_lifecycle_and_new_uid(self):
        run = run_fixture()
        row = fixture_resource(run)
        self.assertEqual(recovery.fixture_owned(row, run, set(), 'owned-fixture')['uid'], 'owned-fixture')
        for path, value in [(['metadata', 'namespace'], 'nuwax-k8s-prod'),
                            (['metadata', 'name'], 'rcoder-e2e-hot-similar'),
                            (['metadata', 'labels', recovery.FIXTURE_LABEL], 'another-run'),
                            (['metadata', 'labels', recovery.FIXTURE_CASE_LABEL], 'another-case'),
                            (['metadata', 'annotations', 'rcoder.io/application-id'], 'foreign'),
                            (['metadata', 'annotations', 'rcoder.io/lifecycle-id'], 'foreign'),
                            (['kind'], 'PersistentVolumeClaim')]:
            changed = copy.deepcopy(row)
            parent = changed
            for key in path[:-1]:
                parent = parent[key]
            parent[path[-1]] = value
            with self.subTest(path=path), self.assertRaises(AssertionError):
                recovery.fixture_owned(changed, run, set())
        with self.assertRaises(AssertionError):
            recovery.fixture_owned(row, run, {'owned-fixture'})
        with self.assertRaises(AssertionError):
            recovery.fixture_owned(row, run, set(), 'different-uid')

    def test_fixture_documents_reuse_runtime_image_and_never_mount_pvc_or_docker_socket(self):
        run = run_fixture()
        owned = recovery.ArtifactFixture(run, 'example/runtime@sha256:checked', {'B': b'actual B zip', 'D': b'actual D zip'})
        cm, service, pod = owned.documents()
        self.assertEqual([row['kind'] for row in (cm, service, pod)], ['ConfigMap', 'Service', 'Pod'])
        self.assertEqual(set(cm['binaryData']), {'B.zip', 'D.zip'})
        self.assertEqual(pod['spec']['containers'][0]['image'], 'example/runtime@sha256:checked')
        self.assertEqual(pod['spec']['containers'][0]['command'], ['python3', '-u', '/fixture/server.py'])
        self.assertFalse(pod['spec']['automountServiceAccountToken'])
        self.assertTrue(all('persistentVolumeClaim' not in row and 'hostPath' not in row
                            for row in pod['spec']['volumes']))
        self.assertNotIn('docker.sock', json.dumps(pod))
        self.assertNotIn('env', pod['spec']['containers'][0])
        self.assertEqual(service['spec']['selector'],
                         {recovery.FIXTURE_LABEL: run.id, recovery.FIXTURE_CASE_LABEL: recovery.FIXTURE_CASE})

    def test_production_namespace_and_unknown_control_are_rejected_before_io(self):
        run = run_fixture()
        run.args.namespace = 'nuwax-k8s-prod'
        with self.assertRaisesRegex(ValueError, 'restricted'):
            recovery.ArtifactFixture(run, 'image', {})
        run.args.namespace = 'nuwax-k8s-test'
        fixture = recovery.ArtifactFixture(run, 'image', {})
        fixture.current = Mock()
        for action, version in [('delete', None), ('release', 'A'), ('release', '../B')]:
            with self.assertRaises(ValueError):
                fixture.control(action, version)
        fixture.current.assert_not_called()

    def test_environment_completion_requires_entire_current_release_identity(self):
        artifact = {'url': 'http://artifact/B.zip', 'sha256': 'actual-sha'}
        data = {'APP_DEPLOY_URL': artifact['url'], 'APP_DEPLOY_SHA256': artifact['sha256'],
                'APP_RELEASE_ID': 'original-request-release', 'APP_DEPLOY_OPERATION_ID': 'original-op'}
        self.assertTrue(recovery.env_matches({'data': data}, artifact, 'original-request-release', 'original-op'))
        for key in data:
            changed = {**data, key: 'old-or-foreign'}
            with self.subTest(key=key):
                self.assertFalse(recovery.env_matches({'data': changed}, artifact, 'original-request-release', 'original-op'))

    def test_runtime_fixture_uses_actual_pullable_leaf_not_preloaded_config_id(self):
        leaf = 'example/runtime@sha256:' + 'ab' * 32
        pod = {'images': [{'image': 'sha256:' + 'cd' * 32, 'imageID': leaf}]}
        self.assertEqual(recovery.runtime_fixture_image(pod), leaf)
        self.assertEqual(recovery.runtime_fixture_image({'images': [{'image': 'local-config', 'imageID': 'docker-pullable://' + leaf}]}), leaf)
        for invalid in [{'images': []}, {'images': [{'image': 'sha256:' + 'cd' * 32}]},
                        {'images': [{'image': leaf}, {'image': leaf}]}]:
            with self.subTest(invalid=invalid), self.assertRaises(AssertionError):
                recovery.runtime_fixture_image(invalid)

    def test_original_terminal_preserves_failure_diagnostics_while_revision_bookkeeping_advances(self):
        before = {'operation_id': 'op', 'app_id': 'app', 'lifecycle_id': 'life',
                  'state': 'Failed', 'step': 'hot_converging', 'revision': 9,
                  'error_code': 'original-error', 'error_message': 'original failure'}
        self.assertTrue(recovery.historical_terminal_preserved(before, {**before, 'revision': 10}))
        for key, value in [('operation_id', 'other'), ('lifecycle_id', 'new'), ('state', 'Succeeded'),
                           ('step', 'completed'), ('error_code', None), ('error_message', None),
                           ('revision', 8), ('revision', True)]:
            with self.subTest(key=key):
                self.assertFalse(recovery.historical_terminal_preserved(before, {**before, key: value}))


class HotFixtureCleanupTests(unittest.TestCase):
    def checked_run(self):
        run = run_fixture()
        run.save = Mock()
        def check(_name, ok, detail=None):
            if not ok:
                raise AssertionError(detail)
        run.check = Mock(side_effect=check)
        return run

    def test_reply_loss_discovery_and_cleanup_use_exact_delete_options_never_pvc(self):
        run = self.checked_run()
        fixture = recovery.ArtifactFixture(run, 'image', {})
        fixture.intents = ['configmap']  # API accepted create, response/UID capture was lost.
        row = fixture_resource(run, 'ConfigMap')
        present = [True]
        def kube(*args):
            return json.dumps(row) if args[0] == 'get' and present[0] else ''
        run.kube = Mock(side_effect=kube)
        def remove(_run, verb, body, *args):
            self.assertEqual(verb, 'delete')
            self.assertEqual(body['preconditions'], {'uid': 'owned-fixture', 'resourceVersion': 'original-version'})
            self.assertIn('/configmaps/' + fixture.name, args[1])
            self.assertNotIn('persistentvolumeclaims', args[1])
            present[0] = False
            return {'kind': 'Status'}
        with patch.object(recovery, 'kube_json_input', side_effect=remove) as delete:
            fixture.cleanup()
        delete.assert_called_once()
        self.assertFalse(present[0])
        self.assertEqual(run.check.call_args.args[0], 'hot_recovery_fixture_cleanup')
        self.assertEqual(run.check.call_args.args[2]['pvc_delete_requests'], 0)

    def test_replaced_or_baseline_uid_is_never_deleted(self):
        for baseline, uid in [({'owned-fixture'}, 'owned-fixture'), (set(), 'replacement')]:
            run = self.checked_run()
            fixture = recovery.ArtifactFixture(run, 'image', {})
            fixture.baseline_uids = baseline
            fixture.intents = ['configmap']
            fixture.created['configmap'] = {'uid': 'owned-fixture'}
            run.kube = Mock(return_value=json.dumps(fixture_resource(run, 'ConfigMap', uid)))
            with patch.object(recovery, 'kube_json_input') as delete, self.assertRaises(AssertionError):
                fixture.cleanup()
            delete.assert_not_called()


class OriginalObserverBudgetTests(unittest.TestCase):
    def test_cleanup_observes_only_original_request_remaining_budget(self):
        coordinator = recovery.HotRecovery(run_fixture())
        thread = Mock()
        thread.is_alive.return_value = False
        coordinator.threads = [(thread, {}, 'original-request')]
        coordinator.associations['original-request'] = {'request_started_monotonic': 10.0}
        with patch.object(recovery.time, 'monotonic', return_value=50.0):
            coordinator.finish_threads()
        thread.join.assert_called_once_with(timeout=260.0)
        saved = coordinator.run.save.call_args.args
        self.assertEqual(saved[0], 'hot-final-http-observers.json')
        self.assertEqual(saved[1][0]['original_absolute_deadline_monotonic'], 310.0)
        self.assertEqual(saved[1][0]['waited_seconds'], 0)

    def test_expired_original_deadline_never_gets_a_new_cleanup_window(self):
        coordinator = recovery.HotRecovery(run_fixture())
        thread = Mock()
        thread.is_alive.return_value = True
        coordinator.threads = [(thread, {}, 'original-request')]
        coordinator.associations['original-request'] = {'request_started_monotonic': 10.0}
        with patch.object(recovery.time, 'monotonic', return_value=400.0), self.assertRaises(TimeoutError):
            coordinator.finish_threads()
        thread.join.assert_called_once_with(timeout=0)

    def test_missing_observer_identity_is_not_a_fresh_deadline(self):
        coordinator = recovery.HotRecovery(run_fixture())
        thread = Mock()
        coordinator.threads = [(thread, {}, 'unregistered-request')]
        with self.assertRaises(AssertionError):
            coordinator.finish_threads()
        thread.join.assert_not_called()


class ArtifactServerTests(unittest.TestCase):
    def with_server(self, test, budget=.5):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            control, artifacts = root / 'control', root / 'artifacts'
            artifacts.mkdir()
            (artifacts / 'B.zip').write_bytes(b'original exact ZIP bytes')
            (artifacts / 'C.zip').write_bytes(b'original exact C ZIP bytes')
            state = fixture_server.State(control)
            server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), fixture_server.handler(state, artifacts, budget))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                test(state, 'http://127.0.0.1:' + str(server.server_port))
            finally:
                (control / 'B.release').touch()
                (control / 'D.release').touch()
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)
                self.assertFalse(thread.is_alive())

    def test_real_get_waits_for_filesystem_barrier_then_serves_exact_original_bytes(self):
        def test(state, base):
            response = []
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            thread = threading.Thread(target=lambda: response.append(opener.open(base + '/B.zip', timeout=2).read()))
            thread.start()
            deadline = time.monotonic() + 1
            while not state.events:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.005)
            self.assertEqual(state.counts, {'B': 1, 'D': 0})
            self.assertTrue(thread.is_alive())
            self.assertEqual(response, [])
            (state.root / 'B.release').touch()
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
            self.assertEqual(response, [b'original exact ZIP bytes'])
            self.assertTrue(any(row['event'] == 'served' for row in state.events))
        self.with_server(test)

    def test_c_is_exact_read_only_unblocked_and_does_not_change_barrier_counts(self):
        def test(state, base):
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            for _ in range(2):
                with opener.open(base + '/C.zip', timeout=2) as response:
                    self.assertEqual(response.read(), b'original exact C ZIP bytes')
            self.assertEqual(state.counts, {'B': 0, 'D': 0})
            self.assertEqual(state.static_counts, {'C': 2})
            self.assertFalse((state.root / 'C.release').exists())
        self.with_server(test)

    def test_unreleased_real_get_has_a_finite_failure_deadline_and_no_http_control(self):
        def test(state, base):
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with self.assertRaises(urllib.error.HTTPError) as failure:
                opener.open(base + '/B.zip', timeout=2)
            self.assertEqual(failure.exception.code, 504)
            failure.exception.close()
            self.assertTrue(any(row['event'] == 'deadline' for row in state.events))
            for path in ('/release/B', '/control', '/E.zip'):
                with self.subTest(path=path), self.assertRaises(urllib.error.HTTPError) as failure:
                    opener.open(base + path, timeout=2)
                self.assertEqual(failure.exception.code, 404)
                failure.exception.close()
            self.assertFalse((state.root / 'B.release').exists())
        self.with_server(test, .03)


class HotScenarioContractTests(unittest.TestCase):
    def test_independent_hot_scenario_preserves_original_all_81_and_recreate_contract(self):
        self.assertEqual(len(acceptance.REQUIRED), 81)
        self.assertEqual(acceptance.SCENARIO_REQUIRED['all'], acceptance.REQUIRED)
        self.assertEqual(acceptance.SCENARIO_REQUIRED['hot_recovery'], recovery.REQUIRED)
        self.assertEqual(acceptance.SCENARIO_IDENTITIES['hot_recovery'], 'k8s_userapp_hot_recovery')
        self.assertTrue({'hot_expired_original_store_fenced', 'hot_convergence_original_failed',
                         'hot_convergence_fresh_request', 'hot_recovery_fixture_cleanup',
                         'hot_recovery_data_preserved', 'cleanup', 'baseline_preserved'} <= recovery.REQUIRED)

    def test_cold_a_consumes_its_verified_artifact_before_other_builds_can_retire_it(self):
        run = run_fixture()
        run.entries, run.user = ['http://test'], 'owner'
        calls = []
        run.build = Mock(side_effect=lambda version: (calls.append('build-' + version) or ({'url': 'artifact-' + version, 'sha256': 'actual'}, version)))
        def cold(*_args, **_kwargs):
            calls.append('cold-A')
            raise RuntimeError('end controlled observation before real I/O')
        run.api = Mock(side_effect=cold)
        with patch.object(recovery.HotRecovery, 'seed_data'), self.assertRaisesRegex(RuntimeError, 'controlled observation'):
            recovery.exercise(run)
        self.assertEqual(calls, ['build-A', 'cold-A'])
        run.build.assert_called_once_with('A')

    def test_hot_scenario_does_not_run_or_waive_the_original_81_or_recreate_steps(self):
        run = acceptance.Run.__new__(acceptance.Run)
        run.scenario = 'hot_recovery'
        run.prepare = Mock()
        run.workspace = Mock()
        run.recreate_lifecycle = Mock()
        run.dev_readonly_locator = Mock()
        run.deploy = Mock()
        with patch.object(recovery, 'exercise') as hot:
            run.exercise()
        hot.assert_called_once_with(run)
        run.prepare.assert_called_once()
        run.workspace.assert_called_once()
        run.recreate_lifecycle.assert_not_called()
        run.dev_readonly_locator.assert_not_called()
        run.deploy.assert_not_called()


if __name__ == '__main__':
    unittest.main()
