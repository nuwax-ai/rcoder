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
import uuid

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


class TestChatFailureReporting(unittest.TestCase):
    """T04：启动器非零退出时失败用例仍进 context['cases']，再上抛失败。"""

    def _context(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        return {'snapshot_path': root, 'snapshot_manifest_path': root / 'inputs.json',
                'origin_head': 'head', 'report_dir': root}

    def test_failure_parses_cases_then_reraises(self):
        context = self._context()

        class C:
            values = {}
            state = Path(self.tmp.name) / 'state'
            host = ns = context = id = 'stub'
            nodeport = 1

            def get(self, key, required=False):
                return ''

        def fake_run(args, **kwargs):
            (context['report_dir'] / 'chat.log').write_text(
                'pass: k8s_lb::chat_ok\nfail: k8s_lb::chat_bad\n')
            raise RuntimeError('python3 exited 1: boom')

        env = {'LLM_API_KEY': 'k', 'LLM_MODEL': 'm', 'LLM_BASE_URL': 'u'}
        with patch.dict(os.environ, env), patch.object(main, 'run', fake_run):
            with self.assertRaises(RuntimeError):
                main.run_chat_suite(C(), {'url': 'http://127.0.0.1:1'}, context)
        self.assertEqual(context['cases'], [
            {'name': 'chat_ok', 'verdict': 'pass'},
            {'name': 'chat_bad', 'verdict': 'fail'}])

    def test_infrastructure_failure_yields_empty_cases_not_fake_ones(self):
        context = self._context()

        class C:
            values = {}
            state = Path(self.tmp.name) / 'state'
            host = ns = context = id = 'stub'
            nodeport = 1

            def get(self, key, required=False):
                return ''

        def fake_run(args, **kwargs):
            (context['report_dir'] / 'chat.log').write_text('cargo build failed')
            raise RuntimeError('python3 exited 101: build failed')

        env = {'LLM_API_KEY': 'k', 'LLM_MODEL': 'm', 'LLM_BASE_URL': 'u'}
        with patch.dict(os.environ, env), patch.object(main, 'run', fake_run):
            with self.assertRaises(RuntimeError):
                main.run_chat_suite(C(), {'url': 'http://127.0.0.1:1'}, context)
        self.assertEqual(context['cases'], [])


class TestExecuteSuitesPostVerify(unittest.TestCase):
    """T03：套件执行后必须再次校验冻结快照——被篡改输入上的结果不得记 pass。"""

    def test_verify_runs_before_and_after(self):
        calls = []

        def fake_verify(record):
            calls.append(record)

        context = {'snapshot_record': {'r': 1}}

        def fake_userapp(c, receipt, ctx):
            calls.append('ran')

        with patch.object(main.test_snapshot, 'verify', fake_verify), \
                patch.object(main, 'run_userapp_suite', fake_userapp):
            main.execute_suites(None, {}, context, 'userapp')
        self.assertEqual([str(x) for x in calls], ["{'r': 1}", 'ran', "{'r': 1}"])

    def test_post_verify_failure_blocks_pass(self):
        context = {'snapshot_record': {}}
        state = {'verify_calls': 0}

        def fake_verify(record):
            state['verify_calls'] += 1
            if state['verify_calls'] > 1:
                raise RuntimeError('Frozen test snapshot was modified: content mismatch')

        with patch.object(main.test_snapshot, 'verify', fake_verify), \
                patch.object(main, 'run_userapp_suite', lambda *a: None):
            with self.assertRaises(RuntimeError):
                main.execute_suites(None, {}, context, 'userapp')
        self.assertEqual(state['verify_calls'], 2)


class TestStructuredChatCases(unittest.TestCase):
    """T04：chat 用例优先消费严格启动器落盘的结构化 summary。"""

    def test_parses_structured_results_and_missing_reports_yield_none(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            context = {'report_dir': root}
            self.assertIsNone(main._structured_chat_cases(context))
            run_dir = root / 'e2e-reports' / 'run-1'
            run_dir.mkdir(parents=True)
            (run_dir / 'summary.json').write_text(json.dumps({
                'run_id': 'run-1',
                'results': [{'suite': 'k8s_lb', 'test': 'chat_alpha', 'verdict': 'fail',
                             'errors': ['libtest exit 1']},
                            {'suite': 'k8s_lb', 'test': 'chat_beta', 'verdict': 'pass'},
                            {'suite': 'other', 'test': 'noise', 'verdict': 'pass'}]}))
            rows = main._structured_chat_cases(context)
            self.assertEqual([r['name'] for r in rows], ['chat_alpha', 'chat_beta'])
            self.assertEqual(rows[0]['verdict'], 'fail')
            self.assertEqual(rows[0]['errors'], ['libtest exit 1'])


class TestExecuteSuitesFailurePathVerify(unittest.TestCase):
    """Q05：失败/取消路径也执行末尾快照校验，双错误不互相掩盖。"""

    def test_suite_failure_with_clean_snapshot_reraises_original(self):
        context = {'snapshot_record': {}}
        original = RuntimeError('business failure')

        def failing_userapp(c, receipt, ctx):
            raise original

        with patch.object(main.test_snapshot, 'verify', lambda record: None), \
                patch.object(main, 'run_userapp_suite', failing_userapp):
            with self.assertRaises(RuntimeError) as caught:
                main.execute_suites(None, {}, context, 'userapp')
        self.assertIs(caught.exception, original)

    def test_suite_failure_with_tampered_snapshot_reports_both(self):
        context = {'snapshot_record': {}}
        state = {'calls': 0}

        def failing_userapp(c, receipt, ctx):
            raise RuntimeError('business failure')

        def verify(record):
            state['calls'] += 1
            if state['calls'] > 1:
                raise RuntimeError('content mismatch in snapshot')

        with patch.object(main.test_snapshot, 'verify', verify), \
                patch.object(main, 'run_userapp_suite', failing_userapp):
            with self.assertRaises(RuntimeError) as caught:
                main.execute_suites(None, {}, context, 'userapp')
        message = str(caught.exception)
        self.assertIn('business failure', message)
        self.assertIn('content mismatch in snapshot', message)
        self.assertEqual(state['calls'], 2)


class TestRetestCaseVerdict(unittest.TestCase):
    """V01：整轮失败不得被 case pass 覆盖；run 身份显式传递。"""

    def _run(self, tests_behavior):
        calls = {}
        def fake_tests(c, suite, case='', frozen_snapshot=None, receipt_override=None):
            calls['args'] = (suite, case, frozen_snapshot, receipt_override)
            return tests_behavior()
        with patch.object(main, 'tests', fake_tests):
            return (*main.run_retest_case(None, {'snapshot_id': 's1'}, {'r': 1}, 'chat_alpha'), calls)

    def test_case_pass_with_full_run_pass_is_pass_and_binds_parent_receipt(self):
        outcome, _tid, _res, calls = self._run(lambda: ('tid1', {
            'verdict': 'pass', 'cases': [{'name': 'chat_alpha', 'verdict': 'pass'}]}))
        self.assertEqual(outcome['verdict'], 'pass')
        self.assertEqual(outcome['test_id'], 'tid1')
        self.assertIsNone(outcome.get('error'))
        # receipt 绑定父报告身份（tests 内 smoke/identity 按该身份校验）
        self.assertEqual(calls['args'][3], {'r': 1})
        self.assertEqual(calls['args'][1], 'chat_alpha')

    def test_case_pass_but_run_failed_stays_fail(self):
        # 整轮失败（快照/身份/outside 任一保护失败）不得被 case pass 覆盖
        def behavior():
            raise main.TestRunError('tid2', {
                'verdict': 'fail',
                'error': 'RuntimeError: deployment identity changed',
                'cases': [{'name': 'chat_alpha', 'verdict': 'pass'}]})
        outcome, _tid, _res, _calls = self._run(behavior)
        self.assertEqual(outcome['verdict'], 'fail')
        self.assertEqual(outcome['test_id'], 'tid2')
        self.assertIn('deployment identity changed', outcome['error'])

    def test_run_pass_but_case_failed_stays_fail(self):
        outcome, _tid, _res, _calls = self._run(lambda: ('tid3', {
            'verdict': 'pass', 'cases': [{'name': 'chat_alpha', 'verdict': 'fail'}]}))
        self.assertEqual(outcome['verdict'], 'fail')
        self.assertEqual(outcome['test_id'], 'tid3')

    def test_unexpected_exception_keeps_explicit_failure_without_test_id(self):
        def behavior():
            raise RuntimeError('report creation failed')
        outcome, _tid, _res, _calls = self._run(behavior)
        self.assertEqual(outcome['verdict'], 'fail')
        self.assertIsNone(outcome.get('test_id'))
        self.assertIn('report creation failed', outcome['error'])


class TestFrozenFingerprintContentSensitivity(unittest.TestCase):
    """T03：冻结模式下 launcher 指纹必须覆盖文件实际内容，而非仅清单 JSON。"""

    def _load_launcher(self, source_root, manifest_path):
        import importlib.util
        tools_dir = Path(__file__).resolve().parents[3] / 'tests-e2e' / 'tools'
        sys.path.insert(0, str(tools_dir))
        try:
            spec = importlib.util.spec_from_file_location(
                'strict_launcher_' + uuid.uuid4().hex[:8], tools_dir / 'run.py')
            module = importlib.util.module_from_spec(spec)
            env = {'E2E_SOURCE_ROOT': str(source_root),
                   'E2E_INPUT_MANIFEST': str(manifest_path),
                   'E2E_RUN_ROOT': str(source_root / 'reports')}
            with patch.dict(os.environ, env):
                spec.loader.exec_module(module)
            return module
        finally:
            sys.path.remove(str(tools_dir))

    def test_content_change_without_manifest_change_alters_fingerprint(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / 'a.txt').write_text('one')
            (root / 'b.txt').write_text('two')
            manifest = {'a.txt': {'kind': 'file', 'sha256': 'x', 'executable': False},
                        'b.txt': {'kind': 'file', 'sha256': 'y', 'executable': False}}
            manifest_path = root / 'inputs.json'
            manifest_path.write_text(json.dumps(manifest))
            module = self._load_launcher(root, manifest_path)
            first = module.source_fingerprint()
            (root / 'a.txt').write_text('changed')  # 清单不变、内容被改
            second = module.source_fingerprint()
            self.assertNotEqual(first, second, '文件内容变化必须改变冻结指纹')
            (root / 'a.txt').unlink()  # 条目消失
            third = module.source_fingerprint()
            self.assertNotEqual(second, third)


class TestDiagnosticsClassification(unittest.TestCase):
    """T07：ceph 健康、deployment ready、Forbidden 分类按真实条件判定。"""

    class C:
        id = 'env'
        context = 'stub-context'
        state = Path(tempfile.gettempdir()) / 'remote-k8s-test-state'

        def __init__(self, ssh_rows=None, deployment=None):
            self._ssh_rows = ssh_rows or ''
            self._deployment = deployment

        def ssh(self, args, timeout=None):
            return self._ssh_rows

        def obj(self, verb, *names):
            if self._deployment is not None and 'rcoder' in names:
                return self._deployment
            return {'items': []}

    def test_ceph_health_err_is_failure(self):
        import diagnostics
        row = diagnostics._ceph(TestDiagnosticsClassification.C(ssh_rows='HEALTH_ERR'))
        self.assertEqual(row['status'], 'fail')

    def test_ceph_health_ok_passes_and_empty_is_unknown(self):
        import diagnostics
        row = diagnostics._ceph(TestDiagnosticsClassification.C(ssh_rows='HEALTH_OK'))
        self.assertEqual(row['status'], 'pass')
        # Q06：空观察不能证明健康——unknown（no-observation），绝不当通过
        row = diagnostics._ceph(TestDiagnosticsClassification.C(ssh_rows=''))
        self.assertEqual(row['status'], 'unknown')
        self.assertEqual(row['error_class'], 'no-observation')

    def test_deployment_not_ready_is_failure(self):
        import diagnostics

        def make(ready, replicas):
            return TestDiagnosticsClassification.C(deployment={'metadata': {'uid': 'u', 'generation': 1},
                                                              'spec': {'replicas': replicas,
                                                                       'template': {'spec': {'containers': [{'image': 'i'}]}}},
                                                              'status': {'readyReplicas': ready}})

        row = diagnostics._deployment(make(0, 2))
        self.assertEqual(row['status'], 'fail')
        row = diagnostics._deployment(make(2, 2))
        self.assertEqual(row['status'], 'pass')

    def test_kubectl_forbidden_is_unknown_not_fail(self):
        import diagnostics

        @diagnostics._probe('stub-probe')
        def forbidden_probe(c):
            raise RuntimeError('Error from server (Forbidden): deployments.apps is forbidden')

        row = forbidden_probe(None)
        self.assertEqual(row['status'], 'unknown')
        self.assertEqual(row['error_class'], 'forbidden')


if __name__ == '__main__':
    unittest.main()
