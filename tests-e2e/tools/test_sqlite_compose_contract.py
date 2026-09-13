import copy
import unittest
from sqlite_compose_contract import validate


class SQLiteComposeContract(unittest.TestCase):
    def configuration(self):
        return {'name': 'isolated', 'services': {'rcoder': {
            'environment': {'RCODER_USERAPP_STORAGE_BACKEND': 'sqlite',
                            'RCODER_USERAPP_SQLITE_PATH': '/app/data/userapp.sqlite3'},
            'volumes': [{'type': 'bind', 'source': '/dedicated/data', 'target': '/app/data'}],
        }}}

    def test_entire_directory_mount(self):
        evidence = validate(self.configuration(), '/dedicated/data')
        self.assertEqual(evidence['evidence_level'], 'compose_configuration_only')

    def test_file_mount_readonly_wrong_directory_and_replicas_are_rejected(self):
        for mutation in ('file', 'readonly', 'wrong-source', 'replicas', 'nested'):
            with self.subTest(mutation=mutation):
                config = self.configuration()
                service = config['services']['rcoder']
                mount = service['volumes'][0]
                if mutation == 'file':
                    mount['target'] = '/app/data/userapp.sqlite3'
                elif mutation == 'readonly':
                    mount['read_only'] = True
                elif mutation == 'wrong-source':
                    mount['source'] = '/foreign/data'
                elif mutation == 'replicas':
                    service['deploy'] = {'replicas': 2}
                else:
                    service['volumes'].append({'type': 'bind', 'source': '/foreign/file', 'target': '/app/data/userapp.sqlite3-wal'})
                with self.assertRaises(ValueError):
                    validate(config, '/dedicated/data')

    def test_named_volume_must_remain_project_scoped(self):
        config = self.configuration()
        config['services']['rcoder']['volumes'][0] = {'type': 'volume', 'source': 'rcoder-userapp-data', 'target': '/app/data'}
        config['volumes'] = {'rcoder-userapp-data': {'name': 'isolated_rcoder-userapp-data'}}
        validate(config, named_volume=True)
        for field, value in [('external', True), ('name', 'shared-global-data')]:
            changed = copy.deepcopy(config)
            changed['volumes']['rcoder-userapp-data'][field] = value
            with self.assertRaises(ValueError):
                validate(changed, named_volume=True)

    def test_wrong_backend_or_database_is_rejected(self):
        for field, value in [('RCODER_USERAPP_STORAGE_BACKEND', 'auto'),
                             ('RCODER_USERAPP_SQLITE_PATH', '/workspace/userapp.sqlite3')]:
            config = self.configuration()
            config['services']['rcoder']['environment'][field] = value
            with self.assertRaises(ValueError):
                validate(config, '/dedicated/data')
