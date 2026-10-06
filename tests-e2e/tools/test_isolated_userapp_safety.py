"""Harness safety checks only; no simulated UserApp or Docker business success."""
import ast
from contextlib import redirect_stderr, redirect_stdout
from copy import deepcopy
import io
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

TOOLS = Path(__file__).resolve().parent


class OwnedContainerCleanupTests(unittest.TestCase):
    def run_finalization(self, code, initial_success=True, failure=None):
        # Execute the actual script's final report/cleanup boundary without
        # invoking any of its business fixture. Only cleanup subprocess output
        # is controlled; this is not an E2E success substitute.
        tree = ast.parse((TOOLS / 'userapp_root_logs.py').read_text())
        main = next(node for node in tree.body
                    if isinstance(node, ast.FunctionDef) and node.name == 'main')
        guarded = next(node for node in main.body
                       if isinstance(node, ast.Try) and node.finalbody)
        boundary = ast.FunctionDef(
            name='finish',
            args=ast.arguments(posonlyargs=[], args=[], kwonlyargs=[],
                               kw_defaults=[], defaults=[]),
            body=deepcopy(guarded.finalbody) + [deepcopy(main.body[-1])],
            decorator_list=[],
        )
        module = ast.fix_missing_locations(ast.Module(body=[boundary], type_ignores=[]))
        calls = []

        def cleanup(*argv, **kwargs):
            calls.append((argv, kwargs))
            if failure:
                raise failure
            return subprocess.CompletedProcess(['docker', *argv], code, '', '')

        with tempfile.TemporaryDirectory() as folder:
            output = Path(folder) / 'report.json'
            report = {'success': initial_success, 'checks': []}
            scope = {'cid': 'exact-fixture-owned-container', 'report': report,
                     'args': SimpleNamespace(report=output), 'docker': cleanup,
                     'json': json}
            exec(compile(module, str(TOOLS / 'userapp_root_logs.py'), 'exec'), scope)
            with redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as raised:
                scope['finish']()
            self.assertEqual(calls, [(('rm', '-f', 'exact-fixture-owned-container'),
                                      {'check': False})])
            self.assertTrue(output.is_file(), 'cleanup failure must preserve the report')
            return raised.exception.code, json.loads(output.read_text())

    def test_cleanup_failure_cannot_publish_success(self):
        code, report = self.run_finalization(5)
        self.assertEqual(code, 1)
        self.assertIs(report['success'], False)
        self.assertIs(report['cleanup_ok'], False)

    def test_cleanup_exception_cannot_publish_success(self):
        code, report = self.run_finalization(
            0, failure=subprocess.TimeoutExpired(['docker', 'rm'], 1))
        self.assertEqual(code, 1)
        self.assertIs(report['success'], False)
        self.assertIs(report['cleanup_ok'], False)

    def test_cleanup_success_preserves_prior_business_failure(self):
        code, report = self.run_finalization(0, initial_success=False)
        self.assertEqual(code, 1)
        self.assertIs(report['success'], False)
        self.assertIs(report['cleanup_ok'], True)

    def test_cleanup_success_allows_completed_harness_result(self):
        code, report = self.run_finalization(0)
        self.assertEqual(code, 0)
        self.assertIs(report['success'], True)
        self.assertIs(report['cleanup_ok'], True)


class DockerEndpointAdmissionTests(unittest.TestCase):
    def require_endpoint(self, environment, output='unix:///known/local/docker.sock', code=0):
        from isolated_docker import require_local_docker_endpoint
        with patch.dict(os.environ, environment, clear=True), patch(
                'isolated_docker.subprocess.run', return_value=subprocess.CompletedProcess(
                    ['docker', 'context', 'inspect'], code, output, '')) as run:
            result = require_local_docker_endpoint()
            return result, run.call_args_list

    def test_explicit_unix_socket_needs_no_context_fallback(self):
        endpoint, calls = self.require_endpoint({'DOCKER_HOST': 'unix:///known/local/docker.sock'})
        self.assertEqual(endpoint, 'unix:///known/local/docker.sock')
        self.assertEqual(calls, [])

    def test_loopback_tcp_is_allowed(self):
        for host in ('localhost', '127.0.0.1', '127.1.2.3', '[::1]'):
            with self.subTest(host=host):
                endpoint, calls = self.require_endpoint({'DOCKER_HOST': 'tcp://' + host + ':2375'})
                self.assertEqual(endpoint, 'tcp://' + host + ':2375')
                self.assertEqual(calls, [])

    def test_remote_or_ambiguous_endpoints_are_rejected(self):
        for endpoint in ('ssh://host', 'tcp://192.0.2.3:2375', 'tcp://example.com:2375',
                         'unix://other-host/docker.sock', 'unix://relative', 'fd://3',
                         'tcp://localhost', 'tcp://localhost:2375/path',
                         'tcp://name:secret@localhost:2375'):
            with self.subTest(endpoint=endpoint), self.assertRaises(RuntimeError):
                self.require_endpoint({'DOCKER_HOST': endpoint})

    def test_selected_context_overrides_host_without_fallback(self):
        endpoint, calls = self.require_endpoint(
            {'DOCKER_CONTEXT': 'known-local', 'DOCKER_HOST': 'ssh://ignored-host'})
        self.assertEqual(endpoint, 'unix:///known/local/docker.sock')
        self.assertEqual(calls[0].args[0], [
            'docker', 'context', 'inspect', 'known-local', '--format',
            '{{(index .Endpoints "docker").Host}}'])
        with self.assertRaises(RuntimeError):
            self.require_endpoint({'DOCKER_CONTEXT': 'remote',
                                   'DOCKER_HOST': 'unix:///ignored/local.sock'},
                                  output='ssh://remote-host')

    def test_default_context_is_inspected(self):
        endpoint, calls = self.require_endpoint({})
        self.assertEqual(endpoint, 'unix:///known/local/docker.sock')
        self.assertEqual(calls[0].args[0], [
            'docker', 'context', 'inspect', '--format',
            '{{(index .Endpoints "docker").Host}}'])

    def test_context_inspection_failure_does_not_fallback(self):
        with self.assertRaises(RuntimeError):
            self.require_endpoint({}, code=1)

    def test_both_real_entrypoints_refuse_remote_before_any_resource_operation(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            app, proxy = root / 'app-cli', root / 'file-server-proxy'
            app.touch()
            proxy.touch()
            for script in ('app_cli_recovery', 'userapp_root_logs'):
                with self.subTest(script=script):
                    spec = importlib.util.spec_from_file_location(script, TOOLS / (script + '.py'))
                    module = importlib.util.module_from_spec(spec)
                    spec.loader.exec_module(module)
                    arguments = [script, '--app-cli', str(app), '--file-server-proxy', str(proxy),
                                 '--report', str(root / 'report.json')]
                    if script == 'userapp_root_logs':
                        arguments.extend(['--build-source', str(root / 'unused.json')])
                    with patch.dict(os.environ, {'DOCKER_HOST': 'ssh://remote'}, clear=True), \
                            patch.object(sys, 'argv', arguments), patch('subprocess.run') as run, \
                            redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
                        module.main()
                    self.assertEqual(raised.exception.code, 2)
                    run.assert_not_called()
                    self.assertFalse((root / 'report.json').exists())


class NativeOwnerScriptArgumentTests(unittest.TestCase):
    def test_captured_owner_scripts_are_valid_subprocess_arguments(self):
        from app_cli_recovery import OWNER_PROCESS_SCRIPT, STOP_CONTROLLER_SCRIPT, R3_EVIDENCE_SCRIPT
        tree = ast.parse((TOOLS / 'app_cli_recovery.py').read_text())
        function = next(node for node in ast.walk(tree)
                        if isinstance(node, ast.FunctionDef) and node.name == 'captured_process_gone')
        assignment = next(node for node in function.body if isinstance(node, ast.Assign))
        scripts = {'owner_identity': OWNER_PROCESS_SCRIPT,
                   'stop_controller': STOP_CONTROLLER_SCRIPT,
                   'r3_evidence': R3_EVIDENCE_SCRIPT,
                   'captured_process_gone': ast.literal_eval(assignment.value)}
        for name, code in scripts.items():
            with self.subTest(helper=name):
                result = subprocess.run([
                    sys.executable, '-c', 'import ast,sys; ast.parse(sys.argv[1])', code,
                ], capture_output=True, text=True, check=False)
                self.assertEqual(result.returncode, 0, result.stderr)


class CapturedOwnerIdentityGuardTests(unittest.TestCase):
    """Controlled /proc observations test the actual fault-injection guards.

    These fixtures cannot claim a running UserApp, HTTP recovery or E2E success.
    Every signal is intercepted, including in the positive guard path.
    """
    def setUp(self):
        from app_cli_recovery import OWNER_PROCESS_SCRIPT
        self.scope = {'__name__': 'owner_guard_fixture'}
        exec(compile(OWNER_PROCESS_SCRIPT, 'actual-owner-process-script', 'exec'), self.scope)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        folder = Path(self.temp.name).resolve()
        self.root, self.workspace, self.proc = folder / 'state', folder / 'source', folder / 'proc'
        self.root.mkdir()
        self.workspace.mkdir()
        generation = 'captured-generation'
        self.work = self.root / 'work' / generation
        self.work.mkdir(parents=True)
        self.process = self.proc / '42'
        (self.process / 'fd').mkdir(parents=True)
        (self.process / 'fdinfo').mkdir()
        self.write_stat(self.process, 42, '500')
        self.write_stat(self.proc / '1', 1, '100')
        boot = self.proc / 'sys/kernel/random/boot_id'
        boot.parent.mkdir(parents=True)
        boot.write_text('captured-boot\n')
        self.argv = ['/usr/local/bin/app-cli', 'serve', '--workspace', str(self.workspace)]
        (self.process / 'cmdline').write_bytes(bytes([0]).join(v.encode() for v in self.argv))
        executable = folder / 'app-cli'
        executable.write_text('executable identity fixture')
        (self.process / 'exe').symlink_to(executable)
        self.native = {'supervisor_id': 'captured-supervisor', 'generation': generation,
                       'binding': {'component': 'app-cli', 'resource': str(self.workspace)}}
        self.discovery = {'instance': 'captured-supervisor', 'snapshot': deepcopy(self.native)}
        self.kernel = {'application_id': 'fixture-app', 'workspace_id': 'fixture-app',
                       'source_root': str(self.workspace), 'runtime_instance_id': 'captured-runtime'}
        self.domain = {'authority': 'fixture', 'instance': 'captured-container', 'volume': 'owned-volume'}
        self.receipt = {'id': generation, 'supervisor': 'captured-supervisor',
                        'phase': 'Running', 'worker_pid': 42, 'physical_domain': self.domain,
                        'process_epoch': 'pid1:captured-boot:100'}
        self.write_json(self.root / 'supervisor.json', self.discovery)
        self.write_json(self.root / 'identity.json', self.kernel)
        self.write_json(self.work / 'generation.json', self.receipt)
        self.authority = {'state_root': str(self.root), 'workspace': str(self.workspace),
                          'application_id': 'fixture-app', 'native': self.native,
                          'kernel': self.kernel, 'physical_domain': self.domain,
                          'container_id': 'exact-owned-container'}
        self.lock_lines = []
        for fd, path in ((3, self.root / 'owner.lock'), (4, self.work / 'generation.lock')):
            path.touch()
            (self.process / 'fd' / str(fd)).symlink_to(path)
            # The translated device differs from the exact mounted file stat.
            # This mirrors the real retained-volume read-only reproduction.
            key = f'00:25:{path.stat().st_ino}'
            line = f'{fd}: FLOCK ADVISORY WRITE 42 {key} 0 EOF'
            (self.process / 'fdinfo' / str(fd)).write_text('lock:\t' + line + '\n')
            self.lock_lines.append(line)
        (self.proc / 'locks').write_text('\n'.join(self.lock_lines) + '\n')

    def write_stat(self, root, pid, start):
        root.mkdir(parents=True, exist_ok=True)
        fields = ['S'] + ['0'] * 18 + [start]
        (root / 'stat').write_text(f'{pid} (command with spaces) ' + ' '.join(fields))

    def write_json(self, path, value):
        path.write_text(json.dumps(value))

    def capture(self):
        result = self.scope['observe_owner'](self.authority, self.proc)
        result['authority'] = deepcopy(self.authority)
        return result

    def send(self, captured, expected_error=None):
        with patch.object(self.scope['os'], 'pidfd_open', return_value=123, create=True) as opened, \
                patch.object(self.scope['signal'], 'pidfd_send_signal', create=True) as sent, \
                patch.object(self.scope['os'], 'close') as closed, \
                patch.object(self.scope['os'], 'kill') as numeric_kill:
            if expected_error:
                with self.assertRaisesRegex(RuntimeError, expected_error):
                    self.scope['signal_owner'](captured, 15, self.proc)
                sent.assert_not_called()
            else:
                result = self.scope['signal_owner'](captured, 15, self.proc)
                self.assertEqual(result['signal_sent'], 15)
                sent.assert_called_once_with(123, 15)
            opened.assert_called_once_with(captured['pid'], 0)
            closed.assert_called_once_with(123)
            numeric_kill.assert_not_called()

    def test_mount_device_translation_keeps_exact_fd_and_kernel_holder_proof(self):
        captured = self.capture()
        self.assertEqual(captured['owner_lock']['kernel_key'][:2], [0, 37])
        self.assertEqual(captured['owner_lock']['file_id'],
                         [self.root.joinpath('owner.lock').stat().st_dev,
                          self.root.joinpath('owner.lock').stat().st_ino])
        self.send(captured)

    def test_exact_dot_run_alias_preserves_raw_argv_and_owner_identity(self):
        alias = self.workspace / '.run'
        alias.mkdir()
        self.argv[-1] = str(alias)
        (self.process / 'cmdline').write_bytes(bytes([0]).join(v.encode() for v in self.argv))
        captured = self.capture()
        self.assertEqual(captured['argv'], self.argv)
        self.assertEqual(captured['authority']['native']['binding']['resource'], str(self.workspace))
        self.assertEqual(captured['owner_lock']['holder_pid'], 42)
        self.assertEqual(captured['generation_lock']['holder_pid'], 42)
        self.send(captured)

    def test_foreign_dot_run_alias_refuses_signal(self):
        captured = self.capture()
        foreign = self.workspace.parent / 'foreign' / '.run'
        foreign.mkdir(parents=True)
        self.argv[-1] = str(foreign)
        (self.process / 'cmdline').write_bytes(bytes([0]).join(v.encode() for v in self.argv))
        self.send(captured, 'serve command differs')

    def test_arbitrary_source_child_directory_refuses_signal(self):
        captured = self.capture()
        child = self.workspace / 'another-directory'
        child.mkdir()
        self.argv[-1] = str(child)
        (self.process / 'cmdline').write_bytes(bytes([0]).join(v.encode() for v in self.argv))
        self.send(captured, 'serve command differs')

    def test_dot_run_symlink_escape_refuses_signal(self):
        captured = self.capture()
        foreign = self.workspace.parent / 'foreign' / '.run'
        foreign.mkdir(parents=True)
        alias = self.workspace / '.run'
        alias.symlink_to(foreign, target_is_directory=True)
        self.argv[-1] = str(alias)
        (self.process / 'cmdline').write_bytes(bytes([0]).join(v.encode() for v in self.argv))
        self.send(captured, 'serve command differs')

    def test_wrong_kernel_lock_holder_refuses_signal(self):
        captured = self.capture()
        (self.proc / 'locks').write_text(self.lock_lines[0].replace('WRITE 42 ', 'WRITE 84 ') + '\n'
                                       + self.lock_lines[1] + '\n')
        self.send(captured, 'kernel flock holder differs')

    def test_wrong_descriptor_lock_holder_refuses_signal(self):
        captured = self.capture()
        path = self.process / 'fdinfo/3'
        path.write_text(path.read_text().replace('WRITE 42 ', 'WRITE 84 '))
        self.send(captured, 'exact descriptor does not hold')

    def test_unrelated_descriptor_cannot_authorize_matching_numeric_holder(self):
        captured = self.capture()
        fd = self.process / 'fd/3'
        fd.unlink()
        fd.symlink_to(self.root / 'identity.json')
        self.send(captured, 'no unique held flock descriptor')

    def test_wrong_captured_pid_refuses_signal(self):
        captured = self.capture()
        captured['pid'] = 84
        self.send(captured, 'physical identity changed')

    def test_reused_pid_with_changed_starttime_refuses_signal(self):
        captured = self.capture()
        self.write_stat(self.process, 42, '501')
        self.send(captured, 'physical identity changed')

    def test_changed_native_instance_refuses_signal(self):
        captured = self.capture()
        self.discovery['instance'] = 'replacement-supervisor'
        self.write_json(self.root / 'supervisor.json', self.discovery)
        self.send(captured, 'native instance or generation changed')

    def test_changed_generation_refuses_signal(self):
        captured = self.capture()
        self.discovery['snapshot']['generation'] = 'replacement-generation'
        self.write_json(self.root / 'supervisor.json', self.discovery)
        self.send(captured, 'native instance or generation changed')

    def test_wrong_source_root_refuses_signal(self):
        captured = self.capture()
        self.argv[-1] = str(self.root)
        (self.process / 'cmdline').write_bytes(bytes([0]).join(v.encode() for v in self.argv))
        self.send(captured, 'serve command differs')

    def test_wrong_runtime_identity_refuses_signal(self):
        captured = self.capture()
        self.kernel['runtime_instance_id'] = 'replacement-runtime'
        self.write_json(self.root / 'identity.json', self.kernel)
        self.send(captured, 'kernel identity differs')

    def test_wrong_physical_container_refuses_signal(self):
        captured = self.capture()
        self.receipt['physical_domain']['instance'] = 'replacement-container'
        self.write_json(self.work / 'generation.json', self.receipt)
        self.send(captured, 'physical container or volume differs')

    def test_wrong_process_epoch_refuses_signal(self):
        captured = self.capture()
        self.write_stat(self.proc / '1', 1, '101')
        self.send(captured, 'process epoch changed')

    def test_same_generation_draining_keeps_exact_owner_identity(self):
        captured = self.capture()
        self.receipt['phase'] = 'Draining'
        self.write_json(self.work / 'generation.json', self.receipt)
        self.send(captured)

    def test_released_generation_lock_refuses_signal(self):
        captured = self.capture()
        (self.process / 'fdinfo/4').write_text('pos:\t0\n')
        (self.proc / 'locks').write_text(self.lock_lines[0] + '\n')
        self.send(captured, 'no unique held flock descriptor')

    def test_replaced_generation_lock_refuses_signal(self):
        captured = self.capture()
        lock = self.work / 'generation.lock'
        lock.rename(self.work / 'retained-old-generation.lock')
        lock.touch()
        self.send(captured, 'physical identity changed')

    def test_pidfd_signal_error_still_closes_descriptor_without_numeric_fallback(self):
        captured = self.capture()
        with patch.object(self.scope['os'], 'pidfd_open', return_value=123, create=True), \
                patch.object(self.scope['signal'], 'pidfd_send_signal',
                             side_effect=ProcessLookupError('captured process exited'), create=True), \
                patch.object(self.scope['os'], 'close') as closed, \
                patch.object(self.scope['os'], 'kill') as numeric_kill, \
                self.assertRaises(ProcessLookupError):
            self.scope['signal_owner'](captured, 15, self.proc)
        closed.assert_called_once_with(123)
        numeric_kill.assert_not_called()

    def test_missing_pidfd_support_has_no_numeric_pid_fallback(self):
        captured = self.capture()
        self.scope['os'] = SimpleNamespace()
        with patch('os.kill') as numeric_kill, self.assertRaisesRegex(RuntimeError, 'pidfd support'):
            self.scope['signal_owner'](captured, 15, self.proc)
        numeric_kill.assert_not_called()


class SubprocessDiagnosticEvidenceTests(unittest.TestCase):
    def test_actual_subprocess_failure_keeps_stderr_and_exit_code(self):
        from app_cli_recovery import subprocess_failure_evidence
        with self.assertRaises(subprocess.CalledProcessError) as raised:
            subprocess.run([sys.executable, '-c',
                            'raise RuntimeError("fixture guard: wrong lock holder")',
                            'private-argv-token'], capture_output=True, text=True, check=True)
        evidence = subprocess_failure_evidence(raised.exception)
        self.assertEqual(evidence['returncode'], 1)
        self.assertIn('fixture guard: wrong lock holder', evidence['stderr'])
        self.assertNotIn('private-argv-token', json.dumps(evidence))


class AdvisoryMigrationAssertionTests(unittest.TestCase):
    """Review the real H verdict expressions; no Docker or business is simulated."""
    def actual_check(self, prefix, scope):
        tree = ast.parse((TOOLS / 'app_cli_recovery.py').read_text())
        call = next(node for node in ast.walk(tree)
                    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
                    and node.func.id == 'check' and node.args
                    and isinstance(node.args[0], ast.Constant)
                    and str(node.args[0].value).startswith(prefix))
        expression = ast.fix_missing_locations(ast.Expression(body=deepcopy(call.args[1])))
        return eval(compile(expression, 'actual-h-verdict', 'eval'), {}, scope)

    def test_long_failure_output_and_unique_success_terminal_are_required(self):
        complete = 'H-ADVISORY-STDOUT-BEGIN\nH-ADVISORY-STDERR-BEGIN' + 'x' * 24576 + 'H-ADVISORY-STDERR-END\nERROR run.migrate Exit'
        terminal = [{'event': 'completed'}]
        valid = {'lines': complete, 'terminal': terminal, 'events': terminal}
        prefix = 'H: complete stdout and long stderr'
        self.assertTrue(self.actual_check(prefix, valid))
        for changes in [
            {'lines': complete.replace('H-ADVISORY-STDERR-END', '')},
            {'lines': complete.replace('x' * 24576, 'x' * 8192)},
            {'lines': complete.replace('ERROR run.migrate Exit', '')},
            {'terminal': terminal + terminal},
            {'terminal': [{'event': 'failed'}], 'events': [{'event': 'failed'}]},
            {'events': terminal + [{'event': 'log'}]},
        ]:
            with self.subTest(changes=list(changes)):
                self.assertFalse(self.actual_check(prefix, {**valid, **changes}))

    def test_advisory_exit_requires_false_receipt_real_http_and_same_instance(self):
        valid = {'failed_receipt': {'completed': False},
                 'content': lambda: 'recovery-c-2', 'identity': lambda: 'captured-owner',
                 'owner_h': 'captured-owner'}
        prefix = 'H: exit 1 never fabricates'
        self.assertTrue(self.actual_check(prefix, valid))
        for changes in [{'failed_receipt': {'completed': True}},
                        {'content': lambda: None}, {'identity': lambda: 'foreign-owner'}]:
            self.assertFalse(self.actual_check(prefix, {**valid, **changes}))

    def test_native_stop_acceptance_cannot_replace_original_physical_completion(self):
        scope = {'native_h': {'complete': False}, 'content': lambda: None,
                 'identity': lambda: 'captured-owner', 'owner_h': 'captured-owner'}
        self.assertFalse(self.actual_check('H: native Stop settles original', scope))


class NativeStopReceiptRegressionTests(unittest.TestCase):
    """Exercise the actual R3 completion assertions, without running its fixture."""
    def actual_check(self, prefix, scope):
        import app_cli_recovery
        tree = ast.parse((TOOLS / 'app_cli_recovery.py').read_text())
        call = next(node for node in ast.walk(tree)
                    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
                    and node.func.id == 'check' and node.args
                    and isinstance(node.args[0], ast.Constant)
                    and str(node.args[0].value).startswith(prefix))
        condition = ast.fix_missing_locations(ast.Expression(body=deepcopy(call.args[1])))
        return eval(compile(condition, 'actual-r3-completion-assertion', 'eval'),
                    vars(app_cli_recovery), scope)

    def test_stopped_intent_with_stopping_phase_is_not_native_completion(self):
        snapshot = {'phase': 'stopping', 'intent': 'stopped',
                    'operation_id': 'original-stop'}
        accepted = subprocess.CompletedProcess(['native-stop'], 0,
                                               json.dumps(snapshot), '')
        result = self.actual_check('R3: native StopWork', {
            'native': accepted, 'native_evidence': {'complete': False},
        })
        self.assertFalse(result, 'an accepted Stop is not its terminal receipt')

    def test_identity_alone_does_not_prove_journal_decoding_failure(self):
        result = self.actual_check('R3: management survives', {
            'degraded': {'verified': False},
        })
        self.assertFalse(result, 'management identity alone cannot prove business failure')


class R3ReceiptEvidenceTests(unittest.TestCase):
    """Use controlled disk/proc/RPC state; never execute native Stop or a signal."""
    write_stat = CapturedOwnerIdentityGuardTests.write_stat
    write_json = CapturedOwnerIdentityGuardTests.write_json
    capture = CapturedOwnerIdentityGuardTests.capture

    def setUp(self):
        from app_cli_recovery import R3_EVIDENCE_SCRIPT
        CapturedOwnerIdentityGuardTests.setUp(self)
        self.scope = {'__name__': 'actual-r3-receipt-fixture'}
        exec(compile(R3_EVIDENCE_SCRIPT, 'actual-r3-receipt-script', 'exec'), self.scope)
        self.owner = self.capture()
        self.request = {'request_id': 'original-native-stop', 'action': 'stop_work',
                        'expected_generation': self.owner['generation']}
        self.stopped = {'binding': self.native['binding'], 'supervisor_id': self.native['supervisor_id'],
                        'generation': None, 'phase': 'stopped', 'intent': 'stopped',
                        'operation_id': self.request['request_id']}
        self.discovery['requests'] = [[deepcopy(self.request), deepcopy(self.stopped)]]
        self.write_json(self.root / 'supervisor.json', self.discovery)
        self.receipt['phase'] = 'Quiescent'
        self.write_json(self.work / 'generation.json', self.receipt)
        self.write_json(self.work / 'cleanup-outcome.json', {'outcome': 'empty'})
        self.write_json(self.work / 'command-admission.json', {
            'generation': self.owner['generation'], 'accepting': False})
        self.write_json(self.work / 'supervisord-engine.json', {
            'generation': self.owner['generation'], 'supervisor_id': self.owner['supervisor_id'],
            'socket': '/var/run/supervisor.sock'})
        self.payload = {'owner': self.owner, 'request_id': self.request['request_id'],
                        'reply': deepcopy(self.stopped), 'kernel': deepcopy(self.kernel)}

    def proof(self, error=None):
        engine = {'supervisor_pid': 1, 'processes': [{'name': 'app-svc-web', 'pid': 0, 'state': 0}]}
        with patch.dict(self.scope, {'stopped_engine': lambda _: engine}), \
                patch.object(self.scope['os'], 'kill') as killed, \
                patch.object(self.scope['signal'], 'pidfd_send_signal', create=True) as signalled:
            if error:
                with self.assertRaisesRegex(RuntimeError, error):
                    self.scope['native_stop_proof'](self.payload, self.proc)
                result = None
            else:
                result = self.scope['native_stop_proof'](self.payload, self.proc)
            killed.assert_not_called()
            signalled.assert_not_called()
            return result

    def test_original_receipt_and_original_quiescence_complete_with_new_idle_generation(self):
        self.discovery['snapshot'].update(generation='new-idle-generation', phase='ready', intent='stopped')
        self.write_json(self.root / 'supervisor.json', self.discovery)
        result = self.proof()
        self.assertTrue(result['complete'])
        self.assertEqual(result['receipt']['operation_id'], 'original-native-stop')
        self.assertEqual(result['quiescence']['generation'], 'captured-generation')
        self.assertEqual(result['retained_owner']['owner_lock'], self.owner['owner_lock'])

    def test_stopping_original_receipt_does_not_complete_from_stopped_intent(self):
        self.discovery['requests'][0][1]['phase'] = 'stopping'
        self.payload['reply']['phase'] = 'stopping'
        self.write_json(self.root / 'supervisor.json', self.discovery)
        self.assertFalse(self.proof()['complete'])

    def test_latest_stopped_snapshot_cannot_replace_original_request_receipt(self):
        self.discovery['snapshot'].update(phase='stopped', intent='stopped', operation_id=None)
        self.discovery['requests'][0][1]['phase'] = 'cleanup_pending'
        self.write_json(self.root / 'supervisor.json', self.discovery)
        self.assertFalse(self.proof()['complete'])

    def test_changed_original_request_parameters_refuse_completion(self):
        self.discovery['requests'][0][0]['expected_generation'] = 'another-generation'
        self.write_json(self.root / 'supervisor.json', self.discovery)
        self.proof('original native Stop request changed')

    def test_another_native_reply_refuses_completion(self):
        self.payload['reply']['operation_id'] = 'another-stop'
        self.proof('original native Stop receipt identity changed')

    def test_original_draining_generation_cannot_use_latest_stopped_snapshot(self):
        self.receipt['phase'] = 'Draining'
        self.write_json(self.work / 'generation.json', self.receipt)
        self.proof('no aggregate Quiescent receipt')

    def test_cleanup_observation_failure_cannot_complete(self):
        self.write_json(self.work / 'cleanup-outcome.json', {
            'outcome': 'observation_failed', 'reason': 'controlled engine unavailable'})
        self.proof('original cleanup is not Empty')

    def test_original_physical_domain_mismatch_refuses_completion(self):
        self.receipt['physical_domain']['instance'] = 'replacement-container'
        self.write_json(self.work / 'generation.json', self.receipt)
        self.proof('original cleanup generation identity changed')

    def test_management_instance_change_refuses_completion(self):
        self.payload['kernel']['runtime_instance_id'] = 'replacement-instance'
        self.proof('original management instance changed')

    def test_owner_pid_reuse_refuses_completion(self):
        self.write_stat(self.process, 42, '501')
        self.proof('original management process identity changed')

    def test_replaced_owner_inode_refuses_completion(self):
        self.root.joinpath('owner.lock').rename(self.root / 'retained-owner.lock')
        self.root.joinpath('owner.lock').touch()
        self.proof('stable owner lock changed')

    def test_original_generation_lock_still_held_does_not_complete(self):
        import fcntl
        with self.work.joinpath('generation.lock').open('rb') as file:
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                self.assertFalse(self.proof()['complete'])
            finally:
                fcntl.flock(file, fcntl.LOCK_UN)

    def journal_fixture(self, log_error=True):
        self.receipt['phase'] = 'Running'
        self.write_json(self.work / 'generation.json', self.receipt)
        backup = self.root / '.deploy-operation.corrupt-this-attempt.json'
        damaged = '{damaged-journal:exact-attempt'
        backup.write_text(damaged)
        self.write_json(self.root / '.deploy-recovery-required.json', {
            'version': 1, 'originals': [str(backup)]})
        log = self.root / 'owner.log'
        log.write_text('rebuilding damaged deployment bookkeeping after supervisor takeover '
                       + 'backup=' + str(backup) + ' error=key must be a string\n'
                       if log_error else 'startup failed: credentials unavailable\n')
        return {'authority': self.owner['authority'], 'damaged_bytes': damaged,
                'log_path': str(log)}

    def test_this_journal_decode_failure_has_original_backup_and_same_live_management(self):
        result = self.scope['journal_failure'](self.journal_fixture(), self.proc)
        self.assertTrue(result['verified'])
        self.assertEqual(result['owner']['runtime_instance_id'], 'captured-runtime')
        self.assertEqual(result['owner']['owner_lock'], self.owner['owner_lock'])
        self.assertIn('key must be a string', result['owner_diagnostic'])

    def test_password_diagnostic_cannot_prove_this_journal_failure(self):
        result = self.scope['journal_failure'](self.journal_fixture(False), self.proc)
        self.assertFalse(result['verified'])

    def test_other_attempt_backup_cannot_prove_this_journal_failure(self):
        payload = self.journal_fixture()
        payload['damaged_bytes'] = '{different-attempt'
        self.assertFalse(self.scope['journal_failure'](payload, self.proc)['verified'])

    def test_engine_live_service_is_a_physical_stop_failure(self):
        from unittest.mock import MagicMock
        client = MagicMock()
        client.__enter__.return_value = client
        client.supervisor.getPID.return_value = 1
        client.supervisor.getAllProcessInfo.return_value = [
            {'name': 'app-svc-web', 'pid': 77, 'state': 20}]
        with patch.object(self.scope['xmlrpc'].client, 'ServerProxy', return_value=client), \
                self.assertRaisesRegex(RuntimeError, 'not physically stopped'):
            self.scope['stopped_engine']('/controlled-socket')

    def test_actual_native_loop_replays_the_same_request_and_generation(self):
        import app_cli_recovery
        tree = ast.parse((TOOLS / 'app_cli_recovery.py').read_text())
        functions = [deepcopy(node) for node in ast.walk(tree)
                     if isinstance(node, ast.FunctionDef)
                     and node.name in ('r3_remaining', 'complete_native_stop')]
        module = ast.fix_missing_locations(ast.Module(body=functions, type_ignores=[]))
        calls, proofs = [], []
        def docker(*args, **kwargs):
            calls.append((args, kwargs))
            if args[2] == 'python3': output = json.dumps(self.owner)
            elif args[2] == 'app-cli': output = json.dumps(self.stopped)
            else: return subprocess.CompletedProcess(args, 7, '', 'connection refused')
            return subprocess.CompletedProcess(args, 0, output, '')
        def evidence(mode, payload, deadline):
            self.assertEqual(mode, 'native')
            proofs.append(payload)
            return {'complete': len(proofs) == 2}
        scope = {'docker': docker, 'r3_evidence': evidence, 'r3_identity': lambda _: self.kernel,
                 'time': SimpleNamespace(monotonic=lambda: 0, sleep=lambda _: None),
                 'uuid': SimpleNamespace(uuid4=lambda: SimpleNamespace(hex='fixed-request')),
                 'json': json, 'report': {}, 'cid': 'exact-owned-container',
                 'workspace': str(self.workspace), 'OWNER_PROCESS_SCRIPT': app_cli_recovery.OWNER_PROCESS_SCRIPT}
        exec(compile(module, 'actual-native-replay-loop', 'exec'), scope)
        result = scope['complete_native_stop'](self.owner)
        native = [args for args, _ in calls if args[2] == 'app-cli']
        self.assertEqual(len(native), 2)
        self.assertEqual(native[0], native[1])
        self.assertEqual(native[0][-4:], ('--request-id', 'r3-native-fixed-request',
                                         '--generation', 'captured-generation'))
        self.assertEqual([payload['request_id'] for payload in proofs], ['r3-native-fixed-request'] * 2)
        self.assertEqual(result['business_http_exit_code'], 7)
        self.assertTrue(all(kwargs['timeout'] == 60 for _, kwargs in calls))


class PhysicalStopBarrierRegressionTests(unittest.TestCase):
    write_stat = CapturedOwnerIdentityGuardTests.write_stat
    write_json = CapturedOwnerIdentityGuardTests.write_json
    capture = CapturedOwnerIdentityGuardTests.capture

    def setUp(self):
        from app_cli_recovery import STOP_CONTROLLER_SCRIPT
        CapturedOwnerIdentityGuardTests.setUp(self)
        self.scope = {'__name__': 'actual-stop-controller-fixture'}
        exec(compile(STOP_CONTROLLER_SCRIPT, 'actual-stop-controller', 'exec'), self.scope)
        web = self.workspace / 'web'
        web.mkdir()
        (web / 'main.py').write_text('captured source fixture')
        self.business_proc = self.proc / '77'
        self.write_stat(self.business_proc, 77, '600')
        stat = self.business_proc / 'stat'
        stat.write_text(stat.read_text().replace(') S 0 ', ') S 1 '))
        (self.business_proc / 'cmdline').write_bytes(b'python3' + bytes([0]) + b'main.py')
        (self.business_proc / 'cwd').symlink_to(web)
        (self.business_proc / 'exe').symlink_to(self.process / 'exe')
        (self.process / 'cwd').symlink_to(self.workspace)
        self.ack = web / 'stop-ack'
        self.engine = {'generation': self.native['generation'],
                       'supervisor_id': self.native['supervisor_id'],
                       'socket': '/var/run/supervisor.sock'}
        self.write_json(self.work / 'supervisord-engine.json', self.engine)
        self.info = {'name': 'app-svc-web', 'group': 'app-svc-web',
                     'pid': 77, 'state': 20, 'statename': 'RUNNING'}
        self.owner = self.capture()
        self.request = {'operation_id': 'original-stop', 'expected_runtime_instance_id': 'captured-runtime',
                        'expected_revision': 9, 'workspace_id': 'fixture-app', 'kind': 'stop',
                        'profile': {'profile': 'source', 'input': {'workspace_id': 'fixture-app'}}}
        self.view = {'operation_id': 'original-stop', 'kind': 'stop', 'state': 'accepted',
                     'request_digest': 'original-request-digest', 'revision': 9,
                     'runtime_instance_id': 'captured-runtime'}
        self.original_receipt = {'view': deepcopy(self.view), 'request': deepcopy(self.request)}
        self.record = self.root / 'operations/original-stop.json'
        self.record.parent.mkdir()
        self.payload = {'owner': self.owner, 'operation_id': 'original-stop',
                        'revision': 9, 'deploy_token': 'private-fixture-token'}
        self.emit_ack = True
        self.ack_pid = 77
        self.after_observation = None
        self.calls = []

    def http(self, method, path, token, body=None, timeout=1):
        self.calls.append((method, path, deepcopy(body)))
        self.assertEqual(token, 'private-fixture-token')
        if path == '/v1/runtime/identity':
            return 200, {'success': True, 'data': deepcopy(self.kernel)}
        if method == 'POST':
            self.assertEqual(body, self.request)
            self.assertFalse(self.ack.exists(), 'stale ACK must be cleared before the original POST')
            self.write_json(self.record, self.original_receipt)
            return 202, {'success': True, 'data': {'operation_id': 'original-stop', 'state': 'accepted'}}
        self.assertEqual(path, '/v1/runtime/operations/original-stop')
        if self.emit_ack:
            self.ack.write_bytes(str(self.ack_pid).encode())
        if self.after_observation:
            self.after_observation()
        return 200, {'success': True, 'data': deepcopy(self.view)}

    def controller(self, error=None, clock=None):
        progress = {}
        replacements = {'runtime_request': self.http,
                        'supervisor_process': lambda _: (deepcopy(self.info), 1)}
        with patch.dict(self.scope, replacements), \
                patch.object(self.scope['os'], 'pidfd_open', return_value=123, create=True) as opened, \
                patch.object(self.scope['signal'], 'pidfd_send_signal', create=True) as sent, \
                patch.object(self.scope['os'], 'kill') as numeric_kill, \
                patch.object(self.scope['os'], 'close') as closed, \
                patch.object(self.scope['select'], 'select', return_value=([123], [], [])) as exited:
            def run():
                if error:
                    with self.assertRaisesRegex(RuntimeError, error):
                        self.scope['controlled_stop'](self.payload, self.proc, progress)
                    sent.assert_not_called()
                    exited.assert_not_called()
                    return progress
                result = self.scope['controlled_stop'](self.payload, self.proc, progress)
                sent.assert_called_once_with(123, 9)
                exited.assert_called_once()
                self.assertTrue(result['kill_proof']['pidfd_readable'])
                self.assertEqual(result['barrier']['receipt'], self.original_receipt)
                self.assertEqual(result['operation_id'], 'original-stop')
                return result
            if clock:
                with patch.object(self.scope['time'], 'monotonic', side_effect=clock):
                    result = run()
            else:
                result = run()
            opened.assert_called_once_with(42, 0)
            closed.assert_called_once_with(123)
            numeric_kill.assert_not_called()
        self.assertLessEqual(sum(method == 'POST' for method, _, _ in self.calls), 1,
                             'a missed original Stop must never be submitted again')
        self.assertNotIn('private-fixture-token', json.dumps(result))
        return result

    def test_accepted_original_stop_with_same_pid_ack_is_not_excluded(self):
        result = self.controller()
        self.assertEqual(result['barrier']['operation']['state'], 'accepted')
        self.assertEqual(result['barrier']['ack']['bytes'], '77')
        self.assertTrue(result['barrier']['business_alive'])
        self.assertEqual(result['business_capture']['start_time'], '600')
        self.assertEqual(result['business_capture']['cwd'], str(self.workspace / 'web'))
        self.assertEqual(result['owner_proof']['owner_lock']['holder_pid'], 42)

    def test_real_ack_is_required_after_acceptance(self):
        self.emit_ack = False
        result = self.controller('ACK window missed', clock=[0, 0, 0, 0, 11])
        self.assertEqual(result['admission']['status'], 202)
        self.assertNotIn('signal', result)

    def test_stale_ack_is_cleared_and_never_authorizes_signal(self):
        self.ack.write_bytes(b'77')
        self.emit_ack = False
        self.controller('ACK window missed', clock=[0, 0, 0, 0, 11])

    def test_other_service_pid_ack_refuses_signal(self):
        self.ack_pid = 88
        self.controller('ACK differs from the captured business PID')

    def test_original_operation_terminal_refuses_signal(self):
        self.view['state'] = self.original_receipt['view']['state'] = 'succeeded'
        self.controller('Stop window closed')

    def test_illegal_running_state_refuses_signal(self):
        self.view['state'] = self.original_receipt['view']['state'] = 'running'
        self.controller('state is unsupported')

    def test_wrong_operation_id_refuses_signal(self):
        self.view['operation_id'] = 'another-stop'
        self.controller('Stop receipt identity differs')

    def test_wrong_admitting_instance_refuses_signal(self):
        self.view['runtime_instance_id'] = 'another-runtime'
        self.controller('Stop receipt identity differs')

    def test_wrong_persisted_original_request_refuses_signal(self):
        self.original_receipt['request']['expected_revision'] = 999
        self.controller('Stop receipt identity differs')

    def test_reused_business_pid_starttime_refuses_signal(self):
        def changed():
            path = self.business_proc / 'stat'
            path.write_text(path.read_text().replace('600', '601'))
        self.after_observation = changed
        self.controller('business physical identity changed')

    def test_zombie_business_refuses_signal(self):
        def changed():
            path = self.business_proc / 'stat'
            path.write_text(path.read_text().replace(') S ', ') Z '))
        self.after_observation = changed
        self.controller('not the captured Source service')

    def test_wrong_supervisor_service_pid_refuses_before_post(self):
        self.info['pid'] = 42
        self.controller('not the captured Source service')
        self.assertFalse(any(method == 'POST' for method, _, _ in self.calls))

    def test_wrong_engine_generation_refuses_before_post(self):
        self.engine['generation'] = 'another-generation'
        self.write_json(self.work / 'supervisord-engine.json', self.engine)
        self.controller('engine does not belong')
        self.assertFalse(any(method == 'POST' for method, _, _ in self.calls))

    def test_wrong_owner_lock_holder_after_ack_refuses_signal(self):
        def changed():
            (self.proc / 'locks').write_text(self.lock_lines[0].replace('WRITE 42 ', 'WRITE 84 ') + '\n'
                                           + self.lock_lines[1] + '\n')
        self.after_observation = changed
        self.controller('kernel flock holder differs')

    def test_terminal_transition_during_final_recheck_refuses_signal(self):
        observations = [0]
        def changed():
            observations[0] += 1
            if observations[0] == 2:
                self.view['state'] = self.original_receipt['view']['state'] = 'succeeded'
                self.write_json(self.record, self.original_receipt)
        self.after_observation = changed
        self.controller('Stop window closed')

    def test_owner_generation_changes_during_final_operation_read_refuses_signal(self):
        observations = [0]
        def changed():
            observations[0] += 1
            if observations[0] == 2:
                self.discovery['snapshot']['generation'] = 'replacement-generation'
                self.write_json(self.root / 'supervisor.json', self.discovery)
        self.after_observation = changed
        self.controller('native instance or generation changed')

    def test_busy_admission_is_not_retried_or_signalled(self):
        original = self.http
        def busy(method, path, token, body=None, timeout=1):
            if method == 'POST':
                self.calls.append((method, path, body))
                return 409, {'success': False, 'code': 'ERR_OPERATION_IN_PROGRESS'}
            return original(method, path, token, body, timeout)
        self.http = busy
        self.controller('was not admitted')


if __name__ == '__main__':
    unittest.main()
