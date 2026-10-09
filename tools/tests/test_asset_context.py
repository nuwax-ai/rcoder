import importlib.util
import concurrent.futures
import json
import hashlib
import shutil
from pathlib import Path
import subprocess
import tempfile
import unittest

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
        for relative in ('make/docker.mk', 'docker/build-app-runtime.py', 'crates/app-cli/Cargo.toml', 'crates/app-cli/src/build_deploy/devtool.rs'):
            target = root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((ROOT / relative).read_bytes())

    def run_gate(self, root, *extra):
        return subprocess.run(['python3', str(ROOT / 'k8s/scripts/pingap_version_gate.py'), '--repo-root', str(root), *extra], capture_output=True, text=True)

    def test_local_is_self_contained_cross_repo_is_explicit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'rcoder'
            self.fixture(root)
            self.assertEqual(self.run_gate(root).returncode, 0)
            self.assertNotEqual(self.run_gate(root, '--cross-repo').returncode, 0)

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


if __name__ == '__main__':
    unittest.main()
