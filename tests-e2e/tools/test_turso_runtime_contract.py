import json
from pathlib import Path
import tempfile
import unittest
from turso_runtime_contract import service_config, parse_observer_output, validate_owned, isolated_config


class RuntimeIsolationTests(unittest.TestCase):
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
