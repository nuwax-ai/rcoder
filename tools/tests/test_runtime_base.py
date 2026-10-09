"""Check observed runtime binaries and immutable base selection before build."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('runtime_base', ROOT / 'tools/build/runtime_base.py')
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)
VERSION = '0.15.0'
COMMIT = '8270a1ebb7a238ea86fa220215714613410378bb'
IMAGE_ID = 'sha256:' + 'a' * 64


class RuntimeBaseTests(unittest.TestCase):
    def observer(self, overrides=None):
        values = {'id':IMAGE_ID, 'short':'pingap '+VERSION,
                  'long':f'pingap {VERSION} ({COMMIT}, tls=openssl)', 'node':'v22.23.2'}
        values.update(overrides or {})
        self.observations = []
        def output(arguments):
            self.observations.append(arguments)
            if arguments[:2] == ['image', 'inspect']:
                return values['id']
            if arguments[:1] == ['tag']:
                self.assertEqual(arguments[1], IMAGE_ID)
                return ''
            self.assertEqual(arguments[-2], IMAGE_ID, 'used mutable tag for observation')
            self.assertIn('none', arguments)
            binary = arguments[arguments.index('--entrypoint')+1]
            return values['node'] if binary.endswith('/node') else values['short'] if arguments[-1] == '-V' else values['long']
        return output

    def test_valid_short_and_long_version_are_both_observed_on_same_image_id(self):
        with patch.object(base, 'docker_output', self.observer()):
            receipt = base.verify_base('base:moving', VERSION, COMMIT, '22.23.2')
        self.assertEqual(receipt['image_id'], IMAGE_ID)
        self.assertEqual(len(self.observations), 4)
        self.assertEqual(self.observations[1][-1], '-V')
        self.assertEqual(self.observations[2][-1], '--version')

    def test_old_version_wrong_commit_tls_and_node_are_rejected(self):
        for override in [{'short':'pingap 0.14.3'}, {'short':'pingap 0.15.01'},
                         {'long':f'pingap {VERSION} ('+'0'*40+', tls=openssl)'},
                         {'long':f'pingap {VERSION} ({COMMIT}, tls=rustls)'}, {'node':'v24.0.0'}]:
            with self.subTest(override=override), patch.object(base, 'docker_output', self.observer(override)):
                with self.assertRaisesRegex(ValueError, 'mismatch'):
                    base.verify_base('base:moving', VERSION, COMMIT, '22.23.2')

    def test_base_tag_is_replaced_with_observed_immutable_id(self):
        command = ['docker', 'buildx', 'build', '--build-arg', 'BASE_IMAGE=base:moving', '{context}']
        with patch.object(base, 'docker_output', self.observer()):
            base.pin_base(command, VERSION, COMMIT, '22.23.2')
        self.assertIn('BASE_IMAGE=local/rcoder-verified-base:' + 'a'*64, command)
        self.assertNotIn('BASE_IMAGE=base:moving', command)

    def test_remote_multiarch_base_uses_one_registry_digest_and_observes_both_platforms(self):
        immutable = 'registry.test:5000/app-runtime-base@' + IMAGE_ID
        observations = []
        def output(arguments):
            observations.append(arguments)
            if arguments[:3] == ['buildx','imagetools','inspect']:
                return 'Name: registry.test:5000/app-runtime-base:tag\nDigest: '+IMAGE_ID+'\n'
            self.assertEqual(arguments[-2], immutable)
            binary = arguments[arguments.index('--entrypoint')+1]
            return 'v22.23.2' if binary.endswith('/node') else 'pingap '+VERSION if arguments[-1]=='-V' else f'pingap {VERSION} ({COMMIT}, tls=openssl)'
        command = ['docker','buildx','build','--platform','linux/amd64,linux/arm64','--build-arg','BASE_IMAGE=registry.test:5000/app-runtime-base:tag','{context}']
        with patch.object(base, 'docker_output', output):
            receipt = base.pin_base(command, VERSION, COMMIT, '22.23.2', registry=True)
        self.assertIn('BASE_IMAGE='+immutable, command)
        self.assertEqual([value['platform'] for value in receipt['platforms']], ['linux/amd64','linux/arm64'])
        self.assertEqual(len([args for args in observations if args[:1]==['run']]), 6)

    def test_missing_immutable_id_and_duplicate_base_selection_fail(self):
        with patch.object(base, 'docker_output', self.observer({'id':'base:moving'})):
            with self.assertRaisesRegex(ValueError, 'immutable'):
                base.verify_base('base:moving', VERSION, COMMIT, '22.23.2')
        with self.assertRaisesRegex(ValueError, 'one explicit'):
            base.pin_base(['--build-arg','BASE_IMAGE=a','--build-arg','BASE_IMAGE=b'], VERSION, COMMIT, '22.23.2')


if __name__ == '__main__':
    unittest.main()
