import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from turso_runtime_contract import service_config, parse_observer_output, validate_owned, isolated_config, paired_isolated_config


class RuntimeIsolationTests(unittest.TestCase):
    def test_selected_runtime_enters_private_service_environment(self):
        runtime = 'sha256:' + 'd' * 64
        with patch.dict('os.environ', {'E2E_RUNTIME_IMAGE': runtime}, clear=True):
            service = service_config(Path('/owned'), 'controller', 'run', 'case', '/docker.sock')['services']['rcoder']
        self.assertEqual(service['environment']['RCODER_RUNTIME_IMAGE_DIGEST'], runtime)

    def test_supplied_paired_config_receipt_must_be_current_and_cannot_inherit_old_tags(self):
        source = ('cleanup_config:\n  enabled: true\nimages:\n'
                  '  controller: "dev-master-rcoder:latest"\n'
                  '  builder: "dev-rcoder-agent-runner:latest"\n')
        identity = {'origin_head': 'fixture', 'worktree_sha256': 'current'}
        build = {'version': 1, 'source': identity,
                 'rcoder': {'image_id': 'sha256:' + 'a' * 64, 'binary_sha256': 'b' * 64},
                 'builder': {'image_id': 'sha256:' + 'c' * 64,
                             'agent_runner_sha256': 'd' * 64, 'app_cli_sha256': 'e' * 64}}
        with tempfile.TemporaryDirectory() as temporary:
            receipt = Path(temporary) / 'build.json'
            receipt.write_text(json.dumps(build))
            base = {'E2E_STRICT': '1', 'E2E_COMPUTE_BUILD_RECEIPT': str(receipt)}
            with patch('prod_readiness_contract.source_identity', return_value=identity), \
                 patch('turso_runtime_contract.command', side_effect=AssertionError('no Docker side effects')):
                with patch.dict('os.environ', base, clear=True):
                    result = paired_isolated_config(source)
                self.assertIn(build['rcoder']['image_id'], result)
                self.assertIn(build['builder']['image_id'], result)
                self.assertNotIn('dev-master-rcoder:latest', result)
                self.assertIn('enabled: true', source, 'original source must remain unchanged')
                with patch.dict('os.environ', {'E2E_STRICT': '1'}, clear=True):
                    self.assertEqual(paired_isolated_config(source), isolated_config(source))
                with patch.dict('os.environ', {'E2E_STRICT': '1', 'E2E_BUILDER_IMAGE': 'explicit-builder'}, clear=True):
                    selected = paired_isolated_config(source)
                    self.assertIn('explicit-builder', selected)
                    self.assertIn('dev-master-rcoder:latest', selected)
                for name in ('E2E_TURSO_RUNTIME_IMAGE', 'E2E_TURSO_BINARY_SHA256', 'E2E_BUILDER_IMAGE'):
                    with self.subTest(name=name), patch.dict('os.environ', dict(base, **{name: 'different'}), clear=True):
                        with self.assertRaisesRegex(ValueError, 'disagrees with the paired build receipt'):
                            paired_isolated_config(source)
                receipt.write_text(json.dumps(dict(build, source={'origin_head': 'old', 'worktree_sha256': 'old'})))
                with patch.dict('os.environ', base, clear=True):
                    with self.assertRaisesRegex(ValueError, 'does not match frozen source'):
                        paired_isolated_config(source)
                with patch.dict('os.environ', {}, clear=True):
                    self.assertEqual(paired_isolated_config(source), isolated_config(source))

    def test_supplied_runtime_receipt_rejects_source_or_image_disagreement(self):
        identity = {'origin_head': 'fixture', 'worktree_sha256': 'current'}
        runtime = 'sha256:' + 'a' * 64
        build = {'version': 1, 'source': identity,
                 'rcoder': {'image_id': 'sha256:' + 'b' * 64, 'binary_sha256': 'c' * 64},
                 'runtime': {'image_id': runtime, 'app_cli_sha256': 'd' * 64}}
        with tempfile.TemporaryDirectory() as temporary:
            receipt = Path(temporary) / 'runtime.json'
            receipt.write_text(json.dumps(build))
            env = {'E2E_RUNTIME_IMAGE': runtime, 'E2E_PROD_BUILD_RECEIPT': str(receipt)}
            with patch('prod_readiness_contract.source_identity', return_value=identity):
                with patch.dict('os.environ', env, clear=True):
                    selected = service_config(Path('/owned'), 'controller', 'run', 'case', '/docker.sock')
                    self.assertEqual(selected['services']['rcoder']['environment']['RCODER_RUNTIME_IMAGE_DIGEST'], runtime)
                with patch.dict('os.environ', dict(env, E2E_RUNTIME_IMAGE='different'), clear=True):
                    with self.assertRaisesRegex(ValueError, 'disagrees with the paired runtime build receipt'):
                        service_config(Path('/owned'), 'controller', 'run', 'case', '/docker.sock')
                receipt.write_text(json.dumps(dict(build, source={'origin_head': 'old', 'worktree_sha256': 'old'})))
                with patch.dict('os.environ', env, clear=True):
                    with self.assertRaisesRegex(ValueError, 'does not match frozen source inputs'):
                        service_config(Path('/owned'), 'controller', 'run', 'case', '/docker.sock')

    def test_generated_service_has_private_writable_mounts_and_no_fixed_ports(self):
        root = Path('/private/run-owned')
        config = service_config(root, 'sha256:fixed', 'run', 'case', '/var/run/docker.sock')
        self.assertEqual(set(config['services']), {'rcoder'})
        service = config['services']['rcoder']
        self.assertEqual(service['ports'], ['127.0.0.1::8090'])
        self.assertEqual(service['restart'], 'no')
        self.assertEqual(service['pull_policy'], 'never')
        self.assertEqual(service['environment']['RCODER_USERAPP_STORAGE_BACKEND'], 'turso')
        self.assertEqual(service['environment']['RCODER_USERAPP_TURSO_PATH'], '/app/data/userapp.turso.db')
        for key in ('RCODER_AUTO_CLEANUP', 'RCODER_USERAPP_RECYCLE_ENABLED'):
            self.assertEqual(service['environment'][key], 'false')
        for mount in service['volumes']:
            if mount['target'] != '/var/run/docker.sock' and not mount.get('read_only'):
                self.assertTrue(Path(mount['source']).is_relative_to(root))
        self.assertNotIn('container_name', service)
        self.assertNotIn('network_mode', service)
        self.assertFalse(any(m['target'].startswith('/app/src') for m in service['volumes']))

    def test_owned_service_requires_all_project_run_case_labels(self):
        receipt = {'project': 'project', 'run_id': 'run', 'case_id': 'case'}
        labels = {'com.docker.compose.project': 'project', 'com.docker.compose.service': 'rcoder',
                  'rcoder.e2e.run': 'run', 'rcoder.e2e.case': 'case'}
        validate_owned({'Config': {'Labels': labels}}, receipt)
        for key in labels:
            changed = dict(labels, **{key: 'foreign'})
            with self.assertRaises(ValueError):
                validate_owned({'Config': {'Labels': changed}}, receipt)

    def test_isolated_configuration_disables_cleanup_without_mutating_source(self):
        source = 'cleanup_config:\n  # explanatory comment\n  enabled: true\n  idle_timeout_seconds: 1\n\nother:\n  enabled: true\n'
        result = isolated_config(source)
        self.assertIn('cleanup_config:\n  enabled: false', result)
        self.assertIn('other:\n  enabled: true', result)
        self.assertNotIn('idle_timeout_seconds', result)
        with self.assertRaises(ValueError):
            isolated_config('other: true\n')

    def test_observer_output_requires_sections_and_real_records(self):
        marker_life = json.dumps({'section': 'lifecycles'})
        marker_ops = json.dumps({'section': 'operations'})
        record_life = json.dumps({'app_id': 'app', 'lifecycle_id': 'life'})
        record_op = json.dumps({'app_id': 'app', 'operation_id': 'op', 'state': 'Succeeded'})
        lifecycles, operations = parse_observer_output(
            marker_life + '\n' + record_life + '\n' + marker_ops + '\n' + record_op + '\n')
        self.assertEqual(lifecycles, [{'app_id': 'app', 'lifecycle_id': 'life'}])
        self.assertEqual(operations, [{'app_id': 'app', 'operation_id': 'op', 'state': 'Succeeded'}])
        # 无 section 标记的行 / 缺 operations 段都不能当有效快照
        with self.assertRaises(RuntimeError):
            parse_observer_output(record_life)
        with self.assertRaises(RuntimeError):
            parse_observer_output(marker_life + '\n' + record_life)


if __name__ == '__main__':
    unittest.main()
