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

    def test_unlisted_release_falls_back_and_records_the_downloaded_digest(self):
        catalog = self.root / 'narrow-catalog.json'
        catalog.write_text(json.dumps({'protocol': 'pingap-release-assets-v1',
                                       'repository': 'vicanso/pingap', 'releases': {}}))
        elf = bytearray(64)
        elf[:6] = b'\x7fELF\x02\x01'
        elf[18:20] = (62).to_bytes(2, 'little')
        archive = self.root / 'fallback.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            item = tarfile.TarInfo('pingap-linux-gnu-x86-full')
            item.size = len(elf)
            tar.addfile(item, io.BytesIO(bytes(elf)))
        output = self.root / 'installed-fallback'
        args = argparse.Namespace(arch='amd64', catalog=catalog, archive=archive,
                                  download=False, output=output)
        with patch.object(official, 'native_probe'):
            official.install(args, '9.9.9', 'b' * 40)
        receipt = json.loads((self.root / 'installed-fallback.official.json').read_text())
        self.assertFalse(receipt['catalog_verified'])
        self.assertEqual(receipt['asset_sha256'], official.digest(archive))
        self.assertTrue(output.exists())

    def test_requested_identity_diverging_from_receipt_warns_and_verifies_installed(self):
        # Image builds pass the platform's selected version while the installed
        # binary may lag one release until rcoder converges. The installed
        # identity is still fully verified (receipt digest + native probe).
        with patch.object(official, 'warn') as warned, patch.object(official.platform, 'machine', return_value='x86_64'):
            official.verify(self.args, '0.16.2', 'c' * 40)
        self.assertEqual(warned.call_count, 1)

    def test_unverified_catalog_receipt_skips_catalog_cross_check(self):
        value = json.loads(self.receipt.read_text())
        value['catalog_verified'] = False
        value['asset_sha256'] = 'a' * 64
        self.receipt.write_text(json.dumps(value))
        with patch.object(official.platform, 'machine', return_value='x86_64'):
            official.verify(self.args, VERSION, COMMIT)
        self.binary.write_text(self.binary.read_text() + '\n# changed\n')
        with self.assertRaisesRegex(ValueError, 'final binary'):
            self.verify()

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

    def test_official_workspace_inherited_package_version_is_accepted(self):
        # The exact official v0.15.0 source inherits package.version. Validate
        # that source without editing its Cargo.toml to make the gate pass.
        (self.root / 'Cargo.toml').write_text(
            '[package]\nname="pingap"\nversion.workspace=true\n'
            '[workspace.package]\nversion="0.15.0"\n')
        def git(command, source=None, capture=False):
            self.assertEqual(command[0], 'git')
            return subprocess.CompletedProcess(command, 0,
                COMMIT + '\n' if command[1] == 'rev-parse' else '')
        with patch.object(official, 'checked', side_effect=git):
            official.unchanged_source(self.root, VERSION, COMMIT)

    def test_inherited_version_does_not_bypass_exact_pin_or_version_checks(self):
        cases = [
            '[package]\nversion.workspace=true\n[workspace.package]\nversion="0.14.0"\n',
            '[package]\nversion.workspace=true\n',
            '[package]\nversion.workspace=false\n[workspace.package]\nversion="0.15.0"\n',
            '[package]\nversion.workspace=1\n[workspace.package]\nversion="0.15.0"\n',
        ]
        for manifest in cases:
            with self.subTest(manifest=manifest):
                (self.root / 'Cargo.toml').write_text(manifest)
                def git(command, source=None, capture=False):
                    return subprocess.CompletedProcess(command, 0,
                        COMMIT + '\n' if command[1] == 'rev-parse' else '')
                with patch.object(official, 'checked', side_effect=git):
                    with self.assertRaisesRegex(ValueError, 'package version mismatch'):
                        official.unchanged_source(self.root, VERSION, COMMIT)
        (self.root / 'Cargo.toml').write_text(
            '[package]\nversion.workspace=true\n[workspace.package]\nversion="0.15.0"\n')
        with patch.object(official, 'checked', return_value=subprocess.CompletedProcess(
                ['git', 'rev-parse', 'HEAD'], 0, 'a' * 40 + '\n')):
            with self.assertRaisesRegex(ValueError, 'exact pin'):
                official.unchanged_source(self.root, VERSION, COMMIT)

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

class RequireCatalogCommitMatchTests(unittest.TestCase):
    """复审第五轮 P2: 回退前预核验版本身份——缺架构资产不得跳过 pin 冲突。"""

    @staticmethod
    def _catalog(commit, with_asset):
        assets = {'tag': 'v9.9.9', 'commit': commit}
        if with_asset:
            assets['assets'] = {
                'amd64': {'name': 'pingap-linux-gnu-x86-full.tar.gz', 'id': 1,
                          'sha256': 'a' * 64, 'url': 'https://example/asset'}
            }
        else:
            assets['assets'] = {}
        return {'protocol': 'pingap-release-assets-v1', 'repository': 'vicanso/pingap',
                'releases': {'9.9.9': assets}}

    def test_same_pin_with_missing_asset_falls_back(self):
        import tools.build.pingap_official as po
        catalog = self._catalog('b' * 40, with_asset=False)
        # 同 pin：不应抛错（回退路径由调用方继续）
        po._require_catalog_commit_match(catalog, '9.9.9', 'b' * 40)

    def test_wrong_pin_with_missing_asset_refused(self):
        import tools.build.pingap_official as po
        catalog = self._catalog('b' * 40, with_asset=False)
        with self.assertRaises(ValueError):
            po._require_catalog_commit_match(catalog, '9.9.9', 'c' * 40)

    def test_missing_version_entry_passes(self):
        import tools.build.pingap_official as po
        catalog = {'protocol': 'pingap-release-assets-v1', 'repository': 'vicanso/pingap',
                   'releases': {}}
        po._require_catalog_commit_match(catalog, '8.8.8', 'd' * 40)
