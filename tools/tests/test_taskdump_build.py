"""执行真实诊断脚本的目录/编译参数契约；Linux与Cargo仅为受控替身。"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'tools/taskdump/build-taskdump-linux.sh'
MANIFEST = 'dial9 = { version = "0.5", default-features = false, features = ["tokio"] }\n'
LOCK = '[[package]]\nname = "dial9"\nversion = "0.5.2"\n'


class TaskdumpBuildContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.log = self.root / 'cargo.jsonl'
        for name in ('python3', 'mkdir', 'dirname', 'cat', 'awk', 'wc', 'tr'):
            executable = sys.executable if name == 'python3' else shutil.which(name)
            self.assertIsNotNone(executable)
            (self.bin / name).symlink_to(executable)
        self.stub('uname', 'print({"-s":"Linux", "-m":"x86_64", "-a":"Linux fixture"}[sys.argv[1]])')
        self.stub('date', 'print("2026-10-03T00:00:00+00:00")')
        self.stub('git', 'print("fixture-sha" if "rev-parse" in sys.argv else "", end="")')
        self.stub('cargo', '''
args = sys.argv[1:]
with open(os.environ['CARGO_LOG'], 'a') as output:
    output.write(json.dumps({'args':args, 'flags':os.environ.get('RUSTFLAGS'),
        'encoded':os.environ.get('CARGO_ENCODED_RUSTFLAGS'), 'cwd':os.getcwd()}) + '\\n')
if args[0] == 'build':
    target = Path(os.environ['CARGO_TARGET_DIR']) / 'release'
    target.mkdir(parents=True)
    (target / 'rcoder').write_text('fixture binary')
''')
        self.source = self.root / 'repo'
        self.source.mkdir()
        (self.source / 'Cargo.toml').write_text(MANIFEST)
        (self.source / 'Cargo.lock').write_text(LOCK)
        (self.source / '.source-marker').write_text('keep')

    def stub(self, name, body):
        p = self.bin / name
        p.write_text(f'#!{sys.executable}\nimport json,os,sys\nfrom pathlib import Path\n{body}\n')
        p.chmod(0o755)

    def run_script(self, source, diagnostic, encoded=None, script=SCRIPT):
        env = dict(os.environ, PATH=str(self.bin), CARGO_LOG=str(self.log),
                   RUSTFLAGS='-C debuginfo=1')
        env.pop('CARGO_ENCODED_RUSTFLAGS', None)
        if encoded is not None:
            env['CARGO_ENCODED_RUSTFLAGS'] = encoded
        return subprocess.run(['/bin/bash', str(script), str(source), str(diagnostic)],
                              env=env, capture_output=True, text=True, timeout=10)

    def test_overlapping_or_symlinked_destinations_refused_before_copy(self):
        alias = self.root / 'alias'
        alias.symlink_to(self.source, target_is_directory=True)
        outside = self.root / 'outside'
        outside.mkdir()
        linked = self.root / 'linked'
        linked.mkdir()
        (linked / 'source').symlink_to(outside, target_is_directory=True)
        for diagnostic in (self.source, self.source / 'nested', self.root, alias, linked):
            with self.subTest(diagnostic=diagnostic):
                r = self.run_script(self.source, diagnostic)
                self.assertNotEqual(r.returncode, 0, r.stdout + r.stderr)
                self.assertEqual((self.source / 'Cargo.toml').read_text(), MANIFEST)
                self.assertFalse(self.log.exists())
        self.assertFalse((self.source / 'nested').exists())

    def test_manifest_symlink_cannot_modify_original_source(self):
        original = self.root / 'original.toml'
        original.write_text(MANIFEST)
        (self.source / 'Cargo.toml').unlink()
        (self.source / 'Cargo.toml').symlink_to(original)
        r = self.run_script(self.source, self.root / 'diag')
        self.assertNotEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(original.read_text(), MANIFEST)
        self.assertFalse((self.root / 'diag').exists())

    def test_known_release_links_cannot_escape_the_independent_target(self):
        outside = self.root / 'outside-target'
        outside.mkdir()
        names = ('release', 'release/deps', 'release/build', 'release/incremental',
                 'release/.fingerprint', 'release/examples', 'release/rcoder')
        for index, name in enumerate(names):
            for destination in (self.source, outside):
                with self.subTest(name=name, destination=destination):
                    diagnostic = self.root / f'diag-links-{index}-{destination.name}'
                    candidate = diagnostic / 'target-taskdump' / name
                    candidate.parent.mkdir(parents=True)
                    candidate.symlink_to(destination, target_is_directory=True)
                    result = self.run_script(self.source, diagnostic)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn('构建目标链接', result.stderr)
                    self.assertFalse(self.log.exists())
                    self.assertEqual((self.source / 'Cargo.toml').read_text(), MANIFEST)

    def test_release_artifact_link_to_known_source_inode_is_refused(self):
        diagnostic = self.root / 'diag-artifact'
        release = diagnostic / 'target-taskdump/release'
        release.mkdir(parents=True)
        original = self.source / 'rcoder'
        original.write_text('source-owned artifact')
        os.link(original, release / 'rcoder')
        result = self.run_script(self.source, diagnostic)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('共享 inode', result.stderr)
        self.assertFalse(self.log.exists())
        self.assertEqual(original.read_text(), 'source-owned artifact')

    def test_copy_isolated_and_cfg_present_for_both_cargo_flag_inputs(self):
        for index, encoded in enumerate((None, '', '-C\x1fdebuginfo=1')):
            with self.subTest(encoded=encoded):
                diagnostic = self.root / f'diag-{index}'
                previous = diagnostic / 'source'
                previous.mkdir(parents=True)
                (previous / '.stale').write_text('remove')
                self.log.unlink(missing_ok=True)
                r = self.run_script(self.source, diagnostic, encoded)
                self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
                self.assertEqual((self.source / 'Cargo.toml').read_text(), MANIFEST)
                self.assertEqual((self.source / 'Cargo.lock').read_text(), LOCK)
                self.assertIn('"taskdump"', (previous / 'Cargo.toml').read_text())
                self.assertEqual((previous / '.source-marker').read_text(), 'keep')
                self.assertFalse((previous / '.stale').exists())
                calls = [json.loads(line) for line in self.log.read_text().splitlines()]
                self.assertEqual(calls[0]['args'], ['update', '-p', 'dial9', '--precise', '0.5.2'])
                self.assertEqual(calls[1]['args'], ['build', '--release', '-p', 'rcoder', '--bin',
                                                  'rcoder', '--features', 'dial9', '--locked'])
                self.assertEqual(calls[1]['flags'], '-C debuginfo=1 --cfg tokio_unstable')
                if encoded is not None:
                    self.assertEqual(calls[1]['encoded'].split('\x1f')[-2:], ['--cfg', 'tokio_unstable'])
                    if encoded:
                        self.assertTrue(calls[1]['encoded'].startswith(encoded + '\x1f'))

    def test_real_rsync_detaches_manifest_and_lock_before_any_writes(self):
        real_rsync = shutil.which('rsync')
        self.assertIsNotNone(real_rsync, 'actual rsync is required for this branch regression')
        (self.bin / 'rsync').symlink_to(real_rsync)
        diagnostic = self.root / 'diag'
        copied = diagnostic / 'source'
        copied.mkdir(parents=True)
        for name in ('Cargo.toml', 'Cargo.lock'):
            os.link(self.source / name, copied / name)
            self.assertEqual((self.source / name).stat().st_ino, (copied / name).stat().st_ino)
        self.stub('cargo', '''
args = sys.argv[1:]
with open(os.environ['CARGO_LOG'], 'a') as output:
    output.write(json.dumps({'args':args}) + '\\n')
if args[0] == 'update':
    with open('Cargo.lock', 'a') as lock:
        lock.write('# fixture lock graph update\\n')
elif args[0] == 'build':
    target = Path(os.environ['CARGO_TARGET_DIR']) / 'release'
    target.mkdir(parents=True)
    (target / 'rcoder').write_text('fixture binary')
''')
        result = self.run_script(self.source, diagnostic)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.source / 'Cargo.toml').read_text(), MANIFEST)
        self.assertEqual((self.source / 'Cargo.lock').read_text(), LOCK)
        self.assertIn('"taskdump"', (copied / 'Cargo.toml').read_text())
        self.assertIn('# fixture lock graph update', (copied / 'Cargo.lock').read_text())
        for name in ('Cargo.toml', 'Cargo.lock'):
            source_stat, copied_stat = (self.source / name).stat(), (copied / name).stat()
            self.assertNotEqual((source_stat.st_dev, source_stat.st_ino),
                                (copied_stat.st_dev, copied_stat.st_ino))
            self.assertEqual(copied_stat.st_nlink, 1)
            self.assertFalse((copied / name).is_symlink())
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual([call['args'][0] for call in calls], ['update', 'build'])


if __name__ == '__main__':
    unittest.main()
