"""Exercise release identity with real Git; do not build or publish artifacts."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = yaml.safe_load(
    (ROOT / '.github/workflows/release-file-server-proxy.yml').read_text(encoding='utf-8')
)
TAG_REF = '${{ github.event.inputs.tag || github.ref }}'
TAG_NAME = '${{ github.event.inputs.tag || github.ref_name }}'


class ProxyReleaseIdentityTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='proxy release identity ')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.manifest = self.root / 'crates/file-server-proxy/Cargo.toml'
        self.manifest.parent.mkdir(parents=True)
        self.manifest.write_text('[package]\nname="file-server-proxy"\nversion = "1.2.3"\n', encoding='utf-8')
        self.git('init', '-q')
        self.commit('initial release source')
        self.git('tag', 'file-server-proxy-v1.2.3')

    def git(self, *args):
        return subprocess.run(
            ['git', *args], cwd=self.root, check=True,
            capture_output=True, text=True, timeout=10,
        )

    def commit(self, message):
        self.git('add', '.')
        self.git('-c', 'user.name=Release fixture', '-c', 'user.email=release@example.invalid',
                 'commit', '-qm', message)

    def meta(self, tag='file-server-proxy-v1.2.3', ref=None):
        step = next(item for item in WORKFLOW['jobs']['meta']['steps'] if item.get('id') == 'v')
        output = self.root / 'github-output'
        env = dict(os.environ, REQUESTED_TAG=tag,
                   GITHUB_REF=ref or 'refs/tags/' + tag,
                   GITHUB_REF_NAME=(ref or tag).removeprefix('refs/heads/').removeprefix('refs/tags/'),
                   GITHUB_OUTPUT=str(output))
        result = subprocess.run(
            ['bash', '-e', '-o', 'pipefail', '-c', step['run']],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=10,
        )
        values = dict(line.split('=', 1) for line in output.read_text(encoding='utf-8').splitlines()) if output.exists() else {}
        return result, values

    def test_matching_stable_tag_emits_exact_version_and_stable_channel(self):
        result, values = self.meta()
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(values, {'version': '1.2.3', 'prerelease': 'false'})

    def test_manual_requested_tag_uses_its_checkout_instead_of_branch_name(self):
        result, values = self.meta(ref='refs/heads/release-fixture')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(values, {'version': '1.2.3', 'prerelease': 'false'})

    def test_matching_prerelease_tag_selects_beta_channel(self):
        self.manifest.write_text('[package]\nname="file-server-proxy"\nversion = "1.2.3-rc.1"\n', encoding='utf-8')
        self.commit('prerelease source')
        self.git('tag', 'file-server-proxy-v1.2.3-rc.1')
        result, values = self.meta(tag='file-server-proxy-v1.2.3-rc.1')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(values, {'version': '1.2.3-rc.1', 'prerelease': 'true'})

    def test_version_disagreement_is_rejected_before_publishing_outputs(self):
        self.git('tag', 'file-server-proxy-v1.2.4')
        result, values = self.meta(tag='file-server-proxy-v1.2.4')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('version', values)

    def test_different_checkout_is_rejected_before_publishing_outputs(self):
        (self.root / 'later-source').write_text('a different committed checkout\n', encoding='utf-8')
        self.commit('later unrelated source')
        result, values = self.meta()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('version', values)

    def test_wrong_tag_prefix_is_rejected_before_publishing_outputs(self):
        self.git('tag', 'unrelated-v1.2.3')
        result, values = self.meta(tag='unrelated-v1.2.3')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('version', values)


class ProxyReleaseWiringTests(unittest.TestCase):
    def test_each_source_consumer_checks_out_the_requested_release_tag(self):
        for name in ('meta', 'build', 'publish-npm'):
            with self.subTest(job=name):
                steps = WORKFLOW['jobs'][name]['steps']
                checkouts = [step for step in steps if step.get('uses', '').startswith('actions/checkout@')]
                self.assertEqual(len(checkouts), 1)
                self.assertEqual(checkouts[0].get('with', {}).get('ref'), TAG_REF)
        meta = next(step for step in WORKFLOW['jobs']['meta']['steps'] if step.get('id') == 'v')
        self.assertEqual(meta['shell'], 'bash')
        self.assertEqual(meta.get('env', {}).get('REQUESTED_TAG'), TAG_NAME)

    def test_windows_prerequisite_step_uses_its_declared_bash_syntax(self):
        step = next(step for step in WORKFLOW['jobs']['build']['steps']
                    if step.get('name') == 'Install protoc + nasm (Windows)')
        self.assertEqual(step.get('shell'), 'bash')
        self.assertIn('$GITHUB_PATH', step['run'])

    def test_both_cargo_build_paths_use_the_committed_lockfile(self):
        scripts = [step['run'] for step in WORKFLOW['jobs']['build']['steps'] if 'run' in step]
        commands = [line.strip() for script in scripts for line in script.splitlines()
                    if line.strip().startswith(('cargo build ', 'cargo zigbuild '))]
        self.assertEqual(len(commands), 2)
        for command in commands:
            self.assertIn('--locked', command.split())

    def test_wrapper_tests_run_before_npm_publication(self):
        steps = WORKFLOW['jobs']['publish-npm']['steps']
        publishing = next(index for index, step in enumerate(steps) if 'npm publish ' in step.get('run', ''))
        checking = [index for index, step in enumerate(steps)
                    if 'node --test' in step.get('run', '')
                    and step.get('working-directory') == 'crates/file-server-proxy/npm']
        self.assertTrue(checking, 'the released npm wrapper must run its own tests')
        self.assertTrue(all(index < publishing for index in checking))


if __name__ == '__main__':
    unittest.main()
