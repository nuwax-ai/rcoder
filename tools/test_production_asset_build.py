#!/usr/bin/env python3
"""Explicit production Make regression gate; no Docker daemon or network used."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

LOCAL = Path(__file__).resolve().parents[1]
COMMIT = 'cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8'


class ProductionBuildTests(unittest.TestCase):
    def test_scripts_are_classified_and_shared_helpers_match(self):
        for group, names in {
            'build': ['dbx_cache.py', 'runtime_assets.py', 'asset_context.py', 'runtime_preflight.py'],
            'registry': ['registry-copy-if-absent.sh', 'export-docker-skopeo.sh', 'docker-image-retention.sh'],
            'ci': ['register-gitea-runner.sh', 'setup-build-node.sh', 'mirror-prep.sh', 'mirror-sanitize.sh'],
            'tests': ['test_mirror_prep.py', 'test_registry_copy.py'],
        }.items():
            for name in names:
                self.assertTrue((self.peer / 'scripts' / group / name).is_file())
                self.assertFalse((self.peer / 'scripts' / name).exists())
        for name in ['dbx_cache.py', 'runtime_assets.py', 'asset_context.py', 'production_preflight.py']:
            production_name = 'runtime_preflight.py' if name == 'production_preflight.py' else name
            self.assertEqual((LOCAL / 'tools/build' / name).read_bytes(),
                             (self.peer / 'scripts/build' / production_name).read_bytes())

    def test_static_dependency_versions_have_one_source(self):
        source = self.peer / 'versions.mk'
        self.assertTrue(source.is_file(), 'production dependency versions have no single source')
        names = ['NODE_RUNTIME_VERSION', 'GO_VERSION', 'DENO_VERSION', 'TTYD_VERSION',
                 'PINGAP_VERSION', 'PINGAP_COMMIT', 'BUN_VERSION', 'LIBREOFFICE_VERSION',
                 'FFMPEG_VERSION', 'GH_VERSION', 'SWAGGER_UI_VERSION', 'WEBSOCKIFY_TAG',
                 'DBX_FORK_BRANCH', 'DBX_IMAGE', 'DBX_ZIGLANG_VERSION', 'DBX_CARGO_ZIGBUILD_VERSION', 'PNPM_MAJOR', 'RUST_BASE_TAG']
        content = source.read_text()
        for name in names:
            with self.subTest(name=name):
                self.assertEqual(len(re.findall(r'^' + name + r'\s*[?:]?=', content, re.MULTILINE)), 1)
                for path in (self.peer / 'makefiles').glob('*.mk'):
                    self.assertNotRegex(path.read_text(), r'(?m)^' + name + r'\s*[?:]?=')
        for path in (self.peer / 'build_config').rglob('Dockerfile*'):
            # Vendored application checkouts keep their own build contracts.
            # This gate covers the production repository's Docker consumers.
            if 'code' in path.relative_to(self.peer / 'build_config').parts:
                continue
            self.assertNotRegex(path.read_text(), r'(?m)^ARG (?:NODE_RUNTIME_VERSION|GO_VERSION|DENO_VERSION|TTYD_VERSION|LIBREOFFICE_VERSION)=')

    def fixture(self, overrides=(), missing_authority=False):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        scripts = root / 'scripts/build'
        scripts.mkdir(parents=True)
        configuration = (self.peer / 'versions.mk').read_text()
        (root / 'versions.mk').write_text(configuration)
        pingap_version = re.search(r'^PINGAP_VERSION\s*\?=\s*(\S+)', configuration, re.MULTILINE).group(1)
        pingap_commit = re.search(r'^PINGAP_COMMIT\s*\?=\s*(\S+)', configuration, re.MULTILINE).group(1)
        shutil.copy2(LOCAL / 'tools/build/production_preflight.py', scripts / 'runtime_preflight.py')
        shutil.copy2(LOCAL / 'tools/build/asset_context.py', scripts / 'asset_context.py')
        for name in ['agent', 'runtime', 'rcoder', 'mcp']:
            context = root / name
            source = context / 'code/rcoder'
            (source / 'crates/app-cli/src').mkdir(parents=True)
            (source / 'crates/app-cli/src/devtool.rs').write_text(
                f'const DEFAULT_PINGAP_VERSION: &str = "{pingap_version}";\n'
                f'const DEFAULT_PINGAP_COMMIT: &str = "{pingap_commit}";\n')
            (source / 'crates/app-cli/Cargo.toml').write_text(
                '[dependencies]\npingap-config = { git="https://github.com/vicanso/pingap", rev="' + pingap_commit + '" }\n')
            (context / 'Dockerfile').write_text('FROM scratch\nCOPY downloads/swagger-ui-v5.17.14.zip /tmp/swagger.zip\n')
            (context / 'Dockerfile.base').write_text('FROM scratch\nCOPY cache/ /tmp/cache/\n')
            (context / 'profile.txt').write_text(name)
            (context / 'downloads').mkdir()
            (context / 'downloads/swagger-ui-v5.17.14.zip').write_bytes(b'fixture swagger')
            (context / 'downloads/pingap-v0.1.0-linux-gnu-x86-full.tar.gz').write_bytes(b'stale asset')
            (context / 'downloads/node-v22.23.2-linux-x64.tar.gz').write_bytes(b'mutable wrong Node')
        if missing_authority:
            for name in ['agent', 'runtime', 'rcoder', 'mcp']:
                shutil.rmtree(root / name / 'code/rcoder/crates/app-cli')
        (root / 'makefiles').mkdir()
        shutil.copy2(self.peer / 'makefiles/16-app-runtime.mk', root / 'makefiles/16-app-runtime.mk')
        ref_dir = root / 'refs'
        ref_dir.mkdir()
        for component, version in [('dbx', ''), ('pingap', pingap_version), ('node', '22.23.2'), ('go', '1.26.4'), ('deno', '2.9.7'), ('ttyd', '1.7.7')]:
            entry = root / 'entries' / component
            entry.mkdir(parents=True)
            paths = ({'dbx-web-amd64': b'fixture amd64', 'dbx-web-arm64': b'fixture arm64', 'dbx-static/index.html': b'fixture frontend'} if component == 'dbx' else
                     {f'cache/pingap-v{version}-linux-gnu-x86-full.tar.gz': b'fixture pingap amd64', f'cache/pingap-v{version}-linux-gnu-aarch64-full.tar.gz': b'fixture pingap arm64'} if component == 'pingap' else
                     {f'downloads/ttyd-{arch}': b'fixture ttyd' for arch in ['amd64', 'arm64']} if component == 'ttyd' else
                     {f'cache/node-v{version}-linux-{cpu}.tar.gz': b'fixture node' for cpu in ['x64', 'arm64']} if component == 'node' else
                     {f'cache/go{version}.linux-{arch}.tar.gz': b'fixture go' for arch in ['amd64', 'arm64']} if component == 'go' else
                     {f'cache/{component}-{arch}': b'fixture ' + component.encode() for arch in ['amd64', 'arm64']})
            for name, content in paths.items():
                destination = entry / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(content)
            files = {name: hashlib.sha256(content).hexdigest() for name, content in paths.items()}
            if component == 'dbx':
                manifest = {'protocol': 2, 'inputs': {}, 'files': {name: {'sha256': checksum, 'size': len(paths[name]), 'mode': 0o755} for name, checksum in files.items()}}
            else:
                manifest = {'protocol': 'runtime-assets-v1', 'identity': {'component': component, 'version': version}, 'files': files}
            content = json.dumps(manifest).encode()
            (entry / 'manifest.json').write_bytes(content)
            reference = {'entry': str(entry), 'manifest_sha256': hashlib.sha256(content).hexdigest()}
            if component == 'dbx':
                reference['protocol'] = 2
            (ref_dir / (component + '.ref')).write_text(json.dumps(reference))
        bin_dir = root / 'bin'
        bin_dir.mkdir()
        (bin_dir / 'docker').write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args=sys.argv[1:]
if args[:2] == ['buildx','build']:
    context=pathlib.Path(args[-2] if args[-1] == '--load' else args[-1])
    assert (context/'asset-manifest.json').is_file(), 'Docker consumed mutable context'
    assert not (context/'downloads/pingap-v0.1.0-linux-gnu-x86-full.tar.gz').exists(), 'old owned asset leaked'
    assert (context/'downloads/swagger-ui-v5.17.14.zip').read_bytes() == b'fixture swagger', 'extra asset lost'
    assert pathlib.Path(args[args.index('-f')+1]).exists(), 'snapshot Dockerfile lost'
    manifests=json.loads((context/'asset-manifest.json').read_text())
    profile=(context/'profile.txt').read_text()
    base=pathlib.Path(args[args.index('-f')+1]).name == 'Dockerfile.base'
    expected=({'node','ttyd','go'} if base else {'dbx','pingap'}) if profile == 'agent' else {'node','ttyd'} if profile == 'rcoder' else {'node','deno'} if profile == 'mcp' else {'dbx','pingap','node','ttyd','go','deno'}
    assert {manifest['component'] for manifest in manifests} == expected, 'wrong declared snapshot components'
    if 'node' in expected:
        node_version=next(part.split('=',1)[1] for part in reversed(args) if part.startswith('NODE_RUNTIME_VERSION='))
        node_file=context/('cache' if profile == 'runtime' else 'downloads')/('node-v'+node_version+'-linux-x64.tar.gz')
        assert node_file.read_bytes().startswith(b'fixture node'), 'mutable Node used'
        if 'EXPECTED_NODE_CONTENT' in os.environ:
            assert node_file.read_text() == os.environ['EXPECTED_NODE_CONTENT'], 'another caller Node used'
    with open(os.environ['CALL_LOG'],'a') as log: log.write('docker-build '+json.dumps(args)+'\\n')
elif args[:1] == ['tag']:
    with open(os.environ['CALL_LOG'],'a') as log: log.write('docker-tag\\n')
elif args[:2] == ['image','inspect']:
    pass
else:
    raise RuntimeError(args)
''')
        (bin_dir / 'docker').chmod(0o755)
        prep = ['download-go-cache', 'download-swagger-ui', 'download-pingap-cache', 'build-dbx-fork',
                'resolve-npm-versions', 'download-ttyd-amd64', 'download-ttyd-arm64', 'download-node', 'download-deno',
                'ensure-local-base-image', 'ensure-default-context', 'update-app-runtime-agent',
                'download-bun-amd64', 'download-bun-arm64', 'download-libreoffice-amd64', 'download-libreoffice-arm64',
                'download-ffmpeg', 'download-gh-amd64', 'download-gh-arm64', 'download-novnc', 'download-pcmflux',
                'prepare-agent-runner-maven-settings', 'ensure-buildx-builder', 'download-ttyd',
                'cluster-init', 'cluster-gen-config', 'init-dirs', 'setup']
        makefile = (
            f'PROJECT_ROOT := {root}\nBUILD_CONFIG_DIR := {root}/build_config\n'
            f'AGENT_RUNNER_CONFIG_PATH := {root}/agent\nAGENT_RUNNER_SRC_PATH := {root}/agent/code/rcoder\n'
            f'RCODER_CONFIG_PATH := {root}/rcoder\nMCP_PROXY_CONFIG_PATH := {root}/mcp\nMCP_PROXY_SRC_PATH := {root}/mcp/code\n'
            f'RCODER_SOURCE_DIR := {root}/runtime/code/rcoder\nASSET_REF_DIR := {ref_dir}\n'
            f'ASSET_CONTEXT_TOOL := {scripts}/asset_context.py\nTTYD_VERSION := 1.7.7\n'
            'BUILDX_LOCAL_BUILDER := fixture\nBUILDX_OUTPUT := --load\nVERSION := fixture\nRCODER_VERSION := fixture\n'
            f'include {self.peer}/makefiles/02-build.mk\ninclude {self.peer}/makefiles/16-app-runtime.mk\n'
            f'include {self.peer}/makefiles/22-buildx-cluster.mk\n'
            f'APP_RUNTIME_CONFIG_PATH := {root}/runtime\n'
            '.PHONY: ' + ' '.join(prep) + '\n'
        )
        for name in prep:
            makefile += name + ':\n\t@echo ' + name + ' >> "$(CALL_LOG)"\n'
        (root / 'Makefile').write_text(makefile)
        environment = {**os.environ, 'PATH': str(bin_dir) + os.pathsep + os.environ['PATH'],
                       'CALL_LOG': str(root / 'calls')}
        return root, environment

    def build(self, target='build-agent-runner-amd64', overrides=(), missing_authority=False):
        root, environment = self.fixture(overrides, missing_authority)
        result = subprocess.run(['make', '-j4', target, *overrides], cwd=root, env=environment,
                                capture_output=True, text=True, timeout=60)
        calls = (root / 'calls').read_text().splitlines() if (root / 'calls').exists() else []
        return result, calls

    def test_invalid_node_fails_before_expensive_preparation(self):
        result, calls = self.build(overrides=['NODE_RUNTIME_VERSION=24.1.0'])
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('Node must remain', result.stderr)
        self.assertEqual(calls, [])

    def test_download_override_fails_before_expensive_preparation(self):
        result, calls = self.build(overrides=['PINGAP_DL_VERSION=0.99.9'])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('download version', result.stderr)
        self.assertEqual(calls, [])

    def test_missing_selected_authority_is_actionable(self):
        result, calls = self.build(missing_authority=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('authority missing', result.stderr)
        self.assertEqual(calls, [])

    def test_aggregate_gate_precedes_both_architecture_preparation(self):
        result, calls = self.build('build-agent-runner', ['NODE_RUNTIME_VERSION=24.1.0'])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])

    def test_agent_build_uses_snapshot_and_preserves_extra_asset(self):
        result, calls = self.build()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(sum(call.startswith('docker-build ') for call in calls), 1)
        self.assertTrue(any(call.startswith('build-dbx-fork') for call in calls))

    def test_runtime_base_build_uses_snapshot_and_exact_runtime_args(self):
        result, calls = self.build('build-app-runtime-base-amd64')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        builds = [call for call in calls if call.startswith('docker-build ')]
        self.assertEqual(len(builds), 1)
        self.assertIn('NODE_RUNTIME_VERSION=22.23.2', builds[0])
        self.assertIn('GO_VERSION=1.26.4', builds[0])

    def test_stale_reference_cannot_override_requested_node_version(self):
        result, calls = self.build('build-rcoder-base-amd64', ['NODE_RUNTIME_VERSION=22.24.0'])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('22.24.0', result.stderr)
        self.assertIn('22.23.2', result.stderr)
        self.assertFalse(any(call.startswith('docker-build ') for call in calls))

    def test_base_and_mcp_invalid_node_gate_precedes_every_preparation(self):
        for target in ['build-rcoder-base-amd64', 'build-agent-runner-base-amd64', 'build-mcp-proxy-amd64',
                       'build-rcoder-base', 'build-agent-runner-base', 'build-mcp-proxy', 'build-base-local']:
            with self.subTest(target=target):
                result, calls = self.build(target, ['NODE_RUNTIME_VERSION=24.1.0'])
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('Node must remain', result.stderr)
                self.assertEqual(calls, [])

    def test_base_and_mcp_snapshots_need_no_pingap_authority(self):
        for target, required in [('build-rcoder-base-amd64', ['node', 'ttyd']),
                                 ('build-agent-runner-base-amd64', ['node', 'ttyd', 'go']),
                                 ('build-mcp-proxy-amd64', ['node', 'deno'])]:
            with self.subTest(target=target):
                result, calls = self.build(target, missing_authority=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                builds = [call for call in calls if call.startswith('docker-build ')]
                self.assertEqual(len(builds), 1)
                self.assertIn('NODE_RUNTIME_VERSION=22.23.2', builds[0])
                self.assertFalse(any(call.startswith('download-pingap') or call.startswith('build-dbx') for call in calls))

    def test_cluster_component_gates_precede_remote_preparation(self):
        for target in ['cluster-build-base', 'cluster-build-agent-runner-base', 'cluster-build-mcp-proxy',
                       'cluster-build-agent-runner', 'cluster-build-app-runtime-base',
                       'cluster-build-app-runtime', 'cluster-build', 'cluster-dev']:
            with self.subTest(target=target):
                result, calls = self.build(target, ['NODE_RUNTIME_VERSION=24.1.0'])
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('Node must remain', result.stderr)
                self.assertEqual(calls, [])

    def test_cluster_recipes_consume_checked_snapshots(self):
        for target in ['cluster-build-base', 'cluster-build-agent-runner-base', 'cluster-build-mcp-proxy',
                       'cluster-build-agent-runner', 'cluster-build-app-runtime-base']:
            with self.subTest(target=target):
                result, calls = self.build(target, ['CLUSTER_OUTPUT=--load'])
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(sum(call.startswith('docker-build ') for call in calls), 1)

    def test_concurrent_base_recipes_keep_each_callers_assets(self):
        first, first_env = self.fixture(missing_authority=True)
        second, second_env = self.fixture(missing_authority=True)
        entry = second / 'entries/node'
        files = {}
        for cpu in ['x64', 'arm64']:
            old = entry / ('cache/node-v22.23.2-linux-' + cpu + '.tar.gz')
            replacement = entry / ('cache/node-v22.24.0-linux-' + cpu + '.tar.gz')
            old.unlink()
            replacement.write_bytes(b'fixture node second caller')
            files[str(replacement.relative_to(entry))] = hashlib.sha256(replacement.read_bytes()).hexdigest()
        manifest = {'protocol': 'runtime-assets-v1',
                    'identity': {'component': 'node', 'version': '22.24.0'}, 'files': files}
        content = json.dumps(manifest).encode()
        (entry / 'manifest.json').write_bytes(content)
        (second / 'refs/node.ref').write_text(json.dumps({
            'entry': str(entry), 'manifest_sha256': hashlib.sha256(content).hexdigest()}))
        first_env['EXPECTED_NODE_CONTENT'] = 'fixture node'
        second_env['EXPECTED_NODE_CONTENT'] = 'fixture node second caller'
        target = 'build-rcoder-base-amd64'
        commands = [(['make', '-j4', target, 'NODE_RUNTIME_VERSION=22.23.2'], first, first_env),
                    (['make', '-j4', target, 'NODE_RUNTIME_VERSION=22.24.0',
                      'RCODER_CONFIG_PATH=' + str(first / 'rcoder')], second, second_env)]
        processes = [subprocess.Popen(command, cwd=root, env=environment,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                     for command, root, environment in commands]
        contexts = []
        for process, (_, root, _) in zip(processes, commands):
            output, error = process.communicate(timeout=60)
            self.assertEqual(process.returncode, 0, output + error)
            builds = [line for line in (root / 'calls').read_text().splitlines()
                      if line.startswith('docker-build ')]
            self.assertEqual(len(builds), 1)
            arguments = json.loads(builds[0].split(' ', 1)[1])
            contexts.append(arguments[-2] if arguments[-1] == '--load' else arguments[-1])
        self.assertNotEqual(contexts[0], contexts[1])


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--peer-root', type=Path, required=True)
    parser.add_argument('--baseline-only', action='store_true')
    arguments = parser.parse_args()
    ProductionBuildTests.peer = arguments.peer_root.resolve()
    suite = unittest.TestSuite([ProductionBuildTests('test_invalid_node_fails_before_expensive_preparation')]) if arguments.baseline_only else unittest.defaultTestLoader.loadTestsFromTestCase(ProductionBuildTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
