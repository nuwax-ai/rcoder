"""Exercise paired patch integrity on isolated real Git source fixtures."""
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('paired_pingap_apply', ROOT / 'tools/build/pingap-applied/apply.py')
paired = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paired)


def sha(data):
    return hashlib.sha256(data).hexdigest()


class PairedPingapBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='pingap-paired-source-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        self.bundle = self.root / 'bundle'
        self.bundle.mkdir()
        self.git('init', '-q')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('config', 'user.name', 'Protocol fixture')
        (self.source / 'Cargo.toml').write_text('[package]\nname="pingap"\nversion="0.15.0"\n')
        (self.source / 'README.md').write_text('base\n')
        self.git('add', 'Cargo.toml', 'README.md')
        self.git('commit', '-q', '-m', 'Fixture base')
        commit = self.git('rev-parse', 'HEAD').stdout.strip()
        (self.source / 'README.md').write_text('patched\n')
        patch = self.git('diff', '--binary', 'HEAD').stdout.encode()
        (self.source / 'README.md').write_text('base\n')
        added = self.root / 'new-module.rs'
        added.write_text('// applied protocol module\n')
        # Generate a real new-file diff with relative paths; no index mutation.
        result = subprocess.run(['git', 'diff', '--no-index', '--', '/dev/null', 'new-module.rs'], cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 1)
        patch += result.stdout
        (self.bundle / 'applied-reload.patch').write_bytes(patch)
        self.manifest = {'protocol': 'rcoder-pingap-applied-source-v1', 'apply_protocol_version': 1,
                         'repository': 'https://github.com/vicanso/pingap', 'base_commit': commit,
                         'version': '0.15.0', 'patch_file': 'applied-reload.patch', 'patch_sha256': sha(patch),
                         'patched_files': {'README.md': sha(b'patched\n'), 'new-module.rs': sha(added.read_bytes())},
                         'source_files': {'Cargo.toml': sha((self.source / 'Cargo.toml').read_bytes()),
                                          'README.md': sha(b'patched\n'), 'new-module.rs': sha(added.read_bytes())}}
        self.write_manifest()

    def git(self, *args):
        return subprocess.run(['git', *args], cwd=self.source, text=True, capture_output=True, check=True)

    def write_manifest(self):
        (self.bundle / 'manifest.json').write_text(json.dumps(self.manifest))

    def test_exact_pin_patch_includes_new_module_and_is_verified(self):
        paired.apply(self.source, self.bundle)
        self.assertEqual((self.source / 'README.md').read_text(), 'patched\n')
        self.assertEqual((self.source / 'new-module.rs').read_text(), '// applied protocol module\n')
        paired.apply(self.source, self.bundle, verify_only=True)
        (self.source / 'Cargo.toml').write_text('tampered base file\n')
        with self.assertRaisesRegex(ValueError, 'SHA256 mismatch'):
            paired.apply(self.source, self.bundle, verify_only=True)

    def test_patch_byte_tamper_fails_before_any_source_publication(self):
        with (self.bundle / 'applied-reload.patch').open('ab') as stream:
            stream.write(b'\ncorrupt')
        with self.assertRaisesRegex(ValueError, 'patch SHA256 mismatch'):
            paired.apply(self.source, self.bundle)
        self.assertEqual((self.source / 'README.md').read_text(), 'base\n')
        self.assertFalse((self.source / 'new-module.rs').exists())

    def test_wrong_pin_and_dirty_tree_fail_before_patch(self):
        self.manifest['base_commit'] = 'a' * 40
        self.write_manifest()
        with self.assertRaisesRegex(ValueError, 'exact base commit'):
            paired.apply(self.source, self.bundle)
        self.manifest['base_commit'] = self.git('rev-parse', 'HEAD').stdout.strip()
        self.write_manifest()
        (self.source / 'README.md').write_text('unrelated work\n')
        with self.assertRaisesRegex(ValueError, 'must be clean'):
            paired.apply(self.source, self.bundle)
        self.assertEqual((self.source / 'README.md').read_text(), 'unrelated work\n')

    def test_frozen_export_requires_manifest_and_exact_source_file_hashes(self):
        paired.apply(self.source, self.bundle)
        exported = self.root / 'exported'
        shutil.copytree(self.source, exported, ignore=shutil.ignore_patterns('.git'))
        with self.assertRaisesRegex(ValueError, 'verified frozen source export'):
            paired.apply(exported, self.bundle, verify_only=True)
        (exported / 'SOURCE_MANIFEST.json').write_text(json.dumps({'base_commit': self.manifest['base_commit'], 'patch_sha256': self.manifest['patch_sha256']}))
        paired.apply(exported, self.bundle, verify_only=True)
        (exported / 'new-module.rs').write_text('tampered\n')
        with self.assertRaisesRegex(ValueError, 'SHA256 mismatch'):
            paired.apply(exported, self.bundle, verify_only=True)


if __name__ == '__main__':
    unittest.main()
