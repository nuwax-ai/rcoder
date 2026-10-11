"""Controlled build tests; these do not certify a real Pingap/container."""
import argparse
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('pingap_official', ROOT / 'tools/build/pingap_official.py')
official = importlib.util.module_from_spec(spec)
spec.loader.exec_module(official)
VERSION = '0.15.0'
COMMIT = '8270a1ebb7a238ea86fa220215714613410378bb'


class OfficialBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / 'pingap'
        self.binary.write_text('#!/usr/bin/env python3\nimport sys\n'
            f'if sys.argv[1] == "-V": print("pingap {VERSION}")\n'
            f'elif sys.argv[1] == "--version": print("pingap {VERSION} ({COMMIT}, tls=openssl)")\n'
            'elif "-t" in sys.argv: pass\nelse: sys.exit(2)\n')
        self.binary.chmod(0o755)
        self.receipt = self.root / 'pingap.official.json'
        official.write_receipt(self.binary, dict(origin='official_release', version=VERSION,
            commit=COMMIT, tls_backend='openssl', architecture='amd64',
            asset_sha256='0539eeac37e9bf81307885983a4906e41e03aba1bf093b3c6bc5952e809e8334'))
        self.args = argparse.Namespace(binary=self.binary, receipt=self.receipt,
            catalog=ROOT / 'tools/build/pingap-assets.json')

    def verify(self):
        with patch.object(official.platform, 'machine', return_value='x86_64'):
            official.verify(self.args, VERSION, COMMIT)

    def test_official_binary_without_private_protocol_passes_native_checks(self):
        result = subprocess.run([str(self.binary), '--apply-protocol-version'], capture_output=True)
        self.assertEqual(result.returncode, 2)
        self.verify()

    def test_tampered_binary_and_self_signed_asset_receipt_fail(self):
        self.binary.write_text(self.binary.read_text() + '\n# changed\n')
        with self.assertRaisesRegex(ValueError, 'final binary'):
            self.verify()
        value = json.loads(self.receipt.read_text())
        value.update(binary_sha256=official.digest(self.binary), asset_sha256='a' * 64)
        self.receipt.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError, 'trusted catalog'):
            self.verify()

    def test_target_and_tls_cannot_disagree_with_official_full_asset(self):
        for key, value in [('architecture', 'arm64'), ('tls_backend', 'rustls')]:
            with self.subTest(key=key):
                record = json.loads(self.receipt.read_text())
                saved = dict(record)
                record[key] = value
                self.receipt.write_text(json.dumps(record))
                with self.assertRaises(ValueError):
                    self.verify()
                self.receipt.write_text(json.dumps(saved))

    def test_test_mode_failure_is_fatal(self):
        self.binary.write_text(self.binary.read_text().replace('elif "-t" in sys.argv: pass', 'elif "-t" in sys.argv: sys.exit(17)'))
        with self.assertRaises(subprocess.CalledProcessError) as result:
            official.native_probe(self.binary, VERSION, COMMIT, 'openssl')
        self.assertEqual(result.exception.returncode, 17)

    def test_install_checks_archive_sha_before_extracting_or_running(self):
        archive = self.root / 'fake.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            data = b'not the official binary'
            item = tarfile.TarInfo('pingap-linux-gnu-x86-full')
            item.size = len(data)
            tar.addfile(item, io.BytesIO(data))
        output = self.root / 'installed'
        args = argparse.Namespace(arch='amd64', catalog=self.args.catalog, archive=archive,
                                  download=False, output=output)
        with self.assertRaisesRegex(ValueError, 'asset SHA256'):
            official.install(args, VERSION, COMMIT)
        self.assertFalse(output.exists())

    def test_modified_upstream_source_is_rejected_before_cargo(self):
        args = argparse.Namespace(source=self.root, output=self.root / 'out', fetch=False,
            tls='rustls', full=False, zig_glibc=None, target=None)
        calls = []
        def fake(command, source=None, capture=False):
            calls.append(command)
            return subprocess.CompletedProcess(command, 0,
                COMMIT + '\n' if command[1] == 'rev-parse' else ' M src/main.rs\n')
        with patch.object(official, 'checked', side_effect=fake):
            with self.assertRaisesRegex(ValueError, 'tracked modifications'):
                official.build(args, VERSION, COMMIT)
        self.assertFalse(any(command[0] == 'cargo' for command in calls))

    def test_source_build_is_locked_unpatched_and_records_cross_target_truthfully(self):
        (self.root / 'Cargo.toml').write_text('[package]\nversion="0.15.0"\n')
        target = 'x86_64-apple-darwin'
        binary = self.root / 'target' / target / 'release/pingap'
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b'cross-target-binary')
        args = argparse.Namespace(source=self.root, output=self.root / 'out', fetch=False,
            tls='rustls', full=False, zig_glibc=None, target=target)
        calls = []
        def fake(command, source=None, capture=False):
            calls.append(command)
            output = COMMIT + '\n' if command[:2] == ['git', 'rev-parse'] else ''
            if command[0] == 'rustc':
                output = 'host: aarch64-apple-darwin\n'
            return subprocess.CompletedProcess(command, 0, output)
        with patch.object(official, 'checked', side_effect=fake):
            official.build(args, VERSION, COMMIT)
        cargo = next(command for command in calls if command[0] == 'cargo')
        self.assertEqual(cargo, ['cargo', 'build', '--locked', '--release', '--no-default-features',
                                 '--features', 'tls-rustls', '--target', target])
        receipt = json.loads((self.root / 'out.official.json').read_text())
        self.assertFalse(receipt['executed_native_probe'])
        self.assertEqual(receipt['origin'], 'unchanged_official_source')
        self.assertNotIn('patch_sha256', receipt)


if __name__ == '__main__':
    unittest.main()
