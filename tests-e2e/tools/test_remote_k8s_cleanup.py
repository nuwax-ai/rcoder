import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import remote_k8s_cleanup as cleanup


class RemoteCleanupTests(unittest.TestCase):
    def test_foreign_namespace_refused(self):
        env = {'TEST_K8S_NS': 'rcoder-e2e-test', 'TEST_K8S_ENVIRONMENT_ID': 'mine'}
        with patch.object(cleanup, 'kube', return_value=json.dumps({'metadata': {'labels': {'rcoder.dev/environment': 'foreign'}}})):
            with self.assertRaises(ValueError):
                cleanup.validate(env)

    def test_only_exact_case_workloads_deleted_and_pvcs_retained(self):
        case, run = 'a' * 32, 'b' * 32
        identifier = 'ue' + case[:12] + '-lb'
        name = 'computer-agent-runner-' + identifier
        labels = {'rcoder.io/identifier': identifier, 'rcoder.io/service-type': 'computer-agent-runner',
                  'app.kubernetes.io/managed-by': 'rcoder-runtime'}
        env = {'TEST_K8S_NS': 'rcoder-e2e-test', 'TEST_K8S_CONTEXT': 'test'}
        claim = {'metadata': {'name': 'retained', 'uid': 'pvc-uid'}}
        def kube(_env, *args):
            if args[:2] == ('get', 'pvc'):
                return json.dumps({'items': [claim]})
            if args[:2] == ('get', 'sts,svc'):
                return json.dumps({'items': [
                    {'kind': 'StatefulSet', 'metadata': {'name': name, 'labels': labels}},
                    {'kind': 'Service', 'metadata': {'name': name + '-foreign', 'labels': labels}},
                    {'kind': 'Service', 'metadata': {'name': name + '-svc', 'labels': labels}},
                    {'kind': 'Service', 'metadata': {'name': name + '-headless', 'labels': labels}},
                    {'kind': 'StatefulSet', 'metadata': {'name': name}},
                ]})
            return ''
        with tempfile.TemporaryDirectory() as directory, patch.object(cleanup, 'validate'), patch.object(cleanup, 'kube', side_effect=kube) as command:
            self.assertEqual(cleanup.cleanup(case, run, Path(directory), env), [])
            deletes = [c.args[1:] for c in command.call_args_list if c.args[1] == 'delete']
            self.assertEqual(len(deletes), 3)
            self.assertEqual({c[2] for c in deletes}, {name, name + '-svc', name + '-headless'})
            self.assertTrue(all(c[1] in ['Service', 'StatefulSet'] for c in deletes))
            self.assertTrue(json.loads((Path(directory) / 'resources/k8s-cleanup.json').read_text())['ok'])


if __name__ == '__main__':
    unittest.main()
