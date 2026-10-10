"""Final-image gate tests; executable fixtures are protocol-level substitutes."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'tools/build/pingap-applied/verify.py'
spec = importlib.util.spec_from_file_location('paired_pingap_verify', SCRIPT)
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class PingapFinalImageTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='pingap-image-gate-')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.binary = self.root / 'pingap'
        self.receipt_path = self.root / 'receipt.json'
        self.manifest_path = self.root / 'manifest.json'
        self.manifest = json.loads((ROOT / 'tools/build/pingap-applied/manifest.json').read_text())
        self.outputs = {
            '--apply-protocol-version': '1',
            '-V': 'pingap ' + self.manifest['version'],
            '--version': f"pingap {self.manifest['version']} ({self.manifest['base_commit']}, tls=openssl)",
        }
        self.write_binary()
        self.receipt = {
            'protocol': 'rcoder-pingap-applied-binary-v1', 'apply_protocol_version': 1,
            'upstream_repository': self.manifest['repository'],
            'upstream_base_commit': self.manifest['base_commit'],
            'upstream_version': self.manifest['version'],
            'patch_sha256': self.manifest['patch_sha256'],
            'binary_sha256': hashlib.sha256(self.binary.read_bytes()).hexdigest(),
            'target': gate.runtime_target(), 'tls_backend': 'openssl',
            'cargo_features': ['openssl', 'full'], 'executed_native_probe': False,
        }
        self.write_metadata()

    def write_binary(self, protocol_exit=0):
        # This fixture tests the verifier, not actual Pingap configuration apply.
        program = (f'#!{sys.executable}\nimport sys\noutputs={self.outputs!r}\n'
                   f"if sys.argv[1] == '--apply-protocol-version' and {protocol_exit}:\n"
                   f'    raise SystemExit({protocol_exit})\n'
                   "print(outputs[sys.argv[1]])\n")
        self.binary.write_text(program, encoding='utf-8')
        self.binary.chmod(0o755)

    def write_metadata(self):
        self.receipt_path.write_text(json.dumps(self.receipt), encoding='utf-8')
        self.manifest_path.write_text(json.dumps(self.manifest), encoding='utf-8')

    def verify(self):
        return gate.verify(self.binary, self.receipt_path, self.manifest_path,
                           version=self.manifest['version'], commit=self.manifest['base_commit'],
                           require_full=True, tls='openssl')

    def test_final_probe_executes_even_for_cross_compiled_receipt(self):
        result = self.verify()
        self.assertTrue(result['executed_final_image_probe'])
        self.assertEqual(result['binary_sha256'], self.receipt['binary_sha256'])

    def test_same_version_without_protocol_is_rejected(self):
        self.write_binary(protocol_exit=2)
        self.receipt['binary_sha256'] = hashlib.sha256(self.binary.read_bytes()).hexdigest()
        self.write_metadata()
        with self.assertRaisesRegex(ValueError, 'apply-protocol-version.*exit=2'):
            self.verify()

    def test_actual_protocol_commit_and_tls_outputs_are_required(self):
        for argument, output in [('--apply-protocol-version', '0'), ('-V', 'pingap 0.14.3'),
                                 ('--version', f"pingap {self.manifest['version']} ({'0' * 40}, tls=openssl)"),
                                 ('--version', f"pingap {self.manifest['version']} ({self.manifest['base_commit']}, tls=rustls)")]:
            with self.subTest(argument=argument, output=output):
                original = self.outputs[argument]
                self.outputs[argument] = output
                self.write_binary()
                self.receipt['binary_sha256'] = hashlib.sha256(self.binary.read_bytes()).hexdigest()
                self.write_metadata()
                with self.assertRaisesRegex(ValueError, 'failed final-image verification'):
                    self.verify()
                self.outputs[argument] = original

    def test_receipt_identity_architecture_and_features_cannot_be_self_declared(self):
        original = copy.deepcopy(self.receipt)
        for field, value in [('patch_sha256', '0' * 64), ('upstream_base_commit', '0' * 40),
                             ('apply_protocol_version', 0), ('protocol', 'official-asset'),
                             ('target', 'foreign-unknown-linux-gnu'), ('cargo_features', ['openssl']),
                             ('tls_backend', 'rustls')]:
            with self.subTest(field=field):
                self.receipt = copy.deepcopy(original)
                self.receipt[field] = value
                self.write_metadata()
                with self.assertRaises(ValueError):
                    self.verify()

    def test_binary_overwrite_missing_or_malformed_receipt_fails(self):
        self.binary.write_text('overwritten by official archive', encoding='utf-8')
        with self.assertRaisesRegex(ValueError, 'binary SHA256'):
            self.verify()
        self.receipt_path.write_text('[]', encoding='utf-8')
        with self.assertRaisesRegex(ValueError, 'JSON object'):
            self.verify()
        self.receipt_path.unlink()
        with self.assertRaises(FileNotFoundError):
            self.verify()

    def test_image_arguments_must_match_frozen_source(self):
        for arguments in [{'version': '0.14.3'}, {'commit': '0' * 40}]:
            with self.subTest(arguments=arguments), self.assertRaisesRegex(ValueError, 'paired source'):
                gate.verify(self.binary, self.receipt_path, self.manifest_path, **arguments)

    def test_cli_runs_standalone_without_source_checkout(self):
        standalone = self.root / 'verify.py'
        standalone.write_bytes(SCRIPT.read_bytes())
        command = [sys.executable, str(standalone), '--binary', str(self.binary),
                   '--receipt', str(self.receipt_path), '--manifest', str(self.manifest_path),
                   '--require-full', '--tls', 'openssl']
        result = subprocess.run(command, cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)['executed_final_image_probe'])
        self.receipt_path.unlink()
        failed = subprocess.run(command, cwd=self.root, capture_output=True, text=True)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(failed.stdout, '')


class DockerPairingRecipeTests(unittest.TestCase):
    def test_production_equivalent_recipes_gate_after_the_final_proxy_copy(self):
        for name in ('docker/app-runtime-base/Dockerfile.runtime', 'docker/rcoder-agent-runner/Dockerfile'):
            with self.subTest(name=name):
                text = (ROOT / name).read_text()
                copy = text.rindex('/usr/local/bin/pingap\n')
                check = text.rindex('RUN python3 /usr/local/share/rcoder/verify-pingap.py')
                self.assertLess(copy, check)
                for required in ('/usr/local/share/rcoder/pingap-applied.json',
                                 '/usr/local/share/rcoder/pingap-source.json', '--require-full --tls openssl'):
                    self.assertIn(required, text[copy:])

    def test_remote_hot_images_replace_pingap_alongside_app_cli(self):
        text = (ROOT / 'docker/remote-k8s/Dockerfile').read_text()
        self.assertIn('--fetch --source /build/pingap-source --repo-root /src --full', text)
        self.assertIn('id=rcoder-pingap-target-${TARGETARCH}', text)
        for target in ('computer', 'runtime'):
            with self.subTest(target=target):
                stage = text.split(' AS ' + target + '\n', 1)[1].split('\nFROM ', 1)[0]
                self.assertIn('/out/app-cli', stage)
                self.assertIn('--from=pingap-applied-build /out/pingap /usr/local/bin/pingap', stage)
                self.assertIn('--from=pingap-applied-build /out/pingap.applied.json', stage)
                self.assertIn('RUN python3 /usr/local/share/rcoder/verify-pingap.py', stage)
                self.assertIn('--require-full --tls openssl', stage)


if __name__ == '__main__':
    unittest.main()
