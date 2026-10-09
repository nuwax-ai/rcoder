import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import Config
import main
import manifests
import pingap_runtime_identity as identity


class PingapRuntimeIdentityTests(unittest.TestCase):
    def config(self, root):
        file = root / 'config'
        file.write_text('REMOTE_K8S_SSH=test-host\nREMOTE_K8S_DIR=/tmp/isolated/project\nREMOTE_K8S_CONTEXT=test\n')
        with patch.dict(os.environ, {}, clear=True):
            config = Config(file)
        config.state = root / 'state'
        config.values.update({'REMOTE_K8S_REGISTRY': 'example/test',
                              **{'REMOTE_K8S_' + key: 'example/' + key.lower() + ':base'
                                 for key in ('RCODER_BASE', *identity.BASES)}})
        return config

    def observations(self, config, bases, pingap=None, node='v22.23.2'):
        source = identity.source_identity()
        def ssh(args, **kwargs):
            self.assertEqual(args[:3], ['docker', 'run', '--rm'])
            self.assertIn('--network', args)
            self.assertIn('--pull=always', args)
            self.assertEqual(args[-2], bases['COMPUTER_BASE'] if 'computer_base' in args[-2] else bases['RUNTIME_BASE'])
            if args[args.index('--entrypoint') + 1] == 'node':
                return node
            if args[-1] == '--version':
                return 'pingap ' + source['version'] + ' (' + source['commit'] + ', tls=openssl)'
            return pingap or 'pingap ' + source['version']
        with patch.object(config, 'ssh', side_effect=ssh) as calls:
            receipt = identity.inspect_bases(config, bases, source)
        self.assertEqual(calls.call_count, 6)
        return receipt

    def test_each_base_digest_is_actually_executed_for_pingap_and_node(self):
        with tempfile.TemporaryDirectory() as temporary:
            c = self.config(Path(temporary))
            bases = {key: 'example/' + key.lower() + '@sha256:' + 'a' * 64 for key in identity.BASES}
            receipt = self.observations(c, bases)
            self.assertEqual(identity.validate_receipt(receipt, bases), identity.source_identity())
            for key, record in receipt['bases'].items():
                self.assertEqual(record['image'], bases[key])
                self.assertEqual(record['commands']['pingap'][-2:], [bases[key], '-V'])

    def test_old_pingap_node24_and_wrong_prefix_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            c = self.config(Path(temporary))
            bases = {key: 'example/' + key.lower() + '@sha256:' + 'a' * 64 for key in identity.BASES}
            for pingap, node in [('pingap 0.14.3', 'v22.23.2'), ('pingap 0.15.0', 'v24.0.0'), ('pingap 0.15.00', 'v22.23.2')]:
                with self.subTest(pingap=pingap, node=node), self.assertRaises(ValueError):
                    self.observations(c, bases, pingap, node)

    def test_override_cannot_forge_actual_build_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            c = self.config(Path(temporary))
            for key, value in [('PINGAP_VERSION', '0.14.3'), ('PINGAP_COMMIT', '0' * 40), ('NODE_VERSION', '24.0.0')]:
                with self.subTest(key=key):
                    c.values['REMOTE_K8S_' + key] = value
                    with self.assertRaisesRegex(ValueError, 'override'):
                        identity.check_overrides(c, identity.source_identity())
                    del c.values['REMOTE_K8S_' + key]

    def test_receipt_without_real_observations_or_matching_digests_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            c = self.config(Path(temporary))
            bases = {key: 'example/' + key.lower() + '@sha256:' + 'a' * 64 for key in identity.BASES}
            receipt = self.observations(c, bases)
            for mutation in [lambda r: r.update(status='building'),
                             lambda r: r['bases']['RUNTIME_BASE'].update(pingap_stdout='pingap 0.14.3'),
                             lambda r: r['bases']['COMPUTER_BASE'].update(node_stdout='v24.0.0'),
                             lambda r: r['bases']['COMPUTER_BASE'].update(pingap_full_stdout='pingap 0.15.0 (' + '0' * 40 + ', tls=openssl)'),
                             lambda r: r['bases']['COMPUTER_BASE'].update(image='example/base@sha256:' + 'b' * 64)]:
                altered = json.loads(json.dumps(receipt))
                mutation(altered)
                with self.assertRaises(ValueError):
                    identity.validate_receipt(altered, bases)
            with self.assertRaises(ValueError):
                identity.validate_receipt({'status': 'verified', 'source': identity.source_identity()})

    def test_manifests_require_receipt_and_use_observed_source_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            c = self.config(Path(temporary))
            bases = {key: 'example/' + key.lower() + '@sha256:' + 'a' * 64 for key in identity.BASES}
            images = {key: 'example/' + key + '@sha256:' + 'b' * 64 for key in ('rcoder', 'computer', 'runtime')}
            with self.assertRaisesRegex(ValueError, 'verified Pingap'):
                manifests.render(c, images, 'fixture')
            receipt = self.observations(c, bases)
            rows = manifests.render(c, images, 'fixture', pingap_identity=receipt)
            deployment = next(row for row in rows if row['kind'] == 'Deployment')
            env = {row['name']: row.get('value') for row in deployment['spec']['template']['spec']['containers'][0]['env']}
            self.assertEqual(env['RCODER_PINGAP_VERSION'], receipt['source']['version'])
            self.assertEqual(env['RCODER_PINGAP_COMMIT'], receipt['source']['commit'])

    def test_wrong_base_fails_before_image_build_and_records_failed_stage(self):
        with tempfile.TemporaryDirectory() as temporary:
            c = self.config(Path(temporary))
            def ssh(args, **kwargs):
                if args[:3] == ['docker', 'buildx', 'ls']:
                    return 'rcoder-' + c.id
                if args[:4] == ['docker', 'buildx', 'imagetools', 'inspect']:
                    return 'Digest: sha256:' + 'b' * 64
                if args[0] == 'python3':
                    return json.dumps(identity.source_identity())
                if args[:2] == ['docker', 'run']:
                    return 'pingap 0.14.3' if args[args.index('--entrypoint') + 1] == 'pingap' else 'v22.23.2'
                self.fail('unexpected command: ' + repr(args))
            frozen = {'path': '/tmp/isolated/project/snapshots/fixture', 'source_sha256': 'a' * 64, 'manifest': {}}
            with patch.object(c, 'ssh', side_effect=ssh), patch.object(main, 'doctor'), patch.object(main, 'sync_start'), \
                    patch.object(main.snapshot, 'create', return_value=frozen), patch.object(main.remote_process, 'execute') as execute:
                with self.assertRaisesRegex(ValueError, 'Pingap 0.14.3'):
                    main.build(c)
            execute.assert_not_called()
            receipts = [json.loads(path.read_text()) for path in (c.state / 'builds').glob('*.json') if not path.name.endswith('-source.json')]
            self.assertEqual(len(receipts), 1)
            self.assertEqual(receipts[0]['status'], 'failed')
            self.assertEqual(receipts[0]['failed_stage'], 'pingap-base-preflight')
            self.assertFalse((c.state / 'build.json').exists())

    def test_bad_deployment_receipt_blocks_kubernetes_reads_and_mutations(self):
        with tempfile.TemporaryDirectory() as temporary:
            c = self.config(Path(temporary))
            receipt = {'status': 'built', 'images': {key: 'example/' + key for key in ('rcoder', 'computer', 'runtime')},
                       'environment': c.id}
            c.state.mkdir()
            (c.state / 'build.json').write_text(json.dumps(receipt))
            with patch.object(c, 'kube') as kube, patch.object(main, 'outside_inventory') as inventory:
                with self.assertRaisesRegex(ValueError, 'verified Pingap'):
                    main.deploy(c)
            kube.assert_not_called()
            inventory.assert_not_called()


if __name__ == '__main__':
    unittest.main()
