import json
from pathlib import Path
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch
from cleanup import owned, cleanup_case

class OwnershipTests(unittest.TestCase):
    def container(self, name, cid='new', labels=None):
        return {'Id': cid, 'Name': '/' + name, 'Config': {'Labels': labels}}

    def test_other_case_and_unrelated_family_are_preserved(self):
        self.assertFalse(owned(self.container('rcoder-app-old-app'), 'abcdef123456', 'run', {}))
        self.assertFalse(owned(self.container('database-abcdef1234'), 'abcdef123456', 'run', {}))
        self.assertFalse(owned(self.container('rcoder-app-any'), '', 'run', {}))

    def test_receipt_requires_exact_id_and_name(self):
        receipt = {'old': 'dev-master-rcoder-generated'}
        self.assertTrue(owned(self.container('dev-master-rcoder-generated', 'old'), 'abcdef123456', 'run', receipt))
        self.assertFalse(owned(self.container('dev-master-rcoder-generated', 'replacement'), 'abcdef123456', 'run', receipt))
        self.assertFalse(owned(self.container('another', 'old'), 'abcdef123456', 'run', receipt))

    def test_preexisting_identity_cannot_be_deleted_even_with_matching_label(self):
        container = self.container('rcoder-review-existing', 'existing', {'rcoder.e2e.run': 'run'})
        with tempfile.TemporaryDirectory() as temp:
            with patch('cleanup.command', side_effect=['existing', json.dumps([container])]) as api:
                errors = cleanup_case('abcdef123456', 'run', Path(temp), ['existing'])
                self.assertTrue(any('refusing cleanup' in error for error in errors))
                self.assertEqual(api.call_count, 2)

    def test_pg_project_is_never_removed_as_a_bare_container(self):
        container = self.container('rcoder-pg-test-postgres-1', labels={'rcoder.e2e.run': 'run', 'com.docker.compose.project': 'rcoder-pg-0123456789abcdef'})
        with tempfile.TemporaryDirectory() as temp:
            with patch('cleanup.command', side_effect=['new', json.dumps([container])]) as api:
                errors = cleanup_case('abcdef123456', 'run', Path(temp))
                self.assertTrue(any('refusing bare container removal' in error for error in errors))
                self.assertEqual(api.call_count, 2)

    def test_pg_receipt_cannot_target_another_directory(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            pg = root / 'pg-contract'
            pg.mkdir()
            (pg / 'ownership.json').write_text(json.dumps({'run_id': 'run', 'case_id': 'case', 'project': 'rcoder-pg-0123456789abcdef', 'compose_file': '/unrelated/compose.json'}))
            with patch('cleanup.command', return_value='') as api:
                errors = cleanup_case('case', 'run', root)
                self.assertTrue(any('ownership cleanup failed' in error for error in errors))
                self.assertEqual(api.call_count, 1)

    def pg_pending(self, root):
        pg = root / 'pg-contract'
        pg.mkdir()
        receipt = {'run_id': 'run', 'case_id': 'case', 'project': 'rcoder-pg-0123456789abcdef',
                   'compose_file': str(pg / 'compose.json'), 'creation_state': 'pending'}
        (pg / 'ownership.json').write_text(json.dumps(receipt))
        return receipt

    def test_empty_inventory_cannot_confirm_cleanup_of_pending_pg_creation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.pg_pending(root)
            with patch('cleanup.command', return_value=''), patch('cleanup.subprocess.run', return_value=SimpleNamespace(returncode=0)):
                errors = cleanup_case('case', 'run', root)
            self.assertTrue(any('creation outcome unresolved' in error for error in errors))
            record = json.loads((root / 'resources/pg-project-fallback-cleanup.json').read_text())
            self.assertFalse(record['ok'])

    def test_late_owned_pg_container_settles_pending_creation_before_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            receipt = self.pg_pending(root)
            container = self.container(receipt['project'] + '-postgres-1', labels={
                'rcoder.e2e.run': 'run', 'com.docker.compose.project': receipt['project'],
                'com.docker.compose.service': 'postgres'})
            with patch('cleanup.command', side_effect=['new', json.dumps([container]), '']), patch('cleanup.subprocess.run', return_value=SimpleNamespace(returncode=0)):
                errors = cleanup_case('case', 'run', root)
            self.assertEqual(errors, [])
            saved = json.loads((root / 'pg-contract/ownership.json').read_text())
            self.assertEqual(saved['creation_state'], 'completed')
            self.assertEqual(saved['container_id'], 'new')

    def test_pg_cleanup_rejects_foreign_late_identity_before_compose_down(self):
        for foreign in ('owner', 'identity', 'preexisting'):
            with self.subTest(foreign=foreign), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                receipt = self.pg_pending(root)
                if foreign == 'identity':
                    receipt['container_id'] = 'original'
                    (root / 'pg-contract/ownership.json').write_text(json.dumps(receipt))
                container = self.container(receipt['project'] + '-postgres-1', labels={
                    'rcoder.e2e.run': 'foreign' if foreign == 'owner' else 'run',
                    'com.docker.compose.project': receipt['project'],
                    'com.docker.compose.service': 'postgres'})
                with patch('cleanup.command', side_effect=['new', json.dumps([container]), '']), patch('cleanup.subprocess.run') as mutate:
                    errors = cleanup_case('case', 'run', root, ['new'] if foreign == 'preexisting' else [])
                self.assertTrue(any('PG ownership cleanup failed' in error for error in errors))
                mutate.assert_not_called()

    def test_pg_cleanup_before_create_acceptance_is_known_complete(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            receipt = self.pg_pending(root)
            receipt['creation_state'] = 'not_started'
            (root / 'pg-contract/ownership.json').write_text(json.dumps(receipt))
            with patch('cleanup.command', return_value=''), patch('cleanup.subprocess.run', return_value=SimpleNamespace(returncode=0)):
                self.assertEqual(cleanup_case('case', 'run', root), [])

    def test_generic_cleanup_cannot_bypass_uncertain_hot_creation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            hot = root / 'hot-contract'
            hot.mkdir()
            name = 'rcoder-review-' + 'a' * 32
            (hot / 'ownership.json').write_text(json.dumps({'name': name}))
            late = self.container(name, labels={'rcoder.e2e.run': 'run'})
            with patch('hot_cleanup.cleanup', return_value={'ok': False, 'errors': ['creation outcome unresolved']}), patch('cleanup.command', side_effect=['new', json.dumps([late])]) as api:
                errors = cleanup_case('case', 'run', root)
            self.assertTrue(any('creation outcome unresolved' in error for error in errors))
            self.assertTrue(any('creation-aware cleanup' in error for error in errors))
            self.assertEqual(api.call_count, 2)

    def test_generic_cleanup_cannot_bypass_uncertain_artifact_creation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            context = root / 'build-artifacts'
            context.mkdir()
            name = 'rcoder-context-' + 'a' * 32
            (context / 'ownership.json').write_text(json.dumps({'container_name': name}))
            late = self.container(name, labels={'rcoder.e2e.run': 'run'})
            with patch('build_context_contract.cleanup', side_effect=ValueError('unresolved')), patch('cleanup.command', side_effect=['new', json.dumps([late])]) as api:
                errors = cleanup_case('case', 'run', root)
            self.assertTrue(any('creation-aware cleanup' in error for error in errors))
            self.assertEqual(api.call_count, 2)

    def test_docker_volume_cleanup_rejects_replacement_owner(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            journal = root / 'docker-lifecycle'
            journal.mkdir()
            volume = 'rcoder-test-identity-' + 'a' * 32
            (journal / 'ownership.json').write_text(json.dumps({'run_id': 'run', 'case_id': 'case', 'volume_name': volume}))
            with patch('cleanup.command', side_effect=['', volume, json.dumps([{'Labels': {'rcoder.e2e.run': 'other', 'rcoder.e2e.case': 'case'}}])]) as api:
                errors = cleanup_case('case', 'run', root)
                self.assertTrue(any('owned volume cleanup failed' in error for error in errors))
                self.assertEqual(api.call_count, 3)
                self.assertFalse(json.loads((root / 'resources/docker-volume-fallback-cleanup.json').read_text())['ok'])

    def test_reserved_case_and_explicit_run_label(self):
        self.assertTrue(owned(self.container('rcoder-app-abcdef1234-test'), 'abcdef123456', 'run', {}))
        self.assertTrue(owned(self.container('rcoder-review-id', labels={'rcoder.e2e.run': 'run'}), 'abcdef123456', 'run', {}))
        self.assertFalse(owned(self.container('rcoder-review-id', labels={'rcoder.e2e.run': 'other'}), 'abcdef123456', 'run', {}))

if __name__ == '__main__':
    unittest.main()
