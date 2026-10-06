"""DBX cache and real Make-entry regressions; Docker is a recording fixture."""
import os
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
HELPER = REPO / 'tools/build/dbx_cache.py'

FAKE_DOCKER = r'''#!/usr/bin/env python3
import hashlib, json, os, pathlib, re, subprocess, sys, time
args = sys.argv[1:]
state = pathlib.Path(os.environ['DOCKER_STATE'])
state.mkdir(exist_ok=True)
with (state / 'calls').open('a') as log:
    log.write(json.dumps(args) + '\n')
if args[:3] == ['buildx', 'imagetools', 'inspect']:
    print(json.dumps('sha256:' + hashlib.sha256((args[3] + os.environ.get('BASE_VERSION', '1')).encode()).hexdigest()))
elif args[0] == 'version':
    print('linux/arm64')
elif args[:2] == ['buildx', 'inspect']:
    print('Name: fixture\nDriver: docker\nEndpoint: fixture\nBuildKit version: v1\nPlatforms: linux/arm64, linux/amd64')
elif args[:2] == ['buildx', 'build']:
    source = pathlib.Path.cwd()
    dockerfile = (source / 'deploy/Dockerfile').read_text()
    assert all('@sha256:' in line for line in dockerfile.splitlines() if line.startswith('FROM '))
    assert not (source / '.git').exists(), 'build used mutable working tree'
    assert 'pnpm@10.27.0' in dockerfile
    assert re.search(r'ziglang==[0-9]+\.[0-9]+\.[0-9]+', dockerfile), 'ziglang was not actually pinned'
    assert re.search(r'cargo install --locked --version [0-9]+\.[0-9]+\.[0-9]+ cargo-zigbuild', dockerfile), 'cargo-zigbuild was not actually pinned'
    arch = args[args.index('--platform') + 1].split('/')[1]
    image = 'sha256:' + hashlib.sha256(args[args.index('--tag') + 1].encode()).hexdigest()
    marker = (source / 'marker.txt').read_text()
    (state / image.replace(':', '_')).write_text(json.dumps({'arch': arch, 'marker': marker}))
    pathlib.Path(args[args.index('--iidfile') + 1]).write_text(image)
    mutation = os.environ.get('MUTATE_SOURCE')
    if mutation and arch == 'amd64':
        upstream = pathlib.Path(mutation)
        (upstream / 'marker.txt').write_text('v2')
        subprocess.run(['git', '-C', str(upstream), 'add', 'marker.txt'], check=True)
        subprocess.run(['git', '-C', str(upstream), 'commit', '-qm', 'during build'], check=True)
    time.sleep(float(os.environ.get('BUILD_DELAY', '0')))
elif args[0] == 'create':
    image = args[-1]
    assert image.startswith('sha256:') or '@sha256:' in image, 'extraction used a mutable tag'
    if image.startswith('sha256:'):
        info = json.loads((state / image.replace(':', '_')).read_text())
    else:
        info = {'arch': args[args.index('--platform') + 1].split('/')[1], 'marker': 'official'}
    cid = hashlib.sha256((image + info['arch']).encode()).hexdigest()
    (state / ('container-' + cid)).write_text(json.dumps(info))
    print(cid)
elif args[0] == 'cp':
    cid, source = args[1].split(':', 1)
    info = json.loads((state / ('container-' + cid)).read_text())
    output = pathlib.Path(args[2])
    if source == '/app/static':
        if os.environ.get('FAIL_STATIC'):
            print('injected static extraction failure', file=sys.stderr)
            sys.exit(17)
        output.mkdir()
        (output / 'index.html').write_text('frontend ' + info['marker'])
    else:
        if os.environ.get('FAIL_BINARY'):
            sys.exit(18)
        header = bytearray(64)
        header[:6] = b'\x7fELF\x02\x01'
        header[18:20] = (62 if info['arch'] == 'amd64' else 183).to_bytes(2, 'little')
        output.write_bytes(header + b'ensure-local-pg ' + info['marker'].encode())
elif args[0] == 'rm':
    (state / ('container-' + args[-1])).unlink()
elif args[0] == 'pull':
    pass
else:
    raise RuntimeError(args)
'''


class CacheProtocolTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / 'origin'
        (self.source / 'deploy').mkdir(parents=True)
        (self.source / 'deploy/Dockerfile').write_text(
            'FROM node:22-slim AS frontend\nRUN npm i -g pnpm\n'
            'FROM rust:1-bookworm AS backend\n'
            'RUN pip3 install --break-system-packages -i ${PIP_INDEX_URL} ziglang\n'
            'RUN cargo install cargo-zigbuild && rustup target add x86_64-unknown-linux-gnu\n'
            'FROM debian:bookworm-slim\n'
        )
        (self.source / 'package.json').write_text('{"packageManager":"pnpm@10.27.0"}')
        (self.source / 'marker.txt').write_text('v1')
        self.git('init', '-q')
        self.git('config', 'user.name', 'fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture')
        self.branch = subprocess.check_output(['git', '-C', str(self.source), 'branch', '--show-current'], text=True).strip()
        binary = self.root / 'bin'
        binary.mkdir()
        (binary / 'docker').write_text(FAKE_DOCKER)
        (binary / 'docker').chmod(0o755)
        self.env = {**os.environ, 'PATH': str(binary) + os.pathsep + os.environ['PATH'],
                    'DOCKER_STATE': str(self.root / 'docker-state')}
        self.cache = self.root / 'cache'
        self.context = self.root / 'context/downloads'
        self.ref = self.root / 'request.json'

    def git(self, *args):
        subprocess.run(['git', '-C', str(self.source), *args], check=True, capture_output=True)

    def command(self, context=None, reference=None, *extra):
        return ['python3', str(HELPER), 'build', '--root', str(self.cache),
                '--repo', str(self.source), '--ref', self.branch,
                '--ziglang-version', '0.16.0', '--cargo-zigbuild-version', '0.23.4',
                '--context', str(context or self.context), '--output-ref', str(reference or self.ref), *extra]

    def build(self, *extra, env=None):
        result = subprocess.run(self.command(None, None, *extra), env={**self.env, **(env or {})},
                                capture_output=True, text=True, timeout=30)
        return result

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def entry(self, reference=None):
        ref = json.loads((reference or self.ref).read_text())
        entry = Path(ref['entry'])
        self.assertEqual(ref['manifest_sha256'], hashlib.sha256((entry / 'manifest.json').read_bytes()).hexdigest())
        return entry

    def builds(self):
        calls = (self.root / 'docker-state/calls').read_text().splitlines()
        return [json.loads(line) for line in calls if json.loads(line)[:2] == ['buildx', 'build']]

    def test_verified_cache_reused_with_original_generation(self):
        self.assert_success(self.build())
        first = self.entry()
        self.assert_success(self.build())
        self.assertEqual(self.entry(), first)
        self.assertEqual(len(self.builds()), 2)
        self.assertIn(b'v1', (self.context / 'dbx-web-amd64').read_bytes())

    def test_source_toolchain_and_build_arg_changes_get_distinct_generations(self):
        self.assert_success(self.build())
        first = self.entry()
        original = (first / 'manifest.json').read_bytes()
        self.assert_success(self.build('--pip-index', 'https://packages.example.invalid/simple'))
        second = self.entry()
        self.assertNotEqual(first, second)
        self.assert_success(self.build(env={'BASE_VERSION': '2'}))
        third = self.entry()
        self.assertNotIn(third, [first, second])
        (self.source / 'marker.txt').write_text('v3')
        self.git('add', '.')
        self.git('commit', '-qm', 'new input')
        self.assert_success(self.build())
        self.assertNotIn(self.entry(), [first, second, third])
        self.assertEqual((first / 'manifest.json').read_bytes(), original)

    def test_resolved_tool_version_change_cannot_reuse_the_same_key(self):
        spec = importlib.util.spec_from_file_location('dbx_toolchain_fixture', HELPER)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        source1 = self.root / 'source-tool-1'
        source2 = self.root / 'source-tool-2'
        for source in [source1, source2]:
            (source / 'deploy').mkdir(parents=True)
            (source / 'deploy/Dockerfile').write_text(
                'FROM node:22-slim AS frontend\nRUN npm i -g pnpm\n'
                'FROM rust:1-bookworm AS backend\n'
                'RUN pip3 install --break-system-packages -i ${PIP_INDEX_URL} ziglang\n'
                'RUN cargo install cargo-zigbuild && rustup target add x86_64-unknown-linux-gnu\n'
                'FROM debian:bookworm-slim\n')
            (source / 'package.json').write_text('{"packageManager":"pnpm@10.27.0"}')
        args = module.argparse.Namespace(repo='https://example.invalid/dbx.git', pip_index='https://example.invalid/pypi',
                                         builder='fixture', ziglang_version='0.16.0', cargo_zigbuild_version='0.23.4')
        def response(command, **kwargs):
            if command[:3] == ['docker', 'buildx', 'imagetools']:
                return '"sha256:' + 'a' * 64 + '"'
            if command[:3] == ['docker', 'buildx', 'inspect']:
                return 'Name: fixture\nDriver: docker\nEndpoint: fixture\nBuildKit version: v1\nPlatforms: linux/arm64, linux/amd64'
            return 'linux/arm64'
        with mock.patch.object(module, 'run', side_effect=response):
            first = module.resolve_inputs(args, source1, 'b' * 40)
            args.ziglang_version = '0.15.2'
            second = module.resolve_inputs(args, source2, 'b' * 40)
        self.assertNotEqual(module.digest(module.canonical(first)), module.digest(module.canonical(second)),
                            'different cross-compile tool versions shared one DBX cache key')
        self.assertIn('ziglang==0.16.0', (source1 / 'deploy/Dockerfile').read_text())
        self.assertIn('cargo install --locked --version 0.23.4 cargo-zigbuild', (source1 / 'deploy/Dockerfile').read_text())
        self.assertEqual(first['toolchains'], {'ziglang': '0.16.0', 'cargo-zigbuild': '0.23.4'})

    def test_exact_cross_compile_tool_versions_are_pinned_and_part_of_key(self):
        self.assert_success(self.build('--ziglang-version', '0.16.0', '--cargo-zigbuild-version', '0.23.4'))
        first = self.entry()
        first_manifest = json.loads((first / 'manifest.json').read_text())
        self.assertEqual(first_manifest['inputs']['toolchains'], {'ziglang': '0.16.0', 'cargo-zigbuild': '0.23.4'})
        self.assert_success(self.build('--ziglang-version', '0.15.2', '--cargo-zigbuild-version', '0.23.4'))
        second = self.entry()
        self.assertNotEqual(first, second)
        self.assert_success(self.build('--ziglang-version', '0.16.0', '--cargo-zigbuild-version', '0.23.3'))
        self.assertNotIn(self.entry(), [first, second])
        self.assertEqual(json.loads((first / 'manifest.json').read_text()), first_manifest)

    def test_non_exact_tool_version_fails_before_any_docker_call(self):
        result = self.build('--ziglang-version', 'latest')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('ziglang needs an exact', result.stderr)
        self.assertFalse(self.ref.exists())
        self.assertFalse((self.root / 'docker-state/calls').exists())

    def test_changed_install_shape_fails_instead_of_recording_unpinned_tools(self):
        dockerfile = self.source / 'deploy/Dockerfile'
        dockerfile.write_text(dockerfile.read_text().replace('RUN cargo install cargo-zigbuild && rustup target add x86_64-unknown-linux-gnu\n', ''))
        self.git('add', '.')
        self.git('commit', '-qm', 'unsupported tool installation')
        result = self.build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('one recognized ziglang and cargo-zigbuild', result.stderr)
        self.assertFalse(self.ref.exists())
        self.assertFalse((self.root / 'docker-state/calls').exists())

    def test_failed_extraction_keeps_valid_cache_and_distribution(self):
        self.assert_success(self.build())
        previous = self.ref.read_bytes()
        files = {name: (self.context / name).read_bytes() for name in ['dbx-web-amd64', '.dbx-manifest.json']}
        result = self.build('--force', env={'FAIL_STATIC': '1'})
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('✅', result.stdout)
        self.assertEqual(self.ref.read_bytes(), previous)
        for name, content in files.items():
            self.assertEqual((self.context / name).read_bytes(), content)
        self.assertEqual(len(list((self.cache / 'v2/entries').iterdir())), 1)
        self.assertFalse(list((self.root / 'docker-state').glob('container-*')))

    def test_damaged_binary_rebuilds_without_reusing_marker_only(self):
        self.assert_success(self.build())
        first = self.entry()
        with (first / 'dbx-web-amd64').open('ab') as stream:
            stream.write(b'corrupt but still has ensure-local-pg')
        self.assert_success(self.build())
        self.assertNotEqual(self.entry(), first)
        self.assertEqual(len(self.builds()), 4)
        self.assertNotIn(b'corrupt', (self.context / 'dbx-web-amd64').read_bytes())

    def test_two_callers_build_one_pair_and_receive_same_immutable_entry(self):
        reference2 = self.root / 'second.json'
        process1 = subprocess.Popen(self.command(), env={**self.env, 'BUILD_DELAY': '0.2'}, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        process2 = subprocess.Popen(self.command(self.root / 'other/downloads', reference2), env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        output1 = process1.communicate(timeout=30)
        output2 = process2.communicate(timeout=30)
        self.assertEqual(process1.returncode, 0, output1)
        self.assertEqual(process2.returncode, 0, output2)
        self.assertEqual(self.entry(), self.entry(reference2))
        self.assertEqual(len(self.builds()), 2)
        lock_files = list((self.cache / 'v2/locks').glob('*.lock'))
        identities = {path: path.stat().st_ino for path in lock_files}
        self.assert_success(self.build())
        self.assertEqual(identities, {path: path.stat().st_ino for path in lock_files})

    def test_remote_update_during_build_does_not_change_captured_source(self):
        self.assert_success(self.build(env={'MUTATE_SOURCE': str(self.source)}))
        entry = self.entry()
        for arch in ['amd64', 'arm64']:
            self.assertIn(b'v1', (entry / ('dbx-web-' + arch)).read_bytes())
            self.assertNotIn(b'v2', (entry / ('dbx-web-' + arch)).read_bytes())
        self.assert_success(self.build())
        self.assertIn(b'v2', (self.entry() / 'dbx-web-amd64').read_bytes())

    def test_force_build_creates_new_generation_without_replacing_old(self):
        self.assert_success(self.build())
        first = self.entry()
        old = (first / 'dbx-web-amd64').read_bytes()
        self.assert_success(self.build('--force'))
        self.assertNotEqual(self.entry(), first)
        self.assertEqual((first / 'dbx-web-amd64').read_bytes(), old)

    def test_copy_failure_rolls_back_all_published_components(self):
        spec = importlib.util.spec_from_file_location('dbx_cache_fixture', HELPER)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assert_success(self.build())
        entry = self.entry()
        binary = (self.context / 'dbx-web-amd64').read_bytes()
        manifest = (self.context / '.dbx-manifest.json').read_bytes()
        with mock.patch.object(module.shutil, 'copytree', side_effect=OSError('injected copy failure')):
            with self.assertRaises(OSError):
                module.publish_contexts(entry, [self.context], self.cache / 'v2')
        self.assertEqual((self.context / 'dbx-web-amd64').read_bytes(), binary)
        self.assertEqual((self.context / '.dbx-manifest.json').read_bytes(), manifest)

    def test_publish_failure_rolls_back_assets_and_manifest_in_every_context(self):
        spec = importlib.util.spec_from_file_location('dbx_cache_rollback_fixture', HELPER)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assert_success(self.build())
        entry = self.entry()
        second = self.root / 'second/downloads'
        module.publish_contexts(entry, [second], self.cache / 'v2')
        snapshots = {context: {name: (context / name).read_bytes()
                              for name in ['dbx-web-amd64', '.dbx-manifest.json']}
                     for context in [self.context, second]}
        original = Path.rename

        def fail_publication(path, destination):
            if Path(destination) == second.resolve() / 'dbx-web-arm64' and '.dbx-stage-' in str(path):
                raise OSError('injected publication failure')
            return original(path, destination)

        with mock.patch.object(Path, 'rename', fail_publication):
            with self.assertRaises(OSError):
                module.publish_contexts(entry, [self.context, second], self.cache / 'v2')
        for context, files in snapshots.items():
            for name, content in files.items():
                self.assertEqual((context / name).read_bytes(), content)

    def test_legacy_stage_and_stamp_do_not_skip_v2_build(self):
        legacy = self.cache / 'stage'
        legacy.mkdir(parents=True)
        (legacy / 'dbx-web-amd64').write_text('ensure-local-pg legacy binary')
        (self.cache / 'fork.stamp').write_text('test legacy source')
        self.assert_success(self.build())
        self.assertEqual(len(self.builds()), 2)
        self.assertEqual((legacy / 'dbx-web-amd64').read_text(), 'ensure-local-pg legacy binary')
        self.assertEqual((self.cache / 'fork.stamp').read_text(), 'test legacy source')

    def test_credentials_do_not_appear_in_manifest_or_output(self):
        secret = 'dbx-private-password'
        result = self.build('--pip-index', 'https://builder:' + secret + '@packages.example.invalid/simple?token=private-token')
        self.assert_success(result)
        manifest = (self.entry() / 'manifest.json').read_text()
        self.assertNotIn(secret, manifest + result.stdout + result.stderr)
        self.assertNotIn('private-token', manifest + result.stdout + result.stderr)

    def test_corrupt_source_mirror_rebuilds_without_deleting_lock_inode(self):
        self.assert_success(self.build())
        source_id = hashlib.sha256(str(self.source).encode()).hexdigest()
        mirror = self.cache / 'v2/sources' / (source_id + '.git')
        (mirror / 'HEAD').write_text('damaged metadata')
        source_lock = self.cache / 'v2/locks' / ('source-' + source_id + '.lock')
        identity = source_lock.stat().st_ino
        self.assert_success(self.build())
        self.assertEqual(source_lock.stat().st_ino, identity)
        self.assertEqual(len(self.builds()), 2)
        self.assertTrue(list(mirror.parent.glob(source_id + '.git.invalid-*')))

    def test_real_make_build_entry_emits_immutable_reference(self):
        result = subprocess.run([
            'make', '-f', str(REPO / 'make/dbx.mk'), 'build-dbx-fork',
            f'DBX_PERSIST_ROOT={self.cache}', f'DBX_FORK_REPO={self.source}',
            f'DBX_FORK_BRANCH={self.branch}', f'DBX_CONTEXTS={self.context}',
            f'DBX_OUTPUT_REF={self.ref}',
        ], env=self.env, capture_output=True, text=True, timeout=30)
        self.assert_success(result)
        self.assertTrue((self.entry() / 'manifest.json').is_file())

    def test_official_and_fork_entries_have_separate_identity(self):
        self.assert_success(self.build())
        fork = self.entry()
        result = subprocess.run(['python3', str(HELPER), 'official', '--root', str(self.cache),
                                 '--context', str(self.context), '--output-ref', str(self.ref)],
                                env=self.env, capture_output=True, text=True, timeout=30)
        self.assert_success(result)
        self.assertNotEqual(self.entry(), fork)
        self.assertEqual(json.loads((self.entry() / 'manifest.json').read_text())['inputs']['kind'], 'official-image')
        self.assert_success(self.build())
        self.assertEqual(self.entry(), fork)


class DistributionRegressionTests(unittest.TestCase):
    def test_missing_binary_never_claims_distribution_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = root / 'stage'
            (stage / 'dbx-static').mkdir(parents=True)
            (stage / 'dbx-static/index.html').write_text('valid static')
            context = root / 'downloads'
            context.mkdir()
            (context / 'dbx-web-amd64').write_text('previous valid binary')
            result = subprocess.run(
                ['make', '-f', str(REPO / 'make/dbx.mk'), 'distribute-dbx-stage',
                 f'DBX_STAGE={stage}', f'DBX_CONTEXTS={context}'],
                capture_output=True, text=True, timeout=20,
            )
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertNotIn('✅', result.stdout)
            self.assertEqual((context / 'dbx-web-amd64').read_text(), 'previous valid binary')


if __name__ == '__main__':
    unittest.main()
