import json
import unittest
from unittest.mock import patch
import docker_fault_proxy as proxy


class FaultProxyScopeTests(unittest.TestCase):
    def test_creation_requires_exact_name_family_owner_and_lifecycle(self):
        labels = {'rcoder.io/application-id': 'app',
                  'service-type': 'user-app-builder', 'rcoder.io/lifecycle-id': 'life'}
        with patch.multiple(proxy, APP='app', OWNER='owner'):
            path = '/v1.47/containers/create?name=rcoder-app-builder-app'
            self.assertTrue(proxy.allowed('POST', path, json.dumps({'Labels': labels})))
            self.assertFalse(proxy.allowed('POST', path.replace('builder-app', 'builder-other'), json.dumps({'Labels': labels})))
            for field in labels:
                wrong = dict(labels)
                wrong[field] = ''
                self.assertFalse(proxy.allowed('POST', path, json.dumps({'Labels': wrong})))
            self.assertFalse(proxy.allowed('POST', '/images/create', b''))
            self.assertFalse(proxy.allowed('DELETE', '/networks/foreign', b''))

    def test_container_write_is_bound_to_recorded_physical_id_and_lifecycle(self):
        from types import SimpleNamespace
        labels = {'rcoder.io/application-id': 'app',
                  'service-type': 'user-app-builder', 'rcoder.io/lifecycle-id': 'life'}
        row = {'Id': 'created-id', 'Config': {'Labels': labels}}
        closed = []
        response = SimpleNamespace(status=200, read=lambda: json.dumps(row).encode())
        connection = SimpleNamespace(close=lambda: closed.append(True))
        with patch.multiple(proxy, APP='app', OWNER='owner', OWNED_CONTAINERS={'created-id': 'life'}), patch.object(proxy, 'exchange', return_value=(connection, response)):
            self.assertTrue(proxy.container_owned('created-id'))
            for field in ('service-type', 'rcoder.io/lifecycle-id'):
                old = labels[field]
                labels[field] = 'foreign'
                self.assertFalse(proxy.container_owned('created-id'))
                labels[field] = old
            row['Id'] = 'replacement-id'
            self.assertFalse(proxy.container_owned('created-id'))
        self.assertEqual(len(closed), 4)


if __name__ == '__main__':
    unittest.main()
