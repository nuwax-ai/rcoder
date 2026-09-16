import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import Config, LABEL
import main
import manifests
import snapshot


class WorkflowTests(unittest.TestCase):
    def config(self, directory):
        file = Path(directory) / 'config'
        file.write_text('REMOTE_K8S_SSH=test-host\nREMOTE_K8S_DIR=/tmp/isolated/project\nREMOTE_K8S_CONTEXT=test\n')
        with patch.dict(os.environ, {}, clear=True):
            return Config(file)

    def test_render_isolated_and_retains_storage(self):
        with tempfile.TemporaryDirectory() as temp:
            c = self.config(temp)
            images = {x: 'example/' + x + '@sha256:' + 'a' * 64 for x in ['rcoder', 'computer', 'runtime']}
            rows = manifests.render(c, images, 'test-only')
            for row in rows:
                self.assertEqual(row['metadata']['labels'][LABEL], c.id)
                if 'namespace' in row['metadata']:
                    self.assertEqual(row['metadata']['namespace'], c.ns)
            pv = next(r for r in rows if r['kind'] == 'PersistentVolume')
            self.assertEqual(pv['spec']['persistentVolumeReclaimPolicy'], 'Retain')
            self.assertEqual(pv['spec']['claimRef']['namespace'], c.ns)
            dep = next(r for r in rows if r['kind'] == 'Deployment')
            self.assertEqual(dep['spec']['replicas'], 2)
            self.assertEqual(dep['spec']['template']['spec']['containers'][0]['image'], images['rcoder'])
            config = json.loads(next(r for r in rows if r['kind'] == 'ConfigMap')['data']['config.yml'])
            self.assertEqual(config['docker_config']['multi_image_config']['services'], config['kubernetes_config']['services'])
            self.assertEqual(config['kubernetes_config']['services']['user-app-builder']['image'], images['computer'])
            self.assertEqual(config['kubernetes_config']['services']['user-app']['image'], images['runtime'])
            role = next(r for r in rows if r['kind'] == 'ClusterRole')
            self.assertTrue(all(set(r['verbs']) <= {'get', 'list', 'watch'} for r in role['rules']))
            namespaced = next(r for r in rows if r['kind'] == 'Role')
            # Event publisher（kube-runtime 批次 C）：events.k8s.io 仅 create/patch
            events = next(r for r in namespaced['rules'] if r['apiGroups'] == ['events.k8s.io'])
            self.assertEqual(events['resources'], ['events'])
            self.assertEqual(sorted(events['verbs']), ['create', 'patch'])
            binding = next(r for r in rows if r['kind'] == 'RoleBinding')
            self.assertEqual(binding['roleRef']['name'], namespaced['metadata']['name'])
            self.assertTrue(all(s['kind'] == 'ServiceAccount' and s['name'] == 'rcoder' for s in binding['subjects']))

    def test_foreign_resource_rejected_before_mutation(self):
        with tempfile.TemporaryDirectory() as temp:
            c = self.config(temp)
            with patch.object(c, 'kube', return_value=json.dumps({'metadata': {'labels': {LABEL: 'someone-else'}}})) as kube:
                with self.assertRaisesRegex(RuntimeError, 'foreign'):
                    main.apply(c, {'kind': 'Namespace', 'metadata': {'name': c.ns}})
                self.assertEqual(kube.call_count, 1)

    def test_namespace_and_ssh_injection_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            self.config(temp)
            file = Path(temp) / 'config'
            for setting in ['REMOTE_K8S_NAMESPACE=default', 'REMOTE_K8S_SSH=-oProxyCommand=bad', 'REMOTE_K8S_DIR=/tmp/../etc']:
                original = file.read_text()
                file.write_text(original + setting + '\n')
                with patch.dict(os.environ, {}, clear=True), self.assertRaises(ValueError):
                    Config(file)
                file.write_text(original)

    def test_snapshot_hash_permissions_links_and_extras(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            live = root / 'live'
            live.mkdir()
            f = live / 'hello.rs'
            f.write_text('hello')
            f.chmod(0o755)
            (live / 'link').symlink_to('hello.rs')
            (live / 'stale.rs').write_text('never in build')
            import hashlib
            manifest = {'hello.rs': {'kind': 'file', 'sha256': hashlib.sha256(b'hello').hexdigest(), 'executable': True},
                        'link': {'kind': 'link', 'target': 'hello.rs'}}
            result = subprocess.run([sys.executable, '-c', snapshot.REMOTE_SNAPSHOT, temp, 'one'], input=json.dumps(manifest), text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((root / 'snapshots/one/stale.rs').exists())
            f.write_text('changed after snapshot')
            self.assertEqual((root / 'snapshots/one/hello.rs').read_text(), 'hello')
            result = subprocess.run([sys.executable, '-c', snapshot.REMOTE_SNAPSHOT, temp, 'two'], input=json.dumps(manifest), text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((root / 'snapshots/two').exists())

    def test_snapshot_rejects_symlink_ancestor(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'live').mkdir()
            (root / 'outside').mkdir()
            (root / 'outside/file').write_text('secret')
            (root / 'live/escape').symlink_to(root / 'outside')
            expected = {'escape/file': {'kind': 'file', 'sha256': 'bad', 'executable': False}}
            result = subprocess.run([sys.executable, '-c', snapshot.REMOTE_SNAPSHOT, temp, 'bad'], input=json.dumps(expected), text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('Symlink ancestor', result.stderr)

    def test_credentials_excluded_even_if_git_tracked(self):
        for path in ['.env.local', 'nested/.env.production', 'private.key', '.kube/config', '.remote-k8s/state.json', 'target/foo', 'tests-e2e/reports/a']:
            self.assertTrue(snapshot.excluded(path), path)
        self.assertFalse(snapshot.excluded('crates/rcoder/src/main.rs'))
        self.assertFalse(snapshot.excluded('crates/rcoder/src/cleanup_task/logs/mod.rs'))
        self.assertTrue(snapshot.excluded('logs/runtime.json'))

    def test_tracked_ignored_outputs_are_not_build_inputs(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            subprocess.run(['git', 'init', '-q', temp], check=True)
            for name, text in [('Cargo.toml', '[workspace]'), ('Cargo.lock', 'version = 4'),
                               ('.gitignore', 'generated.bin\n'), ('generated.bin', 'old generated binary'),
                               ('.env.local', 'FAKE_KEY=never-sync')]:
                (root / name).write_text(text)
            subprocess.run(['git', '-C', temp, 'add', '-f', '.'], check=True)
            with patch.object(snapshot, 'ROOT', root):
                rows = snapshot.manifest()
            self.assertIn('Cargo.lock', rows)
            self.assertNotIn('generated.bin', rows)
            self.assertNotIn('.env.local', rows)

    def test_private_pull_credentials_stay_in_namespace_secret(self):
        with tempfile.TemporaryDirectory() as temp:
            c = self.config(temp)
            images = {x: 'example/' + x + '@sha256:' + 'a' * 64 for x in ['rcoder', 'computer', 'runtime']}
            auth = {'auths': {'example': {'auth': 'FAKE_TEST_ONLY'}}}
            rows = manifests.render(c, images, 'test-only', auth)
            for row in rows:
                if 'FAKE_TEST_ONLY' in json.dumps(row):
                    self.assertEqual((row['kind'], row['metadata']['name']), ('Secret', 'registry'))
                    self.assertEqual(row['metadata']['namespace'], c.ns)
            accounts = [r for r in rows if r['kind'] == 'ServiceAccount']
            self.assertEqual({r['metadata']['name'] for r in accounts}, {'default', 'rcoder', 'rcoder-pods-sa'})
            self.assertTrue(all(r['imagePullSecrets'] == [{'name': 'registry'}] for r in accounts))

    def test_deployment_changed_blocks_tests(self):
        with tempfile.TemporaryDirectory() as temp:
            c = self.config(temp)
            with patch.object(main, 'owned', return_value={'metadata': {'uid': 'new', 'generation': 2}}):
                with self.assertRaisesRegex(RuntimeError, 'changed'):
                    main.identity(c, {'deployment_uid': 'old', 'generation': 1})


if __name__ == '__main__':
    unittest.main()
