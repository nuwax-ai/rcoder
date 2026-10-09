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

    def test_both_checkout_jobs_apply_reviewed_patch_before_build(self):
        for job in ('build-pingap-windows', 'build-pingap-unix'):
            with self.subTest(job=job):
                steps = WORKFLOW['jobs'][job]['steps']
                upstream = next(step for step in steps if step.get('with', {}).get('repository') == 'vicanso/pingap')
                self.assertEqual(upstream['with']['path'], '.pingap-source')
                applying = self.step(job, 'Apply reviewed paired Pingap protocol patch')
                building = self.step(job, 'Build pingap (tls-rustls)')
                self.assertIn('apply.py --source .pingap-source --repo-root .', applying['run'])
                self.assertIn('--already-applied --tls rustls', building['run'])
                self.assertLess(steps.index(applying), steps.index(building))
                self.assertNotIn('npm', applying['run'])

    def test_paired_protocol_and_default_full_gates_block_publication(self):
        jobs = WORKFLOW['jobs']
        for job in ('pair-gate-windows', 'pair-gate-unix'):
            self.assertIn('gate-pingap-applied', jobs[job]['needs'])
        gate = jobs['gate-pingap-applied']
        self.assertEqual(gate['env']['RUSTUP_TOOLCHAIN'], '1.98.1')
        default = self.step('gate-pingap-applied', 'Default feature tests and strict Clippy')['run']
        full = self.step('gate-pingap-applied', 'Full feature tests and strict Clippy')['run']
        self.assertIn('--features serde_json/preserve_order', default)
        self.assertIn('cargo test --locked', default)
        self.assertIn('--features full', full)
        for script in (default, full):
            self.assertIn('--no-fail-fast', script)
            self.assertIn('--all-targets', script)
            self.assertIn('-- -D warnings', script)

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
            self.assertIn('--apply-protocol-version', script)
            self.assertIn('[ -s pingap.toml ]', script)
            self.assertIn('"$PINGAP" -c pingap.toml -t', script)

    def test_reviewed_patch_bytes_are_kept_lf_on_windows(self):
        attrs = (ROOT / 'tools/build/pingap-applied/.gitattributes').read_text()
        self.assertIn('* text eol=lf', attrs)
        script = self.step('build-pingap-windows', 'Disable CRLF conversion')['run']
        self.assertIn('core.autocrlf false', script)

    def test_paired_receipts_are_included_in_each_bundled_platform_package(self):
        import json
        for name in ('app-cli-linux-x64', 'app-cli-darwin-x64', 'app-cli-darwin-arm64', 'app-cli-windows-x64'):
            data = json.loads((ROOT / 'npm' / name / 'package.json').read_text())
            receipt = 'pingap.exe.applied.json' if name.endswith('windows-x64') else 'pingap.applied.json'
            self.assertIn(receipt, data['files'])
        scripts = '\n'.join(step.get('run', '') for step in WORKFLOW['jobs']['publish-npm']['steps'])
        self.assertIn('dist/pingap.exe.applied.json', scripts)
        self.assertIn('dist/pingap-x86_64-unknown-linux-gnu.applied.json', scripts)

    def test_smoke_version_comparison_is_exact_and_never_uses_strings_fallback(self):
        for job in ('build-pingap-windows', 'build-pingap-unix'):
            script = self.step(job, 'Smoke test pingap -V')['run']
            self.assertIn('[ "$OUT" = "pingap $PINGAP_VERSION_EXPECTED" ]', script)
            self.assertIn('--apply-protocol-version', script)
            self.assertNotIn('strings ', script)
            self.assertNotIn('|| true', script)


if __name__ == '__main__':
    unittest.main()
