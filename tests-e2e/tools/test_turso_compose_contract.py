import unittest
from pathlib import Path
from turso_compose_contract import validate


def service(environment=None, volumes=None, replicas=None, scale=None):
    service = {'environment': environment or {}, 'volumes': volumes or []}
    if replicas is not None:
        service['deploy'] = {'replicas': replicas}
    if scale is not None:
        service['scale'] = scale
    return service


def config(service_body, volumes=None, name='project'):
    return {'name': name, 'services': {'rcoder': service_body}, 'volumes': volumes or {}}


class TursoComposeContractTests(unittest.TestCase):
    def test_validate_requires_explicit_turso_backend_and_database_path(self):
        result = validate(config(service({'RCODER_USERAPP_STORAGE_BACKEND': 'turso',
                                          'RCODER_USERAPP_TURSO_PATH': '/app/data/userapp.turso.db'},
                                         [{'type': 'bind', 'source': '/data', 'target': '/app/data'}])),
                          '/data')
        self.assertEqual(result['backend'], 'turso')
        self.assertEqual(result['database'], '/app/data/userapp.turso.db')
        for environment in ({}, {'RCODER_USERAPP_STORAGE_BACKEND': 'postgres'},
                            {'RCODER_USERAPP_STORAGE_BACKEND': 'turso'},
                            {'RCODER_USERAPP_STORAGE_BACKEND': 'turso',
                             'RCODER_USERAPP_TURSO_PATH': '/workspace/userapp.turso.db'}):
            with self.subTest(environment=environment):
                with self.assertRaises(ValueError):
                    validate(config(service(environment,
                                            [{'type': 'bind', 'source': '/data', 'target': '/app/data'}])),
                             '/data')

    def test_single_replica_and_whole_directory_mount_are_enforced(self):
        ok = [{'type': 'bind', 'source': '/data', 'target': '/app/data'}]
        for kwargs in ({'replicas': 2}, {'scale': 2}):
            with self.subTest(kwargs=kwargs):
                with self.assertRaises(ValueError):
                    validate(config(service({'RCODER_USERAPP_STORAGE_BACKEND': 'turso',
                                             'RCODER_USERAPP_TURSO_PATH': '/app/data/userapp.turso.db'}, ok, **kwargs)),
                             '/data')
        for bad in ([], ok + [{'type': 'bind', 'source': '/foreign', 'target': '/app/data/userapp.turso.db'}],
                    [{'type': 'bind', 'source': '/data', 'target': '/app/data', 'read_only': True}]):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    validate(config(service({'RCODER_USERAPP_STORAGE_BACKEND': 'turso',
                                             'RCODER_USERAPP_TURSO_PATH': '/app/data/userapp.turso.db'}, bad)),
                             '/data')

    def test_named_volume_variant_must_be_project_scoped_and_not_external(self):
        mounts = [{'type': 'volume', 'source': 'rcoder-userapp-data', 'target': '/app/data'}]
        environment = {'RCODER_USERAPP_STORAGE_BACKEND': 'turso',
                       'RCODER_USERAPP_TURSO_PATH': '/app/data/userapp.turso.db'}
        result = validate(config(service(environment, mounts),
                                 {'rcoder-userapp-data': {'name': 'project_rcoder-userapp-data'}},
                                 name='project'),
                          named_volume=True)
        self.assertEqual(result['mount_type'], 'volume')
        for definition in ({'name': 'project_rcoder-userapp-data', 'external': True},
                           {'name': 'other_rcoder-userapp-data'}, {}):
            with self.subTest(definition=definition):
                with self.assertRaises(ValueError):
                    validate(config(service(environment, mounts),
                                    {'rcoder-userapp-data': definition}, name='project'),
                            named_volume=True)


if __name__ == '__main__':
    unittest.main()
