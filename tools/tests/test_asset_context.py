import importlib.util
import concurrent.futures
import json
import hashlib
import os
import re
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from tools.tests import test_runtime_assets as fixtures
assets = fixtures.assets
ROOT = fixtures.ROOT

spec = importlib.util.spec_from_file_location('asset_context', ROOT / 'tools/build/asset_context.py')
context = importlib.util.module_from_spec(spec)
spec.loader.exec_module(context)


class AssetContextTests(unittest.TestCase):
    setUp = fixtures.RuntimeAssetsTests.setUp
    downloader = fixtures.RuntimeAssetsTests.downloader
    prepare = fixtures.RuntimeAssetsTests.prepare
    def refs(self):
        refs = self.root / 'refs'
        refs.mkdir()
        for component, version in [('ttyd', '1.7.7'), ('node', '22.23.2'), ('deno', '2.9.7'), ('go', '1.26.4'), ('pingap', '0.14.3')]:
            entry = self.prepare(component, version)
            assets.atomic_json(refs / (component + '.ref'), {'entry': str(entry), 'manifest_sha256': assets.digest(entry / 'manifest.json')})
        dbx = self.root / 'dbx'
        (dbx / 'dbx-static').mkdir(parents=True)
        (dbx / 'dbx-static/index.html').write_text('dbx')
        for arch in ('amd64', 'arm64'):
            (dbx / ('dbx-web-' + arch)).write_bytes(b'dbx-' + arch.encode())
        files = {str(p.relative_to(dbx)): {'sha256': assets.digest(p)} for p in dbx.rglob('*') if p.is_file()}
        assets.atomic_json(dbx / 'manifest.json', {'protocol': 2, 'files': files})
        assets.atomic_json(refs / 'dbx.ref', {'protocol': 2, 'entry': str(dbx), 'manifest_sha256': assets.digest(dbx / 'manifest.json')})
        return sorted(refs.glob('*.ref'))

    def source(self):
        source = self.root / 'source'
        (source / 'downloads').mkdir(parents=True)
        (source / 'cache').mkdir()
        (source / 'Dockerfile').write_text('FROM scratch')
        (source / 'downloads/swagger-ui-v5.17.14.zip').write_bytes(b'swagger')
        (source / 'downloads/dbx-web-amd64').write_bytes(b'old-mutable')
        (source / 'cache/node-v22.0.0-linux-x64.tar.gz').write_bytes(b'old-node')
        return source

    def test_cli_snapshot_uses_script_repository_cache_instead_of_system_temp(self):
        self.refs()
        output = self.root / 'observed-context.txt'
        observer = self.root / 'observe-context.py'
        observer.write_text('import pathlib,sys\npathlib.Path(' + repr(str(output)) + ').write_text(sys.argv[-1])\n')
        isolated = self.root / 'script-repository'
        scripts = isolated / 'tools/build'
        scripts.mkdir(parents=True)
        for name in ('asset_context.py', 'runtime_assets.py'):
            shutil.copy2(ROOT / 'tools/build' / name, scripts / name)
        (scripts / 'pingap-assets.json').write_text(json.dumps(self.catalog))
        result = subprocess.run(['python3', str(scripts / 'asset_context.py'),
                                 '--source', str(self.source()), '--ref-dir', str(self.root / 'refs'),
                                 '--kind', 'runtime', '--', 'python3', str(observer), '{context}'],
                                cwd=self.root, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        created = Path(output.read_text())
        self.assertEqual(created.parent, (isolated / '.cache/build-contexts').resolve())
        self.assertFalse(created.exists(), 'completed private context was not cleaned')

    def test_snapshot_preserves_unrelated_assets_and_excludes_stale_versions(self):
        source = self.source()
        target = self.root / 'snapshot'
        versions = context.snapshot(source, target, self.refs(), 'runtime', trusted_catalog=self.catalog)
        self.assertEqual(versions['node'], '22.23.2')
        self.assertEqual((target / 'downloads/swagger-ui-v5.17.14.zip').read_bytes(), b'swagger')
        self.assertEqual((target / 'downloads/dbx-web-amd64').read_bytes(), b'dbx-amd64')
        self.assertFalse((target / 'cache/node-v22.0.0-linux-x64.tar.gz').exists())
        (source / 'downloads/dbx-web-amd64').write_bytes(b'another-build')
        self.assertEqual((target / 'downloads/dbx-web-amd64').read_bytes(), b'dbx-amd64')

    def test_runtime_named_context_includes_paired_helpers_and_identity(self):
        module = ROOT / 'docker/build-app-runtime.py'
        spec = importlib.util.spec_from_file_location('runtime_build_paired_context', module)
        runtime = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(runtime)
        fixture = self.root / 'rcoder-source'
        fixture.mkdir()
        for name in ('Cargo.toml', 'Cargo.lock'):
            (fixture / name).write_text(name)
        for name in ('crates', 'tests-e2e'):
            (fixture / name).mkdir()
        build = fixture / 'tools/build'
        build.mkdir(parents=True)
        for name in ('pingap_identity.py', 'pingap-assets.json'):
            shutil.copy2(ROOT / 'tools/build' / name, build / name)
        shutil.copytree(ROOT / 'tools/build/pingap-applied', build / 'pingap-applied')
        (build / 'credentials.json').write_text('must not be copied')
        destination = self.root / 'named-cargo-context'
        destination.mkdir()
        runtime.prepare_context(fixture, destination)
        copied = destination / 'tools/build'
        self.assertTrue((copied / 'pingap-applied/build.py').is_file())
        self.assertTrue((copied / 'pingap-applied/applied-reload.patch').is_file())
        self.assertTrue((copied / 'pingap_identity.py').is_file())
        self.assertTrue((copied / 'pingap-assets.json').is_file())
        self.assertFalse((copied / 'credentials.json').exists())
        self.assertFalse(list(copied.rglob('__pycache__')))

    def test_agent_maps_pingap_to_downloads_without_node(self):
        target = self.root / 'snapshot'
        context.snapshot(self.source(), target, self.refs(), 'agent', trusted_catalog=self.catalog)
        self.assertTrue((target / 'downloads/pingap-v0.14.3-linux-gnu-x86-full.tar.gz').exists())
        self.assertFalse(list((target / 'cache').glob('node-*')))

    def test_missing_and_changed_reference_fail(self):
        source = self.source()
        refs = self.refs()
        with self.assertRaises(ValueError):
            context.snapshot(source, self.root / 'missing', [], 'runtime')
        pointer = json.loads(refs[0].read_text())
        (Path(pointer['entry']) / 'manifest.json').write_text('{}')
        with self.assertRaisesRegex(ValueError, 'manifest changed'):
            context.snapshot(source, self.root / 'changed', refs, 'runtime')

    def test_self_signed_pingap_manifest_cannot_replace_official_sha(self):
        refs = self.refs()
        ref = next(ref for ref in refs if ref.stem == 'pingap')
        pointer = json.loads(ref.read_text())
        entry = Path(pointer['entry'])
        manifest = json.loads((entry / 'manifest.json').read_text())
        relative = next(iter(manifest['files']))
        (entry / relative).write_bytes(b'forged tarball')
        checksum = assets.digest(entry / relative)
        manifest['files'][relative] = checksum
        manifest['identity']['trusted_release']['assets']['amd64']['sha256'] = checksum
        assets.atomic_json(entry / 'manifest.json', manifest)
        pointer['manifest_sha256'] = assets.digest(entry / 'manifest.json')
        assets.atomic_json(ref, pointer)
        with self.assertRaisesRegex(ValueError, 'trusted release identity'):
            context.snapshot(self.source(), self.root / 'snapshot', refs, 'agent', trusted_catalog=self.catalog)

    def test_downloads_requires_explicit_component_contract(self):
        with self.assertRaisesRegex(ValueError, 'required components'):
            context.snapshot(self.source(), self.root / 'snapshot', self.refs(), 'downloads')

    def test_downloads_selects_only_declared_refs_and_maps_exact_files(self):
        refs = self.refs()
        # An unrelated incomplete Pingap reference must not make a Node/Deno
        # consumer depend on Pingap or its CLI authority.
        next(ref for ref in refs if ref.stem == 'pingap').write_text('{}')
        target = self.root / 'snapshot'
        versions = context.snapshot(self.source(), target, refs, 'downloads', required={'node', 'deno'})
        self.assertEqual(versions, {'node': '22.23.2', 'deno': '2.9.7'})
        self.assertTrue((target / 'downloads/node-v22.23.2-linux-x64.tar.gz').is_file())
        self.assertTrue((target / 'downloads/deno-amd64').is_file())
        self.assertFalse(list((target / 'downloads').glob('pingap-*')))
        self.assertFalse(list((target / 'downloads').glob('ttyd-*')))
        self.assertFalse(list((target / 'cache').glob('node-*')))
        self.assertEqual((target / 'downloads/swagger-ui-v5.17.14.zip').read_bytes(), b'swagger')

    def test_downloads_missing_declared_component_fails(self):
        refs = [ref for ref in self.refs() if ref.stem != 'deno']
        with self.assertRaisesRegex(ValueError, 'deno'):
            context.snapshot(self.source(), self.root / 'snapshot', refs, 'downloads', required={'node', 'deno'})

    def test_manifest_cannot_override_actual_build_version(self):
        with self.assertRaisesRegex(ValueError, 'node.*22.24.0.*22.23.2'):
            context.runtime_build_args({'node': '22.23.2'},
                                       ['docker', 'buildx', 'build', '--build-arg', 'NODE_RUNTIME_VERSION=22.24.0', '{context}'])

    def test_concurrent_downloads_snapshots_use_their_own_node_generations(self):
        refs1 = self.refs()
        node = self.prepare('node', '22.24.0')
        newer = self.root / 'new-node.ref'
        assets.atomic_json(newer, {'entry': str(node), 'manifest_sha256': assets.digest(node / 'manifest.json')})
        # References use component names; each operation owns its ref directory.
        newer_dir = self.root / 'new-refs'
        newer_dir.mkdir()
        newer.rename(newer_dir / 'node.ref')
        source = self.source()
        old_refs = [ref for ref in refs1 if ref.stem == 'node']
        operations = [(self.root / 'old-snapshot', old_refs),
                      (self.root / 'new-snapshot', [newer_dir / 'node.ref'])]
        with concurrent.futures.ThreadPoolExecutor(2) as workers:
            outcomes = list(workers.map(lambda item: context.snapshot(source, item[0], item[1], 'downloads', required={'node'}), operations))
        self.assertEqual([outcome['node'] for outcome in outcomes], ['22.23.2', '22.24.0'])
        self.assertTrue((operations[0][0] / 'downloads/node-v22.23.2-linux-x64.tar.gz').exists())
        self.assertFalse((operations[0][0] / 'downloads/node-v22.24.0-linux-x64.tar.gz').exists())
        self.assertTrue((operations[1][0] / 'downloads/node-v22.24.0-linux-x64.tar.gz').exists())
        self.assertFalse((operations[1][0] / 'downloads/node-v22.23.2-linux-x64.tar.gz').exists())


class VersionGateTests(unittest.TestCase):
    def fixture(self, root):
        for relative in ('make/docker.mk', 'docker/build-app-runtime.py', 'tools/build/pingap_identity.py',
                         'tools/build/pingap-assets.json', 'crates/app-cli/Cargo.toml', 'crates/app-cli/src/build_deploy/devtool.rs'):
            target = root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((ROOT / relative).read_bytes())

    def run_gate(self, root, *extra, env=None):
        return subprocess.run(['python3', str(ROOT / 'k8s/scripts/pingap_version_gate.py'), '--repo-root', str(root), *extra], capture_output=True, text=True,
                              env=env if env is not None else self.clean_environment())

    def clean_environment(self):
        env = os.environ.copy()
        for name in ('PINGAP_VERSION', 'PINGAP_COMMIT', 'PINGAP_DL_VERSION', 'MAKEFLAGS', 'MFLAGS', 'GNUMAKEFLAGS', 'MAKEOVERRIDES', 'MAKEFILES'):
            env.pop(name, None)
        return env

    def test_local_is_self_contained_cross_repo_is_explicit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'rcoder'
            self.fixture(root)
            self.assertEqual(self.run_gate(root).returncode, 0)
            self.assertNotEqual(self.run_gate(root, '--cross-repo').returncode, 0)

    def test_cross_repo_reads_dynamic_identity_and_rejects_drift(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'rcoder'
            bad = Path(temporary) / 'build-agent-docker'
            self.fixture(root)
            scripts = bad / 'scripts/build'
            scripts.mkdir(parents=True)
            # 受控外部身份提供方：版本没有 commit 字面量，门禁必须调用 CLI。
            (bad / 'versions.mk').write_text('PINGAP_VERSION ?= 0.15.0\nPINGAP_COMMIT ?= $(shell resolve-release-metadata)\n')
            (scripts / 'pingap_config.py').write_text(
                'import json, sys\nfrom pathlib import Path\n'
                'assert sys.argv[1] == "--root" and sys.argv[3:] == ["--field", "pair"]\n'
                'identity = json.loads((Path(sys.argv[2]) / "identity.json").read_text())\n'
                'print(identity["version"] + " " + identity["commit"])\n')
            catalog = json.loads((root / 'tools/build/pingap-assets.json').read_text())
            identity_path = bad / 'identity.json'
            identity_path.write_text(json.dumps({'version': '0.15.0', 'commit': catalog['releases']['0.15.0']['commit']}))
            for name in ('start-up.sh', 'start-up-common.sh', 'start-up-docker-extra.sh', 'start-up-k8s-extra.sh'):
                for directory in (root / 'docker/rcoder-agent-runner', bad / 'build_config/rcoder-agent-runner'):
                    directory.mkdir(parents=True, exist_ok=True)
                    (directory / name).write_text('same startup contract\n')
            result = self.run_gate(root, '--build-agent-docker', str(bad))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            identity_path.write_text(json.dumps({'version': '0.14.3', 'commit': 'b' * 40}))
            result = self.run_gate(root, '--build-agent-docker', str(bad))
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('0.14.3', result.stdout)
            self.assertIn('versions.mk', result.stdout)

    def test_actual_override_and_config_revision_fail_before_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            self.assertNotEqual(self.run_gate(root, '--download-version', '0.1.0').returncode, 0)
            self.assertNotEqual(self.run_gate(root, '--node-version', '24.0.0').returncode, 0)
            cargo = root / 'crates/app-cli/Cargo.toml'
            import tomllib
            revision = tomllib.loads(cargo.read_text())['dependencies']['pingap-config']['rev']
            cargo.write_text(cargo.read_text().replace(revision, '0' * 40))
            result = self.run_gate(root)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('实际 pingap-config rev', result.stdout)

    def test_defaults_follow_a_new_source_identity_without_consumer_edits(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            catalog_path = root / 'tools/build/pingap-assets.json'
            catalog = json.loads(catalog_path.read_text())
            old_version = '0.15.0'
            old_commit = catalog['releases'][old_version]['commit']
            new_version, new_commit = '0.16.0', 'a' * 40
            for relative in ('crates/app-cli/Cargo.toml', 'crates/app-cli/src/build_deploy/devtool.rs'):
                path = root / relative
                path.write_text(path.read_text().replace(old_version, new_version).replace(old_commit, new_commit))
            catalog['releases'][new_version] = {'tag': 'v' + new_version, 'commit': new_commit}
            catalog_path.write_text(json.dumps(catalog))
            result = self.run_gate(root, env=self.clean_environment())
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn('pingap ' + new_version, result.stdout)

    def test_wrong_defaults_are_not_hidden_by_matching_overrides(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            import tomllib
            commit = tomllib.loads((root / 'crates/app-cli/Cargo.toml').read_text())['dependencies']['pingap-config']['rev']
            env = self.clean_environment()
            env.update(PINGAP_VERSION='0.15.0', PINGAP_COMMIT=commit)
            makefile = root / 'make/docker.mk'
            original = makefile.read_text()
            makefile.write_text(original.replace('PINGAP_VERSION ?= $(shell python3 tools/build/pingap_identity.py --field version)', 'PINGAP_VERSION ?= 0.1.0'))
            result = self.run_gate(root, '--pingap-version', '0.15.0', '--pingap-commit', commit, env=env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('[不一致] make/docker.mk（dev agent-runner 注入）默认', result.stdout)
            self.assertIn('0.1.0', result.stdout)
            makefile.write_text(original)
            runtime = root / 'docker/build-app-runtime.py'
            runtime.write_text(runtime.read_text().replace("args.pingap_version = identity['version']", "args.pingap_version = '0.1.0'"))
            result = self.run_gate(root, '--pingap-version', '0.15.0', '--pingap-commit', commit, env=env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('[不一致] docker/build-app-runtime.py（dev app-runtime build-arg）默认', result.stdout)
            self.assertIn('0.1.0', result.stdout)

    def test_wrong_environment_override_fails_before_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            env = self.clean_environment()
            env['PINGAP_VERSION'] = '0.1.0'
            result = self.run_gate(root, env=env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('当前环境', result.stdout)

    def test_explicit_build_pair_overrides_stale_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            catalog = json.loads((root / 'tools/build/pingap-assets.json').read_text())
            commit = catalog['releases']['0.15.0']['commit']
            env = self.clean_environment()
            env.update(PINGAP_VERSION='0.14.3', PINGAP_COMMIT='b' * 40)
            result = self.run_gate(root, '--pingap-version', '0.15.0', '--pingap-commit', commit, env=env)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn('[一致]   当前请求 Pingap build args', result.stdout)

    def test_runtime_cli_overrides_stale_environment_before_asset_preparation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            gate = root / 'k8s/scripts/pingap_version_gate.py'
            gate.parent.mkdir(parents=True)
            gate.write_bytes((ROOT / 'k8s/scripts/pingap_version_gate.py').read_bytes())
            catalog = json.loads((root / 'tools/build/pingap-assets.json').read_text())
            commit = catalog['releases']['0.15.0']['commit']
            env = self.clean_environment()
            env.update(PINGAP_VERSION='0.14.3', PINGAP_COMMIT='b' * 40)
            spec = importlib.util.spec_from_file_location('runtime_build', root / 'docker/build-app-runtime.py')
            runtime = importlib.util.module_from_spec(spec)
            with mock.patch.object(sys, 'path', sys.path.copy()):
                spec.loader.exec_module(runtime)
                with mock.patch.dict(os.environ, env, clear=True), mock.patch.object(
                    sys, 'argv', [str(root / 'docker/build-app-runtime.py'), str(root / 'runtime'),
                                  '--pingap-version', '0.15.0', '--pingap-commit', commit]
                ), mock.patch.object(
                    runtime.importlib.util, 'spec_from_file_location',
                    side_effect=RuntimeError('gate accepted before asset import')
                ):
                    # 实际 main + 子进程 gate；在资产加载边界中止，不准备资产或调用 Docker。
                    with self.assertRaisesRegex(RuntimeError, 'gate accepted before asset import'):
                        runtime.main()
            self.assertFalse((root / '.cache').exists())

    def test_explicit_build_pair_cannot_hide_invalid_actual_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            catalog = json.loads((root / 'tools/build/pingap-assets.json').read_text())
            commit = catalog['releases']['0.15.0']['commit']
            env = self.clean_environment()
            env.update(PINGAP_VERSION='0.15.0', PINGAP_COMMIT=commit)
            for arguments in (
                ['--pingap-version', '0.14.3', '--pingap-commit', commit],
                ['--pingap-version', '0.15.0', '--pingap-commit', 'b' * 40],
                ['--pingap-version', '0.15.0'],
                ['--pingap-commit', commit],
                ['--pingap-version', '0.15.0', '--pingap-commit', commit, '--download-version', '0.14.3'],
                ['--pingap-version', '0.15.0', '--pingap-commit', commit, '--node-version', '24.0.0'],
            ):
                with self.subTest(arguments=arguments):
                    result = self.run_gate(root, *arguments, env=env)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_print_interfaces_do_not_download_or_run_docker(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            trap_bin = root / 'trap-bin'
            trap_bin.mkdir()
            marker = root / 'unexpected-external-call'
            for name in ('docker', 'curl', 'wget'):
                path = trap_bin / name
                path.write_text('#!/bin/sh\nprintf called > "$UNEXPECTED_CALL_MARKER"\nexit 97\n')
                path.chmod(0o755)
            env = self.clean_environment()
            env.update(PATH=str(trap_bin) + os.pathsep + env['PATH'], UNEXPECTED_CALL_MARKER=str(marker))
            for command in (['make', '--no-print-directory', '-s', '-f', 'make/docker.mk', 'print-pingap-build-identity'],
                            ['python3', str(root / 'docker/build-app-runtime.py'), '--print-pingap-identity']):
                with self.subTest(command=command):
                    result = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertRegex(result.stdout.strip(), r'^0\.15\.0 [0-9a-f]{40}$')
            self.assertFalse(marker.exists())
            self.assertFalse((root / '.cache').exists())

    def test_dev_deployment_inherits_builder_image_pingap_identity(self):
        image = (ROOT / 'docker/rcoder-agent-runner/Dockerfile').read_text()
        config = (ROOT / 'docker/config.yml').read_text()
        compose = (ROOT / 'docker/docker-compose.yml').read_text()
        for field in ('VERSION', 'COMMIT'):
            # 锁写入者在 builder 中；版本必须来自构建参数烘焙的镜像环境。
            self.assertRegex(image, re.compile(r'^ARG PINGAP_' + field + r'\s*$', re.MULTILINE))
            self.assertIn('RCODER_PINGAP_' + field + '=${PINGAP_' + field + '}', image)
            # 配置不能用静态值或空 env 覆盖镜像自带身份。
            self.assertNotRegex(config, re.compile(r'^\s*RCODER_PINGAP_' + field + r'\s*:', re.MULTILINE))
            self.assertNotRegex(compose, re.compile(r'^\s*-\s*RCODER_PINGAP_' + field + r'=', re.MULTILINE))
        # digest 无法从 Pingap 编译身份推导，仍须独立提供给 release.lock 写入者。
        self.assertRegex(config, re.compile(r'^\s*RCODER_RUNTIME_IMAGE_DIGEST:\s*"[^"\n]+"\s*$', re.MULTILINE))
        self.assertRegex(compose, re.compile(r'^\s*-\s*RCODER_RUNTIME_IMAGE_DIGEST=\$\{RCODER_RUNTIME_IMAGE_DIGEST:-[^}\n]+\}\s*$', re.MULTILINE))


if __name__ == '__main__':
    unittest.main()
