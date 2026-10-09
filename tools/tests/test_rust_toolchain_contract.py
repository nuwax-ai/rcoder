"""检查 CI 实际执行的 feature 组合及发布工具链契约；不编译或发布。"""
import fnmatch
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


class RustToolchainContract(unittest.TestCase):
    def workflow(self, name):
        return yaml.safe_load((ROOT / '.github/workflows' / name).read_text())

    def test_workspace_checks_execute_both_feature_paths(self):
        jobs = self.workflow('quality.yml')['jobs']
        self.assertIn('workspace', jobs, '根 workspace 必须实际执行 Clippy 和测试')
        job = jobs['workspace']
        cases = job['strategy']['matrix']['include']
        self.assertEqual({case['features'] for case in cases}, {'', '--all-features'})
        runs = [step['run'] for step in job['steps'] if 'run' in step]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            cargo = root / 'cargo'
            cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$COMMAND_LOG"\n')
            cargo.chmod(0o755)
            log = root / 'commands'
            for case in cases:
                log.write_text('')
                for script in runs:
                    if script.lstrip().startswith('cargo '):
                        env = dict(os.environ, PATH=f'{root}:{os.environ["PATH"]}',
                                   COMMAND_LOG=str(log), FEATURE_ARGS=case['features'])
                        subprocess.run(['bash', '-eu', '-c', script], env=env, check=True)
                commands = [line.split() for line in log.read_text().splitlines()]
                clippy = next(args for args in commands if args[0] == 'clippy')
                tests = next(args for args in commands if args[:2] == ['nextest', 'run'])
                self.assertIn('--workspace', clippy)
                self.assertIn('--all-targets', clippy)
                self.assertEqual(clippy[-2:], ['-D', 'warnings'])
                self.assertIn('--workspace', tests)
                self.assertIn('--no-fail-fast', tests)
                for args in (clippy, tests):
                    self.assertEqual('--all-features' in args, bool(case['features']))

    def test_release_updates_stable_even_with_preinstalled_cargo(self):
        for name, job_name in [('release.yml', 'build-local-artifacts'),
                               ('release-beta.yml', 'build'),
                               ('release-app-cli.yml', 'build'),
                               ('release-app-cli.yml', 'build-pingap-windows'),
                               ('release-app-cli.yml', 'build-pingap-unix'),
                               ('release-file-server-proxy.yml', 'build')]:
            with self.subTest(workflow=name):
                job = self.workflow(name)['jobs'][job_name]
                self.assertEqual(job.get('env', {}).get('RUSTUP_TOOLCHAIN'), 'stable',
                                 f'{job_name} must explicitly select stable')
                update = next((step for step in job['steps']
                               if 'rustup update stable' in step.get('run', '')), None)
                self.assertIsNotNone(update, '已有 cargo 也必须更新 stable，不能复用 runner 的旧默认版本')
                self.assertNotIn('if', update, '容器与原生 runner 均需更新 stable')
                with tempfile.TemporaryDirectory() as temp:
                    root = Path(temp)
                    log = root / 'commands'
                    for binary in ('rustup', 'rustc', 'cargo'):
                        stub = root / binary
                        stub.write_text(f'#!/bin/sh\nprintf "%s\\n" "{binary} $*" >> "$COMMAND_LOG"\n')
                        stub.chmod(0o755)
                    env = dict(os.environ, PATH=f'{root}:{os.environ["PATH"]}', COMMAND_LOG=str(log),
                               RUSTUP_TOOLCHAIN=job['env']['RUSTUP_TOOLCHAIN'])
                    script = update['run'].replace('${{ matrix.target }}', 'x86_64-unknown-linux-gnu')
                    subprocess.run(['bash', '-eu', '-c', script], env=env, check=True)
                    calls = log.read_text().splitlines()
                    self.assertIn('rustup update stable', calls)
                    self.assertIn('rustc --version', calls)
                    self.assertIn('cargo --version', calls)
                    self.assertLess(calls.index('rustup update stable'), calls.index('rustc --version'))
                    self.assertLess(calls.index('rustup update stable'), calls.index('cargo --version'))
                    if '${{ matrix.target }}' in update['run']:
                        self.assertGreater(calls.index('rustup target add x86_64-unknown-linux-gnu'),
                                           calls.index('rustup update stable'))
                    steps = job['steps']
                    update_index = steps.index(update)
                    last_checkout = max(i for i, step in enumerate(steps)
                                        if step.get('uses', '').startswith('actions/checkout@'))
                    self.assertGreater(update_index, last_checkout)
                    first_build = next(i for i, step in enumerate(steps)
                                       if 'cargo build ' in step.get('run', '')
                                       or 'cargo zigbuild ' in step.get('run', '')
                                       or 'dist build ' in step.get('run', '')
                                       or 'pingap-applied/build.py ' in step.get('run', ''))
                    self.assertLess(update_index, first_build)

    def test_app_cli_path_dependencies_trigger_independent_checks(self):
        root_manifest = tomllib.loads((ROOT / 'Cargo.toml').read_text())
        workspace_dependencies = root_manifest['workspace']['dependencies']
        seen = set()
        pending = [ROOT / 'crates/app-cli/Cargo.toml']
        while pending:
            path = pending.pop().resolve()
            if path in seen:
                continue
            seen.add(path)
            manifest = tomllib.loads(path.read_text())
            owners = [manifest, *manifest.get('target', {}).values()]
            for owner in owners:
                for key in ('dependencies', 'dev-dependencies', 'build-dependencies'):
                    for name, dependency in owner.get(key, {}).items():
                        if not isinstance(dependency, dict):
                            continue
                        directory = path.parent
                        if dependency.get('workspace'):
                            dependency = workspace_dependencies[name]
                            directory = ROOT
                        if isinstance(dependency, dict) and 'path' in dependency:
                            pending.append(directory / dependency['path'] / 'Cargo.toml')
        workflow = self.workflow('app-cli-cross-platform.yml')
        # PyYAML YAML 1.1 parses the GitHub key "on" as boolean True.
        events = workflow.get('on') or workflow[True]
        required = {str(path.relative_to(ROOT)) for path in seen}
        required.update({'Cargo.toml', '.github/actions/install-protoc/action.yml'})
        for event in ('push', 'pull_request'):
            with self.subTest(event=event):
                patterns = events[event]['paths']
                uncovered = sorted(path for path in required
                                   if not any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns))
                self.assertEqual(uncovered, [], f'{event} misses actual app-cli build inputs')

    def test_release_stable_update_failure_stops_version_observation(self):
        workflows = [('release.yml', 'build-local-artifacts'), ('release-beta.yml', 'build'),
                     ('release-app-cli.yml', 'build'), ('release-app-cli.yml', 'build-pingap-windows'),
                     ('release-app-cli.yml', 'build-pingap-unix'), ('release-file-server-proxy.yml', 'build')]
        for name, job_name in workflows:
            with self.subTest(workflow=name, job=job_name):
                job = self.workflow(name)['jobs'][job_name]
                update = next((step for step in job['steps']
                               if 'rustup update stable' in step.get('run', '')), None)
                self.assertIsNotNone(update, '每条实际 Rust 编译发布链都必须更新 stable')
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    log = root / 'calls'
                    (root / 'rustup').write_text('#!/bin/sh\nprintf "%s\\n" "rustup $*" >> "$COMMAND_LOG"\nexit 42\n')
                    for name in ('rustc', 'cargo'):
                        (root / name).write_text(f'#!/bin/sh\nprintf "%s\\n" "{name} $*" >> "$COMMAND_LOG"\n')
                    for name in ('rustup', 'rustc', 'cargo'):
                        (root / name).chmod(0o755)
                    env = dict(os.environ, PATH=f'{root}:{os.environ["PATH"]}', COMMAND_LOG=str(log),
                               RUSTUP_TOOLCHAIN='stable')
                    script = update['run'].replace('${{ matrix.target }}', 'x86_64-unknown-linux-gnu')
                    result = subprocess.run(['bash', '-eu', '-c', script], env=env)
                    self.assertEqual(result.returncode, 42)
                    self.assertEqual(log.read_text().splitlines(), ['rustup update stable'])

    def test_agent_builder_uses_bookworm_rust_image(self):
        dockerfile = (ROOT / 'docker/rcoder-agent-runner/Dockerfile.build').read_text()
        self.assertIn('FROM rust:bookworm AS builder', dockerfile)
        self.assertNotIn('sh.rustup.rs', dockerfile, '不要把工具链冻结在 apt/rustup 缓存层')
        self.assertIn('/usr/local/cargo/config.toml', dockerfile)


if __name__ == '__main__':
    unittest.main()
