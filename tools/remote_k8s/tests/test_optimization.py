"""remote-k8s 开发验证体验优化（R1-R4）的聚焦回归。

不触达真实集群：快照/缓存/校验逻辑用临时目录与桩 Config；Make 参数安全
用静态检查（recipe 不插值 shell）。真实集群验收见 verification.md。
"""
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_snapshot
import build_cache
import main


class FakeConfig:
    def __init__(self, root):
        self.id = 'testenv00000000'
        self.state = Path(root) / 'state'
        self.state.mkdir(parents=True, exist_ok=True)


class TestSnapshotFreezeVerify(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.config = FakeConfig(self.tmp.name)
        self.manifest = {
            'crates/a/Cargo.toml': {'kind': 'file', 'sha256': 'aa' * 32, 'executable': False},
            'tools/run.sh': {'kind': 'file', 'sha256': 'bb' * 32, 'executable': True},
            'link/inner': {'kind': 'link', 'target': '../crates/a/Cargo.toml'},
        }

    def tearDown(self):
        self.tmp.cleanup()

    def _write_snapshot(self, manifest=None, extra=None, mutate=None, escaping_link=False):
        import hashlib
        destination = self.config.state / 'test-snapshots' / 'sid1'
        (destination / 'source').mkdir(parents=True)
        manifest = {}
        for name, content, executable in [
            ('crates/a/Cargo.toml', b'toml-body', False),
            ('tools/run.sh', b'#!/bin/sh\n', True),
        ]:
            path = destination / 'source' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
            path.chmod(0o755 if executable else 0o644)
            manifest[name] = {'kind': 'file', 'sha256': hashlib.sha256(content).hexdigest(),
                              'executable': executable}
        link = destination / 'source' / 'link/inner'
        link.parent.mkdir(parents=True, exist_ok=True)
        target = '../../crates/a/Cargo.toml' if escaping_link else '../crates/a/Cargo.toml'
        link.symlink_to(target)
        manifest['link/inner'] = {'kind': 'link', 'target': target}
        if extra:
            (destination / 'source' / extra).write_text('rogue')
        if mutate:
            mutate(destination / 'source')
        (destination / 'inputs.json').write_text(json.dumps(manifest))
        identity = __import__('common').digest(manifest)
        (destination / 'snapshot.json').write_text(json.dumps(
            {'schema': 1, 'snapshot_id': 'sid1', 'environment': self.config.id,
             'status': 'sealed', 'source_sha256': identity,
             'files': len(manifest), 'path': str(destination)}))
        return {'snapshot_id': 'sid1', 'path': str(destination),
                'source_sha256': identity, 'status': 'sealed'}

    def test_verify_passes_on_untouched_copy(self):
        record = self._write_snapshot()
        self.assertTrue(test_snapshot.verify(record))

    def test_verify_rejects_unexpected_new_entry(self):
        record = self._write_snapshot(extra='rogue.py')
        with self.assertRaisesRegex(RuntimeError, 'unexpected entry'):
            test_snapshot.verify(record)

    def test_verify_rejects_deleted_entry(self):
        def delete(root):
            (root / 'tools/run.sh').unlink()
        record = self._write_snapshot(mutate=delete)
        with self.assertRaisesRegex(RuntimeError, 'missing snapshot entry'):
            test_snapshot.verify(record)

    def test_verify_rejects_content_change(self):
        def rewrite(root):
            (root / 'crates/a/Cargo.toml').write_bytes(b'y')
        record = self._write_snapshot(mutate=rewrite)
        with self.assertRaisesRegex(RuntimeError, 'content mismatch'):
            test_snapshot.verify(record)

    def test_verify_rejects_escaping_link(self):
        record = self._write_snapshot(escaping_link=True)
        with self.assertRaisesRegex(RuntimeError, 'escapes|mismatch'):
            test_snapshot.verify(record)

    def test_verify_rejects_extra_metadata(self):
        record = self._write_snapshot()
        (Path(record['path']) / 'rogue.json').write_text('{}')
        with self.assertRaisesRegex(RuntimeError, 'unexpected snapshot metadata'):
            test_snapshot.verify(record)


class TestBuildCache(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.config = FakeConfig(self.tmp.name)
        self.bases = {'RCODER_BASE': 'r@sha256:' + '1' * 64,
                      'COMPUTER_BASE': 'c@sha256:' + '2' * 64,
                      'RUNTIME_BASE': 'u@sha256:' + '3' * 64}

        class Cfg:
            state = self.config.state
            def get(self, name, default='', required=False):
                return {'RUST_IMAGE': 'rust:1.95-trixie', 'JOBS': '4',
                        'APT_MIRROR': 'http://deb.debian.org', 'CARGO_MIRROR': ''}.get(name, default)
        self.cfg = Cfg()

    def tearDown(self):
        self.tmp.cleanup()

    def test_key_changes_with_every_input_dimension(self):
        base = build_cache.cache_key('a' * 64, self.bases, self.cfg)
        self.assertNotEqual(build_cache.cache_key('b' * 64, self.bases, self.cfg), base, '源码变化必须 miss')
        changed_bases = dict(self.bases, RCODER_BASE='r@sha256:' + '9' * 64)
        self.assertNotEqual(build_cache.cache_key('a' * 64, changed_bases, self.cfg), base, '基础镜像变化必须 miss')
        with patch.object(self.cfg, 'get', side_effect=lambda n, d='', required=False:
                          {'RUST_IMAGE': 'rust:1.96-trixie'}.get(n, d)):
            self.assertNotEqual(build_cache.cache_key('a' * 64, self.bases, self.cfg), base, '工具链变化必须 miss')

    def test_load_rejects_incomplete_or_failed_receipts(self):
        key = build_cache.cache_key('a' * 64, self.bases, self.cfg)
        self.assertIsNone(build_cache.load(self.config, key), '无记录 → miss')
        good = {'key': key, 'status': 'built',
                'images': {t: 'reg/' + t + '@sha256:' + format(i, 'x') * 64
                           for i, t in enumerate(build_cache.TARGETS)}}
        __import__('common').atomic_json(self.config.state / 'cache' / 'builds' / (key + '.json'), good)
        self.assertEqual(build_cache.load(self.config, key)['key'], key)
        for broken in [
            dict(good, status='failed'),
            dict(good, images={t: 'reg/' + t + '@sha256:' + format(i, 'x') * 64
                                for i, t in enumerate(build_cache.TARGETS[:2])}),
            dict(good, images={**good['images'], 'rcoder': 'reg/rcoder:notadigest'}),
        ]:
            __import__('common').atomic_json(self.config.state / 'cache' / 'builds' / (key + '.json'), broken)
            self.assertIsNone(build_cache.load(self.config, key), str(broken)[:60])

    def test_artifact_mismatch_classified(self):
        class Ssh:
            def __init__(self, output=None, error=None):
                self.output, self.error = output, error
            def __call__(self, args, timeout=None):
                if self.error:
                    raise RuntimeError(self.error)
                return self.output
        ref = 'reg/rcoder@sha256:' + 'a' * 64
        cfg = type('C', (), {'ssh': Ssh(output='Digest: sha256:' + 'a' * 64)})()
        self.assertEqual(build_cache.artifact_matches(cfg, ref), (True, 'digest-mismatch'))
        cfg = type('C', (), {'ssh': Ssh(output='Digest: sha256:' + 'b' * 64)})()
        ok, error = build_cache.artifact_matches(cfg, ref)
        self.assertFalse(ok)
        self.assertEqual(error, 'digest-mismatch')
        cfg = type('C', (), {'ssh': Ssh(error='ssh exited 1: ... manifest unknown ...')})()
        self.assertEqual(build_cache.artifact_matches(cfg, ref), (False, 'artifact-missing'))
        cfg = type('C', (), {'ssh': Ssh(error='ssh exited 255: connection refused')})()
        self.assertEqual(build_cache.artifact_matches(cfg, ref), (False, 'registry-error'))


class TestCaseSelection(unittest.TestCase):
    def test_registered_chat_cases_nonempty(self):
        registered = main.registered_chat_cases()
        self.assertTrue(registered, 'k8s_lb 套件必须有注册场景')

    def test_chat_case_pattern_parses_launcher_output(self):
        output = '\n'.join(['pass: k8s_lb::alpha_case', 'fail: k8s_lb::beta_case',
                            'unrelated line', 'pass: other_suite::x'])
        cases = [{'name': n, 'verdict': v} for v, n in main.CHAT_CASE_PATTERN.findall(output)]
        self.assertEqual(cases, [{'name': 'alpha_case', 'verdict': 'pass'},
                                 {'name': 'beta_case', 'verdict': 'fail'}])


class TestMakeParameterSafety(unittest.TestCase):
    """Make 参数安全：recipe 不把 SUITE/CASE/RUN 插值进 shell 命令行。"""

    def test_recipe_has_no_shell_interpolated_params(self):
        recipe = (Path(__file__).resolve().parents[3] / 'make' / 'remote-k8s.mk').read_text()
        self.assertNotIn('--suite', recipe, '参数不得拼进命令行（经环境变量传递）')
        self.assertNotIn('"$(SUITE)"', recipe, '禁止双引号 shell 插值（$() 会在 recipe shell 执行）')
        self.assertIn('export REMOTE_K8S_SUITE', recipe, '参数必须经 make 解析期 export')

    def test_python_rejects_invalid_case_and_run(self):
        parser_cases = []
        import argparse
        for case, suite in [('unknown_case', 'chat'), ('has-dash', 'chat'), ('ok_case', 'userapp')]:
            with patch.object(sys, 'argv', ['main.py', 'test', '--suite', suite, '--case', case]):
                try:
                    main.main()
                    parser_cases.append('accepted')
                except SystemExit:
                    parser_cases.append('rejected')
        self.assertNotIn('accepted', parser_cases, '未知/非法/错套件 CASE 必须提前拒绝')


if __name__ == '__main__':
    unittest.main()
