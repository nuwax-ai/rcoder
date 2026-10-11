"""Validate official-source release recipe contracts; not native acceptance."""
import json
from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = yaml.safe_load((ROOT / '.github/workflows/release-app-cli.yml').read_text())


class PingapReleaseWorkflowTests(unittest.TestCase):
    def step(self, job, name):
        return next(step for step in WORKFLOW['jobs'][job]['steps'] if step.get('name') == name)

    def test_source_jobs_compile_exact_unmodified_upstream(self):
        for job in ('build-pingap-windows', 'build-pingap-unix'):
            with self.subTest(job=job):
                steps = WORKFLOW['jobs'][job]['steps']
                upstream = next(step for step in steps if step.get('with', {}).get('repository') == 'vicanso/pingap')
                self.assertEqual(upstream['with']['path'], '.pingap-source')
                self.assertEqual(upstream['with']['ref'], '${{ env.PINGAP_REV }}')
                building = self.step(job, 'Build pingap (tls-rustls)')['run']
                self.assertIn('pingap_official.py --repo-root . build --source .pingap-source --tls rustls', building)
                self.assertNotIn('already-applied', building)
                self.assertNotIn('apply.py', building)

    def test_linux_npm_preserves_glibc_and_tls_support_from_unmodified_source(self):
        script = self.step('build-pingap-unix', 'Build pingap (tls-rustls)')['run']
        self.assertIn('--zig-glibc 2.17', script)
        self.assertIn('--tls rustls', script)
        self.assertNotIn('install --download', script)
        abi = self.step('build-pingap-unix', 'Verify Linux Pingap glibc baseline')['run']
        self.assertIn('max(required) > (2, 17)', abi)

    def test_no_private_protocol_or_patch_can_block_or_enter_publication(self):
        self.assertNotIn('gate-pingap-applied', WORKFLOW['jobs'])
        text = (ROOT / '.github/workflows/release-app-cli.yml').read_text()
        for private in ('--apply-protocol-version', 'pingap-applied/', '.applied.json', 'apply-status'):
            self.assertNotIn(private, text)

    def test_all_cargo_release_builds_are_locked(self):
        for job in ('build', 'build-pingap-windows', 'build-pingap-unix'):
            for step in WORKFLOW['jobs'][job]['steps']:
                for line in step.get('run', '').splitlines():
                    if line.strip().startswith(('cargo build ', 'cargo zigbuild ')):
                        self.assertIn('--locked', line)

    def test_publish_requires_native_configuration_pair_gates(self):
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
        for job in ('pair-gate-windows', 'pair-gate-unix'):
            native = self.step(job, 'Real native owner startup Stop and Start')['run']
            self.assertIn('tools/build/verify_official_pair.py', native)
            self.assertIn('--app-cli dist/app-cli-', native)
            self.assertIn('--pingap dist/pingap', native)
            self.assertNotIn('|| true', native)
            uploads = [step for step in WORKFLOW['jobs'][job]['steps'] if step.get('uses', '').startswith('actions/upload-artifact@')]
            self.assertTrue(uploads)
            self.assertTrue(all('official-pair-gate/state' not in step['with']['path'] for step in uploads))

    def test_build_receipts_distribute_official_identity_without_runtime_requirement(self):
        for name in ('app-cli-linux-x64', 'app-cli-darwin-x64', 'app-cli-darwin-arm64', 'app-cli-windows-x64'):
            data = json.loads((ROOT / 'npm' / name / 'package.json').read_text())
            receipt = 'pingap.exe.official.json' if name.endswith('windows-x64') else 'pingap.official.json'
            self.assertIn(receipt, data['files'])
            self.assertFalse(any('applied' in item for item in data['files']))
        scripts = '\n'.join(step.get('run', '') for step in WORKFLOW['jobs']['publish-npm']['steps'])
        self.assertIn('dist/pingap.exe.official.json', scripts)
        self.assertIn('dist/pingap-x86_64-unknown-linux-gnu.official.json', scripts)

    def test_smoke_version_comparison_is_exact(self):
        for job in ('build-pingap-windows', 'build-pingap-unix'):
            script = self.step(job, 'Smoke test pingap -V')['run']
            self.assertIn('[ "$OUT" = "pingap $PINGAP_VERSION_EXPECTED" ]', script)
            self.assertNotIn('strings ', script)
            self.assertNotIn('|| true', script)


if __name__ == '__main__':
    unittest.main()
