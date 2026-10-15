"""用 stub 执行真实热构建脚本，验证 feature/flags 与失败传播；不运行 Cargo 或 Docker。"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'docker/dev-hot-build.sh'


class DevHotFeatures(unittest.TestCase):
    def run_script(self, features=None, failure='', rustflags=None, produce_binary=True, encoded=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workspace = root / 'workspace with spaces'
            workspace.mkdir()
            (workspace / 'Cargo.toml').write_text('[workspace]\n')
            installed = root / 'installed/rcoder'
            installed.parent.mkdir()
            installed.write_text('original binary')
            binaries = root / 'stubs'
            binaries.mkdir()
            log = root / 'calls.jsonl'
            stub = f'''#!{sys.executable}
import json, os, shutil, stat, sys
from pathlib import Path
name = Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ['COMMAND_LOG'], 'a') as out:
    out.write(json.dumps({{
        'command': name, 'args': args, 'rustflags': os.environ.get('RUSTFLAGS'),
        'target': os.environ.get('CARGO_TARGET_DIR'), 'cwd': os.getcwd(),
        'encoded': os.environ.get('CARGO_ENCODED_RUSTFLAGS'),
    }}) + '\\n')
if os.environ['FAIL_COMMAND'] == name:
    sys.exit({{'cargo': 41, 'cp': 42, 'mv': 43}}[name])
if name == 'cargo' and os.environ['PRODUCE_BINARY'] == '1':
    artifact = Path(os.environ['CARGO_TARGET_DIR']) / 'release/rcoder'
    artifact.parent.mkdir(parents=True, exist_ok=True)
    artifact.write_text('replacement binary')
elif name == 'cp':
    shutil.copyfile(args[0], args[1])
elif name == 'chmod':
    path = Path(args[-1])
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
elif name == 'mv':
    os.replace(args[-2], args[-1])
elif name in ('apt-get', 'docker'):
    sys.exit(99)
'''
            for name in ('cargo', 'protoc', 'cp', 'chmod', 'mv', 'apt-get', 'docker'):
                path = binaries / name
                path.write_text(stub)
                path.chmod(0o755)
            env = dict(os.environ, PATH=f'{binaries}:{os.environ["PATH"]}',
                       COMMAND_LOG=str(log), FAIL_COMMAND=failure,
                       PRODUCE_BINARY='1' if produce_binary else '0',
                       RCODER_DEV_HOT_SRC_DIR=str(workspace),
                       RCODER_DEV_HOT_BIN_PATH=str(installed))
            for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_DIR'):
                env.pop(key, None)
            if rustflags is not None:
                env['RUSTFLAGS'] = rustflags
            if encoded is not None:
                env['CARGO_ENCODED_RUSTFLAGS'] = encoded
            command = ['bash', str(SCRIPT)]
            if features is not None:
                command.append(features)
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=15)
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result, calls, installed.read_text(), Path(f'{installed}.new').exists()

    def test_omitted_features_keep_existing_default(self):
        result, calls, installed, _ = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0]['args'],
                         ['build', '--release', '--bin', 'rcoder', '--features', 'hotpath,dial9'])
        self.assertEqual(calls[0]['rustflags'], '--cfg tokio_unstable')
        self.assertEqual(Path(calls[0]['target']).name, 'target-unstable')
        self.assertEqual(installed, 'replacement binary')

    def test_explicit_empty_features_use_default_cargo_features(self):
        result, calls, installed, _ = self.run_script('')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0]['args'], ['build', '--release', '--bin', 'rcoder'])
        self.assertEqual(calls[0]['rustflags'], '')
        self.assertEqual(Path(calls[0]['target']).name, 'target')
        self.assertEqual(installed, 'replacement binary')

    def test_mcp_features_are_forwarded_without_unstable_cfg(self):
        for value in ('hotpath,hotpath-mcp', '--features hotpath,hotpath-mcp'):
            with self.subTest(value=value):
                result, calls, _, _ = self.run_script(value)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls[0]['args'][-2:], ['--features', 'hotpath,hotpath-mcp'])
                self.assertEqual(calls[0]['rustflags'], '')
                self.assertEqual(Path(calls[0]['target']).name, 'target')

    def test_dial9_tokens_select_unstable_cfg_and_separate_cache(self):
        for value in ('--features hotpath,dial9', 'rcoder-engine/dial9', 'hotpath dial9'):
            with self.subTest(value=value):
                result, calls, _, _ = self.run_script(value)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls[0]['rustflags'], '--cfg tokio_unstable')
                self.assertEqual(Path(calls[0]['target']).name, 'target-unstable')

    def test_existing_rustflags_are_preserved(self):
        for value, expected in (('', '-C debuginfo=0'),
                                ('dial9', '-C debuginfo=0 --cfg tokio_unstable')):
            with self.subTest(value=value):
                result, calls, _, _ = self.run_script(value, rustflags='-C debuginfo=0')
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls[0]['rustflags'], expected)

    def test_encoded_flags_cannot_silently_disable_dial9_hooks(self):
        for encoded in ('', '-C\x1fdebuginfo=1'):
            for features in ('dial9', ''):
                with self.subTest(encoded=encoded, features=features):
                    result, calls, _, _ = self.run_script(features, encoded=encoded)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    actual = calls[0]['encoded']
                    if features:
                        self.assertEqual(actual.split('\x1f')[-2:], ['--cfg', 'tokio_unstable'])
                        if encoded:
                            self.assertTrue(actual.startswith(encoded + '\x1f'))
                    else:
                        self.assertEqual(actual, encoded)

    def test_failures_propagate_and_keep_installed_binary(self):
        for failure, code, expected_calls in (
            ('cargo', 41, ['cargo']),
            ('cp', 42, ['cargo', 'cp']),
            ('mv', 43, ['cargo', 'cp', 'chmod', 'mv']),
        ):
            with self.subTest(failure=failure):
                result, calls, installed, _ = self.run_script('', failure=failure)
                self.assertEqual(result.returncode, code, result.stderr)
                self.assertEqual([call['command'] for call in calls], expected_calls)
                self.assertEqual(installed, 'original binary')

    def test_missing_artifact_does_not_replace_installed_binary(self):
        result, calls, installed, pending = self.run_script('', produce_binary=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn('未生成', result.stderr)
        self.assertEqual([call['command'] for call in calls], ['cargo'])
        self.assertEqual(installed, 'original binary')
        self.assertFalse(pending)


if __name__ == '__main__':
    unittest.main()
