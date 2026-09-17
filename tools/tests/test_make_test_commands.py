"""验证 Make 测试入口的参数、串行汇总和错误传播；不运行 Rust 或 Docker。"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class MakeTestCommands(unittest.TestCase):
    def run_target(self, target, fail='', *variables):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'Makefile').write_text(f'include {ROOT}/make/test.mk\n')
            (root / 'docker/rcoder-agent-runner').mkdir(parents=True)
            binaries = root / 'bin'
            binaries.mkdir()
            log = root / 'calls.jsonl'
            stub = f'''#!{sys.executable}
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
name = Path(sys.argv[0]).name
with open(os.environ['COMMAND_LOG'], 'a') as out:
    out.write(json.dumps([name, *args]) + '\\n')
mode = os.environ['FAIL_COMMAND']
failed = mode == 'all'
failed |= mode == 'workspace' and name == 'cargo' and 'nextest' in args and '--workspace' in args
failed |= mode == 'app-cli' and name == 'cargo' and 'nextest' in args and '--manifest-path' in args
failed |= mode == 'doc' and name == 'cargo' and '--doc' in args
failed |= mode == 'build' and name == 'docker' and args[0] == 'build'
failed |= mode == 'run' and name == 'docker' and args[0] == 'run'
sys.exit(17 if failed else 0)
'''
            for name in ('cargo', 'docker', 'python3'):
                path = binaries / name
                path.write_text(stub)
                path.chmod(0o755)
            env = dict(os.environ, PATH=f'{binaries}:{os.environ["PATH"]}',
                       COMMAND_LOG=str(log), FAIL_COMMAND=fail)
            for key in ('MAKEFLAGS', 'MFLAGS', 'MAKELEVEL', 'TEST_FEATURES', 'NEXTEST_ARGS'):
                env.pop(key, None)
            result = subprocess.run(['make', '-j4', target, *variables], cwd=root,
                                    env=env, capture_output=True, text=True, timeout=15)
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result.returncode, calls

    def test_nextest_scopes_and_features(self):
        for target, selector in [('test', '--workspace'), ('test-unit', '--lib'),
                                 ('test-integration', '--test'), ('test-app-cli', '--manifest-path')]:
            with self.subTest(target=target):
                code, calls = self.run_target(target)
                self.assertEqual(code, 0)
                self.assertEqual(calls[0][:3], ['cargo', 'nextest', 'run'])
                self.assertIn(selector, calls[0])
                self.assertIn('--all-features', calls[0])
                self.assertIn('--no-fail-fast', calls[0])
        for target in ('test-default', 'test-blocking'):
            code, calls = self.run_target(target)
            self.assertEqual(code, 0)
            self.assertNotIn('--all-features', calls[0])
        self.assertIn('--test-threads=1', calls[0])

    def test_filter_and_default_override(self):
        code, calls = self.run_target('test', '', 'TEST_FEATURES=', 'NEXTEST_ARGS=-p rcoder')
        self.assertEqual(code, 0)
        self.assertNotIn('--all-features', calls[0])
        self.assertEqual(calls[0][-2:], ['-p', 'rcoder'])

    def test_aggregate_collects_later_results_and_propagates_failure(self):
        for failure in ('', 'workspace', 'app-cli', 'doc'):
            with self.subTest(failure=failure):
                code, calls = self.run_target('test-all', failure)
                self.assertEqual(code == 0, failure == '')
                self.assertEqual(len(calls), 4)
                self.assertIn('--workspace', calls[0])
                self.assertIn('--manifest-path', calls[1])
                self.assertIn('--doc', calls[2])
                self.assertIn('--doc', calls[3])

    def test_e2e_keeps_strict_launcher_and_exit_code(self):
        for failure in ('', 'all'):
            code, calls = self.run_target('test-e2e', failure)
            self.assertEqual(code == 0, failure == '')
            self.assertEqual(calls, [['python3', 'tests-e2e/tools/run.py', '--group', 'userapp']])


if __name__ == '__main__':
    unittest.main()
