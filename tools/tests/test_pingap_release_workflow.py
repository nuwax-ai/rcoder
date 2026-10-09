import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = yaml.safe_load((ROOT / '.github/workflows/release-app-cli.yml').read_text())


class PingapReleaseWorkflowTests(unittest.TestCase):
    def step(self, job, name):
        return next(step for step in WORKFLOW['jobs'][job]['steps'] if step.get('name') == name)

    def prepare(self, job, version='0.15.0', actual_commit=None):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        (root / 'Cargo.toml').write_text('[package]\nname="pingap"\nversion="' + version + '"\n')
        binary = root / 'bin'
        binary.mkdir()
        (binary / 'python').symlink_to(sys.executable)
        (binary / 'git').write_text('#!/bin/sh\nif [ "$*" != "rev-parse HEAD" ]; then exit 9; fi\nprintf "%s\\n" "$FIXTURE_COMMIT"\n')
        (binary / 'git').chmod(0o755)
        environment = dict(os.environ, PATH=str(binary) + os.pathsep + os.environ['PATH'],
                           PINGAP_REV='a' * 40, FIXTURE_COMMIT=actual_commit or 'a' * 40,
                           PINGAP_VERSION_EXPECTED='0.15.0')
        result = subprocess.run(['bash', '-c', self.step(job, 'Verify source and prepare JSON-only admin resources')['run']],
                                cwd=root, env=environment, capture_output=True, text=True, timeout=10)
        return root, result

    def test_both_checkout_jobs_prepare_absent_dist_without_node(self):
        for job in ('build-pingap-windows', 'build-pingap-unix'):
            with self.subTest(job=job):
                root, result = self.prepare(job)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn('admin JSON API', (root / 'dist/README.md').read_text())
                step = self.step(job, 'Verify source and prepare JSON-only admin resources')['run']
                self.assertNotIn('npm', step)
                self.assertNotIn('node ', step)

    def test_wrong_source_version_or_commit_fails_before_placeholder_publication(self):
        for job in ('build-pingap-windows', 'build-pingap-unix'):
            for version, commit in [('0.14.3', 'a' * 40), ('0.15.0', 'b' * 40)]:
                with self.subTest(job=job, version=version, commit=commit):
                    root, result = self.prepare(job, version, commit)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse((root / 'dist').exists())

    def test_all_cargo_release_builds_are_locked(self):
        for job in ('build', 'build-pingap-windows', 'build-pingap-unix'):
            for step in WORKFLOW['jobs'][job]['steps']:
                for line in step.get('run', '').splitlines():
                    if line.strip().startswith(('cargo build ', 'cargo zigbuild ')):
                        self.assertIn('--locked', line)

    def test_publish_and_release_require_every_distributed_platform_pair_gate(self):
        for job in ('publish-npm', 'release'):
            self.assertIn('pair-gate-windows', WORKFLOW['jobs'][job]['needs'])
            self.assertIn('pair-gate-unix', WORKFLOW['jobs'][job]['needs'])
        targets = {entry['target'] for entry in WORKFLOW['jobs']['pair-gate-unix']['strategy']['matrix']['include']}
        self.assertEqual(targets, {'x86_64-apple-darwin', 'aarch64-apple-darwin', 'x86_64-unknown-linux-gnu'})
        for job, step in [('pair-gate-windows', 'Gen lock with fixture and validate via pingap -t'),
                          ('pair-gate-unix', 'Validate the distributed Unix pair')]:
            script = self.step(job, step)['run']
            self.assertIn('parse_pingap_version', script)
            self.assertIn('[ -s pingap.toml ]', script)
            self.assertIn('"$PINGAP" -c pingap.toml -t', script)

    def test_smoke_version_comparison_is_exact_and_never_uses_strings_fallback(self):
        for job in ('build-pingap-windows', 'build-pingap-unix'):
            script = self.step(job, 'Smoke test pingap -V')['run']
            self.assertIn('[ "$OUT" = "pingap $PINGAP_VERSION_EXPECTED" ]', script)
            self.assertNotIn('strings ', script)
            self.assertNotIn('|| true', script)


if __name__ == '__main__':
    unittest.main()
