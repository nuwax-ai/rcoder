"""Execute the real Rust build script without .git, as frozen Docker builds do."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class FrozenBuildIdentityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory(prefix='rcoder-build-identity-')
        cls.binary = Path(cls.directory.name) / 'build-script'
        subprocess.run(['rustc', '--edition=2024', str(ROOT / 'crates/rcoder/build.rs'),
                        '-o', str(cls.binary)], check=True, capture_output=True)

    @classmethod
    def tearDownClass(cls):
        cls.directory.cleanup()

    def execute(self, **values):
        environment = dict(os.environ)
        for name in ['GIT_DIR', 'GIT_WORK_TREE', 'RCODER_SOURCE_COMMIT', 'RCODER_SOURCE_BRANCH']:
            environment.pop(name, None)
        environment.update(values)
        return subprocess.run([str(self.binary)], cwd=self.directory.name,
                              env=environment, text=True, capture_output=True)

    def test_frozen_source_identity_survives_without_git_directory(self):
        commit = 'a' * 40
        result = self.execute(RCODER_SOURCE_COMMIT=commit, RCODER_SOURCE_BRANCH='frozen')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('RCODER_BUILD_GIT_HASH=' + commit + '\n', result.stdout)
        self.assertIn('RCODER_BUILD_GIT_BRANCH=frozen\n', result.stdout)
        self.assertNotIn('+dirty', result.stdout)

    def test_invalid_explicit_commit_fails_before_injecting_identity(self):
        for commit in ['', 'short', 'z' * 40, 'a' * 40 + '\n']:
            with self.subTest(commit=commit):
                result = self.execute(RCODER_SOURCE_COMMIT=commit)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('RCODER_BUILD_GIT_HASH=', result.stdout)

    def test_missing_git_never_claims_an_empty_identity(self):
        result = self.execute()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('RCODER_BUILD_GIT_HASH=unknown\n', result.stdout)
        self.assertIn('RCODER_BUILD_GIT_BRANCH=unknown\n', result.stdout)


if __name__ == '__main__':
    unittest.main()
