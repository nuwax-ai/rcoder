import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from sqlite_runtime_contract import service_config, snapshot_db, validate_owned, isolated_config


class RuntimeIsolationTests(unittest.TestCase):
    def test_generated_service_has_private_writable_mounts_and_no_fixed_ports(self):
        root = Path('/private/run-owned')
        config = service_config(root, 'sha256:fixed', 'run', 'case', '/var/run/docker.sock')
        self.assertEqual(set(config['services']), {'rcoder'})
        service = config['services']['rcoder']
        self.assertEqual(service['ports'], ['127.0.0.1::8090'])
        self.assertEqual(service['restart'], 'no')
        self.assertEqual(service['pull_policy'], 'never')
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

    def test_snapshot_requires_real_sqlite_rows_not_file_existence(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'data').mkdir()
            path = root / 'data/userapp.sqlite3'
            with sqlite3.connect(path) as database:
                database.execute('PRAGMA journal_mode=WAL')
                database.execute('CREATE TABLE userapp_lifecycles(app_id TEXT, record TEXT)')
                database.execute('CREATE TABLE userapp_operations(app_id TEXT, operation_id TEXT, record TEXT)')
                database.commit()
                with self.assertRaises(RuntimeError):
                    snapshot_db(root, 'app')
                database.execute('INSERT INTO userapp_lifecycles VALUES (?, ?)', ('app', json.dumps({'lifecycle_id': 'life'})))
                database.execute('INSERT INTO userapp_operations VALUES (?, ?, ?)', ('app', 'op', json.dumps({'state': 'Succeeded'})))
                database.commit()
                result = snapshot_db(root, 'app')
                self.assertEqual(result['lifecycle'], {'lifecycle_id': 'life'})
                self.assertEqual(result['operations'], {'op': {'state': 'Succeeded'}})
                self.assertEqual(result['inode'], path.stat().st_ino)


if __name__ == '__main__':
    unittest.main()
