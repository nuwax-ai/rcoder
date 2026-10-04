"""Tool contracts only: no Docker, real prod acceptance is a separate run."""
import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import prod_readiness_contract as contract


class InputContracts(unittest.TestCase):
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
