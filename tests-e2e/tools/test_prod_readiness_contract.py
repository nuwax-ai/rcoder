"""Tool contracts only: no Docker, real prod acceptance is a separate run."""
import copy
import json
import os
from pathlib import Path
import re
import tempfile
from types import SimpleNamespace
import tomllib
import unittest
from unittest.mock import patch
import zipfile

import prod_readiness_contract as contract


class InputContracts(unittest.TestCase):
    def test_runtime_restoration_retains_generation_without_replaying_the_previous_request(self):
        before = ['APP_DEPLOY_URL=http://artifact.invalid/old.zip', 'APP_RELEASE_ID=old-release',
                  'APP_DEPLOY_SHA256=old-sha', 'APP_DEPLOY_OPERATION_ID=old-request',
                  'APP_DEPLOY_GENERATION_ID=stable-generation', 'APP_ID=ownedapp', 'BUSINESS_SECRET=x=y']
        after = ['APP_DEPLOY_GENERATION_ID=stable-generation', 'APP_ID=ownedapp', 'BUSINESS_SECRET=x=y']
        retained = contract.retained_runtime_environment(before)
        self.assertEqual(retained, dict(entry.split('=', 1) for entry in after))
        self.assertNotEqual(retained, contract.retained_runtime_environment(
            [entry.replace('stable-generation', 'another-generation') for entry in after]))

    def test_generated_probe_artifact_matches_current_manifest_protocol(self):
        repo = Path(__file__).resolve().parents[2]
        types = (repo / 'crates/workspace-manifest/src/types.rs').read_text()

        def declaration(kind, name):
            found = re.search(r'pub ' + kind + r' ' + name + r'\s*\{(.*?)\n\}', types, re.S)
            self.assertIsNotNone(found, 'current manifest protocol declaration missing: ' + name)
            return found.group(1)

        # Read the real Rust enum vocabulary instead of validating the fixture
        # against a copied list that could repeat its invalid "api" assumption.
        kinds = {name.lower() for name in re.findall(r'^\s*([A-Z]\w*),\s*$', declaration('enum', 'ProjectKind'), re.M)}
        languages = {name.lower() for name in re.findall(r'^\s*([A-Z]\w*),\s*$', declaration('enum', 'ProjectType'), re.M)}

        def fields(name):
            return set(re.findall(r'^\s*pub (?:r#)?(\w+)\s*:', declaration('struct', name), re.M))

        with tempfile.TemporaryDirectory() as directory:
            artifact = contract.make_artifact(repo, Path(directory), 'fixture-release', 'FIXTURE_MARKER')
            with zipfile.ZipFile(artifact) as archive:
                self.assertIsNone(archive.testzip())
                lock = tomllib.loads(archive.read('release.lock.toml').decode())
                self.assertLessEqual(set(lock), fields('ReleaseLock'))
                self.assertEqual({service['service_id'] for service in lock['services']}, {'web', 'probe'})
                for service in lock['services']:
                    self.assertLessEqual(set(service), fields('LockedService'))
                    self.assertIn(service['kind'], kinds)
                    self.assertIn(service['type'], languages)
                    for section, rust_type in [('run', 'RunSection'), ('health', 'HealthSection'), ('proxy', 'ProxySection')]:
                        self.assertLessEqual(set(service[section]), fields(rust_type))
                    project = tomllib.loads(archive.read(service['dir'] + '/project.manifest.toml').decode())
                    self.assertLessEqual(set(project), fields('ProjectManifest'))
                    self.assertLessEqual(set(project['project']), fields('ProjectMeta'))
                    self.assertEqual(project['project'].get('kind', 'web'), service['kind'])
                    self.assertEqual(project['project']['type'], service['type'])
                probe = next(service for service in lock['services'] if service['service_id'] == 'probe')
                project = tomllib.loads(archive.read('probe/project.manifest.toml').decode())
                self.assertEqual(probe['run']['command'], project['run']['command'])
                self.assertEqual(probe['run']['migrate'], project['run']['migrate'])
                self.assertGreater(probe['run']['shutdown_timeout_seconds'], 0)
                self.assertTrue(all(path.startswith('/') for path in probe['health'].values()))
                self.assertEqual(probe['proxy']['path'], '/contract-probe')
                for argv in (probe['run']['command'], probe['run']['migrate']):
                    self.assertEqual(argv[0], 'python3')
                    path = 'probe/' + argv[1]
                    compile(archive.read(path), path, 'exec')

    def test_snapshot_without_git_hashes_actual_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'Cargo.toml').write_text('version = "1"')
            manifest = root / 'inputs.json'
            manifest.write_text(json.dumps({'Cargo.toml': {'sha256': 'frozen'}}))
            with patch.dict(os.environ, {'E2E_ORIGIN_HEAD': 'baseline'}):
                initial = contract.source_identity(root, manifest)
                self.assertEqual(initial['origin_head'], 'baseline')
                (root / 'Cargo.toml').write_text('version = "2"')
                self.assertNotEqual(contract.source_identity(root, manifest), initial)

    def test_snapshot_rejects_escaping_path(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'inputs.json'
            path.write_text(json.dumps({'../secret': {}}))
            with patch.dict(os.environ, {'E2E_ORIGIN_HEAD': 'baseline'}):
                with self.assertRaisesRegex(ValueError, 'escapes'):
                    contract.source_identity(Path(directory), path)

    def test_build_receipt_binds_current_inputs_and_both_binaries(self):
        source = {'origin_head': 'baseline', 'worktree_sha256': 'a' * 64}
        receipt = {'version': 1, 'source': source,
                   'rcoder': {'image_id': 'sha256:' + 'b' * 64, 'binary_sha256': 'c' * 64},
                   'runtime': {'image_id': 'sha256:' + 'd' * 64, 'app_cli_sha256': 'e' * 64}}
        self.assertEqual(contract.validate_receipt(receipt, source), receipt)
        with self.assertRaisesRegex(ValueError, 'frozen source'):
            contract.validate_receipt(receipt, {**source, 'worktree_sha256': 'f' * 64})
        for role, field in [('runtime', 'app_cli_sha256'), ('rcoder', 'binary_sha256'), ('runtime', 'image_id')]:
            missing = copy.deepcopy(receipt)
            del missing[role][field]
            with self.assertRaises(ValueError):
                contract.validate_receipt(missing, source)

    def test_cold_declaration_rejects_duplicate_or_foreign_identity(self):
        expected = {'APP_DEPLOY_OPERATION_ID': 'cold-A', 'APP_DEPLOY_GENERATION_ID': 'cold-A'}
        good = ['APP_DEPLOY_OPERATION_ID=cold-A', 'APP_DEPLOY_GENERATION_ID=cold-A', 'PATH=/bin']
        self.assertTrue(contract.declaration_matches(good, expected))
        self.assertFalse(contract.declaration_matches(good + ['APP_DEPLOY_OPERATION_ID=cold-A'], expected))
        self.assertFalse(contract.declaration_matches(['APP_DEPLOY_OPERATION_ID=cold-A',
                         'APP_DEPLOY_GENERATION_ID=native-session'], expected))
        self.assertFalse(contract.declaration_matches(['APP_DEPLOY_OPERATION_ID=cold-A'], expected))


class FormalReadOnlyQueryContracts(unittest.TestCase):
    def exercise(self, status, envelope, compute_ids=()):
        report = {'checks': []}
        fixture = contract.Contract(SimpleNamespace(user_id='fixture-user'), report, 'formalapp')
        with patch.object(fixture, 'readiness', return_value={'ready': False, 'container': {'status': 'missing'}}), \
             patch.object(fixture, 'api_raw', return_value=(status, envelope)) as query, \
             patch.object(fixture, 'application_ids', return_value=list(compute_ids)), \
             patch.object(contract.time, 'sleep'), patch.object(contract, 'run') as docker, \
             patch('builtins.print'):
            fixture.assert_deleted_queries_read_only('formal-query')
            docker.assert_not_called()
            return report, [call.args[0] for call in query.call_args_list]

    def test_formal_error_envelope_observes_missing_compute_without_creating(self):
        # app_manager::routes installs userapp_http::envelope_errors: status 200
        # is only transport success; exact code + false still describe failure.
        report, paths = self.exercise(200, {'success': False, 'code': 'CONTAINER_NOT_FOUND'})
        self.assertTrue(all(row['ok'] is True for row in report['checks']))
        self.assertEqual(paths, [
            '/api/v1/userapp/formalapp/prod/stats?user_id=fixture-user',
            '/api/v1/userapp/formalapp',
        ] * 2)
        self.assertIn('deleted prod queries remain read only', {row['name'] for row in report['checks']})

    def test_transport_404_wrong_semantic_errors_and_success_cannot_pass(self):
        cases = [(404, {'success': False, 'code': 'CONTAINER_NOT_FOUND'}),
                 (200, {'success': True, 'code': 'CONTAINER_NOT_FOUND'}),
                 (200, {'success': False, 'code': 'ERR_APP_NOT_FOUND'}),
                 (200, {'success': False, 'code': 'ERR_BACKEND_ERROR'}),
                 (200, {'success': False, 'code': 'ERR_VALIDATION'}),
                 (200, {'success': False, 'code': '0000'}),
                 (200, {'success': 0, 'code': 'CONTAINER_NOT_FOUND'})]
        for status, envelope in cases:
            with self.subTest(status=status, envelope=envelope):
                with self.assertRaises(RuntimeError):
                    self.exercise(status, envelope)

    def test_query_side_effect_cannot_pass_even_with_the_expected_error_envelope(self):
        with self.assertRaises(RuntimeError):
            self.exercise(200, {'success': False, 'code': 'CONTAINER_NOT_FOUND'}, ['unexpected-runtime'])


class InvalidArtifactEnvelopeContracts(unittest.TestCase):
    def exercise(self, status=200, response_patch=None, record_patch=None, fetched=True):
        report = {'checks': []}
        fixture = contract.Contract(SimpleNamespace(), report, 'artifactapp')
        response = {'success': False, 'code': 'ERR_BACKEND_ERROR', 'operation_id': 'invalid-op',
                    'message': 'invalid ZIP archive: central directory is missing'}
        response.update(response_patch or {})
        record = {'operation_id': 'invalid-op', 'app_id': 'artifactapp', 'state': 'Failed',
                  'error_code': 'ERR_BACKEND_ERROR', 'error_message': 'invalid ZIP archive'}
        record.update(record_patch or {})
        with patch.object(fixture, 'api', return_value=(200, {'success': True, 'code': '0000', 'data': record})) as query, \
             patch.object(contract, 'run') as docker, patch('builtins.print'):
            failed_id = fixture.verify_invalid_artifact_failure(status, response, [{'path': '/invalid.zip'}] if fetched else [])
            docker.assert_not_called()
            query.assert_called_once_with('/api/v1/userapp/artifactapp/operations/invalid-op')
            return failed_id, report

    def test_formal_200_error_keeps_the_exact_failed_operation_and_actual_bad_download(self):
        failed_id, report = self.exercise()
        self.assertEqual(failed_id, 'invalid-op')
        self.assertTrue(all(row['ok'] is True for row in report['checks']))

    def test_unrelated_rejection_or_foreign_receipt_cannot_masquerade_as_a_bad_artifact(self):
        cases = [dict(status=500), dict(response_patch={'success': True}),
                 dict(response_patch={'code': '0000'}), dict(response_patch={'code': 'ERR_VALIDATION'}),
                 dict(response_patch={'operation_id': None}), dict(fetched=False),
                 dict(response_patch={'message': 'Backend service failed'}),
                 dict(record_patch={'operation_id': 'foreign-op'}), dict(record_patch={'app_id': 'foreign-app'}),
                 dict(record_patch={'state': 'Succeeded'}), dict(record_patch={'error_code': 'ERR_CONFLICT'})]
        for kwargs in cases:
            with self.subTest(kwargs=kwargs):
                with self.assertRaises(RuntimeError):
                    self.exercise(**kwargs)


class RuntimeCommitObservationContracts(unittest.TestCase):
    def fixture(self):
        return contract.Contract(SimpleNamespace(), {'checks': []}, 'ownedfixture')

    def status(self, phase='orchestrating', **changes):
        operation = {'operation_id': 'original-op', 'deployment_generation_id': 'original-generation',
                     'request_release_id': 'release-b', 'artifact_release_id': 'release-b',
                     'phase': phase, 'persisted': True, 'deploy_stage': 'succeeded'}
        operation.update(changes)
        return {'phase': phase, 'operation': operation}

    def test_two_orchestrating_observations_wait_for_the_exact_running_receipt(self):
        fixture = self.fixture()
        with patch.object(fixture, 'runtime_json', side_effect=[self.status(), self.status(), self.status('running')]) as observe, \
             patch.object(contract.time, 'sleep') as sleep, patch.object(contract, 'run') as docker, patch('builtins.print'):
            final = fixture.wait_runtime_commit('captured-physical-uid', 'restored', 'original-op', 'original-generation', 'release-b')
        self.assertEqual(final['operation']['phase'], 'running')
        self.assertEqual(observe.call_count, 3)
        self.assertTrue(all(call.args == ('captured-physical-uid', '/v1/deploy/status') for call in observe.call_args_list))
        self.assertEqual(sleep.call_count, 2)
        docker.assert_not_called()

    def test_changed_identity_failed_or_uncommitted_running_is_not_a_startup_transition(self):
        cases = [self.status(operation_id='foreign-op'), self.status(deployment_generation_id='foreign-generation'),
                 self.status(request_release_id='release-a'), self.status(artifact_release_id='release-a'),
                 self.status('failed'), self.status('idle'), self.status('deploying'),
                 self.status('running', persisted=False), self.status('running', deploy_stage='failed')]
        for status in cases:
            with self.subTest(operation=status['operation']):
                fixture = self.fixture()
                with patch.object(fixture, 'runtime_json', return_value=status) as observe, \
                     patch.object(contract.time, 'sleep') as sleep, patch.object(contract, 'run') as docker, patch('builtins.print'):
                    with self.assertRaises(RuntimeError):
                        fixture.wait_runtime_commit('captured-physical-uid', 'restored', 'original-op', 'original-generation', 'release-b')
                self.assertEqual(observe.call_count, 1)
                sleep.assert_not_called()
                docker.assert_not_called()

    def test_a_finite_orchestration_wait_cannot_report_timeout_as_success(self):
        fixture = self.fixture()
        with patch.object(fixture, 'runtime_json', return_value=self.status()) as observe, \
             patch.object(contract.time, 'monotonic', side_effect=[0.0, 0.0, 61.0]), \
             patch.object(contract.time, 'sleep'), patch.object(contract, 'run') as docker, patch('builtins.print'):
            with self.assertRaisesRegex(RuntimeError, 'running receipt is committed'):
                fixture.wait_runtime_commit('captured-physical-uid', 'restored', 'original-op', 'original-generation', 'release-b')
        self.assertEqual(observe.call_count, 1)
        self.assertIs(fixture.report['checks'][-1]['ok'], False)
        self.assertEqual(fixture.report['checks'][-1]['detail']['timeout_seconds'], 60)
        docker.assert_not_called()


class ControllerRestartEndpointContracts(unittest.TestCase):
    def fixture(self, root):
        root = root.resolve()
        receipt = {'version': 1, 'root': str(root), 'project': 'rcoder-prod-' + 'a' * 16,
                   'run_id': 'run', 'case_id': 'case', 'app_id': 'private-app',
                   'rcoder_image_id': 'sha256:' + 'b' * 64, 'controller_id': 'captured-controller',
                   'base': 'http://127.0.0.1:32777'}
        labels = {'com.docker.compose.project': receipt['project'], 'com.docker.compose.service': 'rcoder',
                  'rcoder.e2e.run': 'run', 'rcoder.e2e.case': 'case'}
        row = {'Id': receipt['controller_id'], 'Image': receipt['rcoder_image_id'],
               'Name': '/' + receipt['project'] + '-rcoder-1', 'State': {'Running': True},
               'Config': {'Labels': labels},
               'Mounts': [{'Type': 'bind', 'Source': str(root / folder), 'Destination': '/app/' + folder}
                          for folder in ('data', 'logs', 'project_workspace', 'computer-project-workspace',
                                         'userapp-workspace', 'app-workspace')],
               'NetworkSettings': {'Ports': {'8090/tcp': [{'HostIp': '127.0.0.1', 'HostPort': '32777'}],
                                             '8088/tcp': [{'HostIp': '127.0.0.1', 'HostPort': '32778'}]}}}
        fixture = contract.Contract(SimpleNamespace(controller_container=row['Id'], url=receipt['base']),
                                    {'checks': []}, 'private-app')
        fixture.owned_controller = receipt
        fixture.fixture_root = root
        fixture.proxy_base = 'http://127.0.0.1:32778'
        fixture.build_receipt = {'rcoder': {'image_id': row['Image'], 'binary_sha256': 'c' * 64}}
        return fixture, row

    def test_restart_refreshes_both_random_ports_and_persists_before_waiting(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture, before = self.fixture(Path(directory))
            after = copy.deepcopy(before)
            after['NetworkSettings']['Ports']['8090/tcp'][0]['HostPort'] = '32779'
            after['NetworkSettings']['Ports']['8088/tcp'][0]['HostPort'] = '32780'
            calls = []
            def docker(argv, **_kwargs):
                calls.append(argv)
                if argv[1] == 'inspect':
                    return json.dumps([before if len(calls) == 1 else after])
                if argv[1] == 'exec':
                    return 'c' * 64 + '  /app/bin/rcoder'
                self.assertEqual(argv, ['docker', 'restart', '-t', '30', before['Id']])
                return ''
            def ready(base):
                self.assertEqual(base, 'http://127.0.0.1:32779')
                self.assertEqual(fixture.args.url, base)
                self.assertEqual(fixture.proxy_base, 'http://127.0.0.1:32780')
                persisted = json.loads((Path(directory) / 'ownership.json').read_text())
                self.assertEqual(persisted['base'], base)
                self.assertEqual(persisted['controller_id'], before['Id'])
            with patch.object(contract, 'run', side_effect=docker), \
                 patch('turso_runtime_contract.wait_ready', side_effect=ready) as wait, \
                 patch.object(fixture, 'current', return_value=('same-runtime', {})), patch('builtins.print'):
                fixture.restart_owned_controller('same-runtime')
            wait.assert_called_once_with('http://127.0.0.1:32779')
            self.assertEqual(sum(argv[1] == 'restart' for argv in calls), 1)
            self.assertEqual([argv[1] for argv in calls], ['inspect', 'restart', 'inspect', 'exec'])

    def test_each_endpoint_requires_one_actual_loopback_mapping(self):
        with tempfile.TemporaryDirectory() as directory:
            _, row = self.fixture(Path(directory))
            self.assertEqual(contract.controller_loopback_endpoints(row),
                             ('http://127.0.0.1:32777', 'http://127.0.0.1:32778'))
            for port in ('8090/tcp', '8088/tcp'):
                for bindings in ([], [{'HostIp': '0.0.0.0', 'HostPort': '32777'}],
                                 [{'HostIp': '192.0.2.1', 'HostPort': '32777'}],
                                 [{'HostIp': '127.0.0.1', 'HostPort': '32777'}] * 2,
                                 [{'HostIp': '127.0.0.1', 'HostPort': '32777'}, {'HostIp': '0.0.0.0', 'HostPort': '32777'}],
                                 [{'HostIp': '127.0.0.1', 'HostPort': '0'}],
                                 [{'HostIp': '127.0.0.1', 'HostPort': '65536'}]):
                    with self.subTest(port=port, bindings=bindings):
                        invalid = copy.deepcopy(row)
                        invalid['NetworkSettings']['Ports'][port] = bindings
                        with self.assertRaises(ValueError):
                            contract.controller_loopback_endpoints(invalid)
            row['NetworkSettings']['Ports']['8088/tcp'][0]['HostIp'] = '::1'
            self.assertEqual(contract.controller_loopback_endpoints(row)[1], 'http://[::1]:32778')

    def test_changed_controller_identity_is_rejected_before_waiting_or_rebinding(self):
        with tempfile.TemporaryDirectory() as directory:
            for change in ('Id', 'Image', 'ownership'):
                with self.subTest(change=change):
                    fixture, before = self.fixture(Path(directory))
                    after = copy.deepcopy(before)
                    if change == 'ownership':
                        after['Config']['Labels']['rcoder.e2e.run'] = 'foreign-run'
                    else:
                        after[change] = 'foreign-value'
                    with patch.object(contract, 'run', side_effect=[json.dumps([before]), '', json.dumps([after])]) as docker, \
                         patch('turso_runtime_contract.wait_ready') as wait, patch('builtins.print'):
                        with self.assertRaises(ValueError):
                            fixture.restart_owned_controller('same-runtime')
                    wait.assert_not_called()
                    self.assertEqual(docker.call_count, 3)
                    self.assertEqual(fixture.args.url, 'http://127.0.0.1:32777')
                    self.assertFalse((Path(directory) / 'ownership.json').exists())


class OwnedPreparationContracts(unittest.TestCase):
    def receipt(self, root, state='unknown'):
        return {'version': 1, 'root': str(root), 'project': 'rcoder-prod-' + 'a' * 16,
                'run_id': 'run', 'case_id': 'case', 'app_id': 'private-app',
                'rcoder_image_id': 'sha256:' + 'b' * 64, 'runtime_image_id': 'sha256:' + 'c' * 64,
                'controller_state': state, 'application_state': 'not_started',
                'controller_id': None, 'application_containers': {}}

    def test_source_mismatch_does_not_create_controller_or_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / 'not-created'
            with patch.object(contract, 'run') as docker:
                with self.assertRaisesRegex(ValueError, 'source'):
                    contract.prepare_controller(root, Path(directory), {'version': 1, 'source': {'old': True}},
                                                {'current': True}, 'run', 'case', 'app')
                docker.assert_not_called()
                self.assertFalse(root.exists())

    def test_unknown_empty_inventory_is_not_cleanup_success(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            contract.persist_ownership(root, self.receipt(root))
            with patch.object(contract, 'run', return_value='') as docker:
                result = contract.cleanup(root, 'run', 'case')
            self.assertFalse(result['ok'])
            self.assertIn('unknown', result['detail'])
            self.assertEqual(docker.call_count, 1)
            self.assertEqual(docker.call_args.args[0][:3], ['docker', 'ps', '-aq'])

    def test_foreign_controller_is_not_stopped_or_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            receipt = self.receipt(root)
            contract.persist_ownership(root, receipt)
            foreign = {'Id': 'physical-id', 'Image': receipt['rcoder_image_id'],
                       'Name': '/' + receipt['project'] + '-rcoder-1', 'Mounts': [],
                       'Config': {'Labels': {'com.docker.compose.project': receipt['project'],
                                            'com.docker.compose.service': 'rcoder',
                                            'rcoder.e2e.run': 'another-run', 'rcoder.e2e.case': 'case'}}}
            calls = []
            def docker(argv, **_kwargs):
                calls.append(argv)
                return 'physical-id' if argv[1] == 'ps' else json.dumps([foreign])
            with patch.object(contract, 'run', side_effect=docker):
                with self.assertRaisesRegex(ValueError, 'ownership'):
                    contract.cleanup(root, 'run', 'case')
            self.assertTrue(all(argv[1] in ('ps', 'inspect') for argv in calls))

    def test_preexisting_controller_is_not_cleaned(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            contract.persist_ownership(root, self.receipt(root))
            row = {'Id': 'preexisting'}
            with patch.object(contract, 'run', side_effect=['preexisting', json.dumps([row])]):
                with self.assertRaisesRegex(ValueError, 'preexisting'):
                    contract.cleanup(root, 'run', 'case', existing_ids={'preexisting'})


class CompletionContracts(unittest.TestCase):
    def good(self):
        return {'checks': [{'name': name, 'ok': True} for name in contract.REQUIRED_STEPS],
                'cleanup': {'captured_containers_removed': True, 'volumes_removed': False}}

    def test_each_mandatory_step_is_required(self):
        contract.require_report_complete(self.good())
        for name in contract.REQUIRED_STEPS:
            report = self.good()
            report['checks'] = [row for row in report['checks'] if row['name'] != name]
            with self.assertRaisesRegex(ValueError, 'missing required steps'):
                contract.require_report_complete(report)

    def test_failure_and_cleanup_cannot_be_success(self):
        for mutate in (lambda report: report['checks'].append({'name': 'extra', 'ok': False}),
                       lambda report: report['cleanup'].update(captured_containers_removed=False),
                       lambda report: report['cleanup'].update(volumes_removed=True)):
            report = self.good()
            mutate(report)
            with self.assertRaises(ValueError):
                contract.require_report_complete(report)



if __name__ == '__main__':
    unittest.main()
