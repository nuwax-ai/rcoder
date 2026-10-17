"""Offline fixture checks do not count as real Compose acceptance."""
import copy
import errno
import json
import http.server
import os
from pathlib import Path
import tempfile
import socket
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import compute_failure_contract as contract
import docker_fault_proxy as proxy


class PhasePollingTests(unittest.TestCase):
    class Clock:
        def __init__(self):
            self.now = 0.0
            self.sleeps = []

        def monotonic(self):
            return self.now

        def sleep(self, seconds):
            self.sleeps.append(seconds)
            self.now += seconds

    def poll(self, api, deadline, field='state', expected='succeeded', interval=.5):
        return contract.poll_state(api, '/original-operation', deadline,
            field=field, expected=expected,
            waiting={'pending', 'running'} if field == 'status' else {'pending', 'running', 'recovery_required'},
            rejected={'failed', 'cancelled'} if field == 'status' else {'failed', 'superseded'},
            known=contract.TASK_STATUSES if field == 'status' else contract.COMPUTE_STATES,
            interval=interval)

    def test_unknown_task_and_compute_values_fail_on_the_first_observation(self):
        for field, expected in (('status', 'completed'), ('state', 'succeeded')):
            for unknown in ('new_wire_variant', None, ['not-a-string']):
                clock = self.Clock()
                replies = [{field: unknown}, {field: expected}]
                calls = []
                def api(path, body=None, timeout=180):
                    calls.append(path)
                    return replies.pop(0)
                with self.subTest(field=field, unknown=unknown), \
                        patch.object(contract.time, 'monotonic', clock.monotonic), \
                        patch.object(contract.time, 'sleep', clock.sleep):
                    with self.assertRaises(ValueError):
                        self.poll(api, contract.PhaseDeadline(10, 'unknown state'), field, expected)
                    self.assertEqual(len(calls), 1)
                    self.assertEqual(clock.sleeps, [])

    def test_post_poll_and_sleep_share_one_decreasing_budget(self):
        clock = self.Clock()
        timeouts = []
        replies = [(4, {'operation_id': 'original'}), (2, {'state': 'running'}),
                   (1, {'state': 'succeeded'})]
        def api(path, body=None, timeout=180):
            timeouts.append(timeout)
            elapsed, reply = replies.pop(0)
            clock.now += elapsed
            return reply
        with patch.object(contract.time, 'monotonic', clock.monotonic), \
                patch.object(contract.time, 'sleep', clock.sleep):
            deadline = contract.PhaseDeadline(10, 'original restart')
            deadline.request(api, '/restart', {'request_id': 'original'})
            self.assertEqual(self.poll(api, deadline)['state'], 'succeeded')
        self.assertEqual(timeouts, [10, 6, 3.5])
        self.assertEqual(clock.sleeps, [.5])

    def test_success_arriving_after_total_deadline_is_not_accepted(self):
        for field, expected in (('status', 'completed'), ('state', 'succeeded')):
            clock = self.Clock()
            def api(path, body=None, timeout=180):
                clock.now += 11
                return {field: expected}
            with self.subTest(field=field), \
                    patch.object(contract.time, 'monotonic', clock.monotonic), \
                    patch.object(contract.time, 'sleep', clock.sleep):
                with self.assertRaises(TimeoutError):
                    self.poll(api, contract.PhaseDeadline(10, 'late result'), field, expected)

    def test_post_time_is_not_reset_before_polling(self):
        clock = self.Clock()
        timeouts = []
        def api(path, body=None, timeout=180):
            timeouts.append(timeout)
            clock.now += 9 if body is not None else 2
            return {'operation_id': 'original'} if body is not None else {'state': 'succeeded'}
        with patch.object(contract.time, 'monotonic', clock.monotonic), \
                patch.object(contract.time, 'sleep', clock.sleep):
            deadline = contract.PhaseDeadline(10, 'restart budget')
            deadline.request(api, '/restart', {'request_id': 'original'})
            with self.assertRaises(TimeoutError):
                self.poll(api, deadline)
        self.assertEqual(timeouts, [10, 1])

    def test_sleep_cannot_overrun_budget_or_allow_another_http_request(self):
        clock = self.Clock()
        calls = []
        def api(path, body=None, timeout=180):
            calls.append(path)
            clock.now += .8 if len(calls) == 1 else 0
            return {'state': 'running' if len(calls) == 1 else 'succeeded'}
        with patch.object(contract.time, 'monotonic', clock.monotonic), \
                patch.object(contract.time, 'sleep', clock.sleep):
            with self.assertRaises(TimeoutError):
                self.poll(api, contract.PhaseDeadline(1, 'short remaining budget'))
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(clock.sleeps), 1)
        self.assertAlmostEqual(clock.sleeps[0], .2)

    def test_expired_phase_makes_no_http_request(self):
        clock = self.Clock()
        calls = []
        with patch.object(contract.time, 'monotonic', clock.monotonic):
            deadline = contract.PhaseDeadline(10, 'expired')
            clock.now = 10
            def api(*args, **kwargs):
                calls.append(args)
                return {'state': 'succeeded'}
            with self.assertRaises(TimeoutError):
                deadline.request(api, '/original')
            self.assertEqual(calls, [])

    def test_every_known_status_has_a_reviewed_wait_success_or_failure_decision(self):
        policies = [
            ('status', 'completed', contract.TASK_STATUSES,
             {'pending': 'wait', 'running': 'wait', 'completed': 'success',
              'failed': 'failure', 'cancelled': 'failure'}),
            ('state', 'recovery_required', contract.COMPUTE_STATES,
             {'pending': 'wait', 'running': 'wait', 'recovery_required': 'success',
              'succeeded': 'failure', 'failed': 'failure', 'superseded': 'failure'}),
            ('state', 'succeeded', contract.COMPUTE_STATES,
             {'pending': 'wait', 'running': 'wait', 'recovery_required': 'wait',
              'succeeded': 'success', 'failed': 'failure', 'superseded': 'failure'}),
        ]
        for field, expected, known, outcomes in policies:
            self.assertEqual(set(outcomes), known, 'new wire states require a reviewed test outcome')
            waiting = {key for key, value in outcomes.items() if value == 'wait'}
            rejected = {key for key, value in outcomes.items() if value == 'failure'}
            for state, outcome in outcomes.items():
                clock = self.Clock()
                replies = [{field: state, 'stage': 'accepted-or-new-display-stage'}, {field: expected}]
                calls = []
                def api(path, body=None, timeout=180):
                    calls.append(path)
                    return replies.pop(0)
                with self.subTest(field=field, expected=expected, state=state), \
                        patch.object(contract.time, 'monotonic', clock.monotonic), \
                        patch.object(contract.time, 'sleep', clock.sleep):
                    deadline = contract.PhaseDeadline(10, 'reviewed wire state')
                    invoke = lambda: contract.poll_state(api, '/original', deadline,
                        field=field, expected=expected, waiting=waiting, rejected=rejected,
                        known=known, interval=.5)
                    if outcome == 'failure':
                        with self.assertRaises(RuntimeError):
                            invoke()
                    else:
                        self.assertEqual(invoke()[field], expected)
                    self.assertEqual(len(calls), 2 if outcome == 'wait' else 1)
                    self.assertEqual(len(clock.sleeps), 1 if outcome == 'wait' else 0)

    def test_new_known_state_without_a_decision_is_rejected_before_http(self):
        clock = self.Clock()
        calls = []
        def api(*args, **kwargs):
            calls.append(args)
            return {'status': 'completed'}
        with patch.object(contract.time, 'monotonic', clock.monotonic):
            with self.assertRaises(ValueError):
                contract.poll_state(api, '/task', contract.PhaseDeadline(10, 'unreviewed status'),
                    field='status', expected='completed', waiting={'pending', 'running'},
                    rejected={'failed', 'cancelled'}, known=contract.TASK_STATUSES | {'new_state'}, interval=1)
        self.assertEqual(calls, [])


class RequiredEntryPortTests(unittest.TestCase):
    def test_rust_entry_port_is_required_and_only_accepts_tcp_port_numbers(self):
        for port in (1, 31234, 65535):
            self.assertEqual(contract.required_app_entry_port({'E2E_COMPUTE_APP_ENTRY_PORT': str(port)}), port)
        for value in (None, '', '0', '65536', '-1', '1.5', 'localhost:1234'):
            with self.subTest(value=value), self.assertRaises(ValueError):
                contract.required_app_entry_port({} if value is None else {'E2E_COMPUTE_APP_ENTRY_PORT': value})

    def test_missing_or_invalid_port_fails_before_docker_or_fixture_creation(self):
        for value in (None, '0', '65536', 'not-a-port'):
            with tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary) / 'reports'
                environment = {'E2E_REPORT_DIR': str(directory), 'E2E_COMPUTE_BUILD_RECEIPT': str(Path(temporary) / 'unused.json')}
                if value is not None:
                    environment['E2E_COMPUTE_APP_ENTRY_PORT'] = value
                with self.subTest(value=value), patch.dict('os.environ', environment, clear=True), \
                        patch.object(contract, 'command') as command:
                    with self.assertRaises(ValueError):
                        contract.main()
                    command.assert_not_called()
                    self.assertFalse(directory.exists())


class ComputeFailureReceiptTests(unittest.TestCase):
    def setUp(self):
        self.source = {'origin_head': 'frozen', 'worktree_sha256': 'a' * 64}
        self.build = {'version': 1, 'source': self.source,
                      'rcoder': {'image_id': 'sha256:' + 'b' * 64, 'binary_sha256': 'c' * 64},
                      'builder': {'image_id': 'sha256:' + 'd' * 64,
                                  'agent_runner_sha256': 'e' * 64, 'app_cli_sha256': 'f' * 64}}

    def test_requires_matching_frozen_source_and_all_build_time_binary_hashes(self):
        self.assertEqual(contract.validate_build(self.build, self.source), self.build)
        for role, field in (('rcoder', 'image_id'), ('rcoder', 'binary_sha256'),
                            ('builder', 'image_id'), ('builder', 'agent_runner_sha256'),
                            ('builder', 'app_cli_sha256')):
            changed = copy.deepcopy(self.build)
            del changed[role][field]
            with self.subTest(role=role, field=field), self.assertRaises(ValueError):
                contract.validate_build(changed, self.source)
        with self.assertRaises(ValueError):
            contract.validate_build(self.build, dict(self.source, worktree_sha256='0' * 64))

    def test_invalid_receipt_fails_before_docker_or_fixture_creation(self):
        with tempfile.TemporaryDirectory() as temporary:
            report = Path(temporary) / 'reports'
            receipt = Path(temporary) / 'build.json'
            receipt.write_text('{}')
            with patch.dict('os.environ', {'E2E_REPORT_DIR': str(report),
                                           'E2E_COMPUTE_BUILD_RECEIPT': str(receipt),
                                           'E2E_COMPUTE_APP_ENTRY_PORT': '31234'}), \
                    patch.object(contract, 'source_identity', return_value=self.source), \
                    patch.object(contract, 'command') as command:
                with self.assertRaises(ValueError):
                    contract.main()
                command.assert_not_called()
            self.assertFalse(report.exists())

    def test_cleanup_rejects_foreign_receipt_before_docker(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'ownership.json').write_text(json.dumps({'root': str(root), 'run_id': 'foreign',
                'case_id': 'case', 'project': 'rcoder-cfailure-' + 'a' * 16, 'app_id': 'foreign'}))
            with patch.object(contract, 'command') as command:
                with self.assertRaises(ValueError):
                    contract.cleanup(root, 'run', 'case')
                command.assert_not_called()

    def test_cleanup_validates_both_owned_compose_services_and_retains_volumes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            run, case = 'run', 'a' * 32
            project = 'rcoder-cfailure-' + 'b' * 16
            labels = {'com.docker.compose.project': project, 'rcoder.e2e.run': run, 'rcoder.e2e.case': case}
            rows = [{'Id': service, 'Name': '/' + project + '-' + service + '-1',
                     'Config': {'Labels': dict(labels, **{'com.docker.compose.service': service})}}
                    for service in ('rcoder', 'docker-proxy')]
            receipt = {'root': str(root), 'run_id': run, 'case_id': case, 'project': project,
                       'app_id': 'cf' + case[:10] + 'c' * 10, 'creation_started': True,
                       'volume_names': [project + '_compute-data', project + '_fault-socket']}
            (root / 'ownership.json').write_text(json.dumps(receipt))
            def command(*args, **kwargs):
                if args[:3] == ('docker', 'ps', '-aq'):
                    return 'rcoder docker-proxy'
                if args[:2] == ('docker', 'inspect'):
                    return json.dumps(rows)
                if args[:3] == ('docker', 'volume', 'inspect'):
                    return json.dumps([{'Labels': labels}])
                return ''
            with patch.object(contract, 'command', side_effect=command) as calls, \
                    patch.object(contract, 'builder_rows', return_value=[]):
                result = contract.cleanup(root, run, case)
            self.assertTrue(result['ok'])
            self.assertFalse(result['volumes_removed'])
            self.assertEqual(result['retained_volumes'], receipt['volume_names'])
            self.assertFalse(any('-v' in call.args for call in calls.call_args_list))
            original_rows = copy.deepcopy(rows)
            for field in ('com.docker.compose.service', 'rcoder.e2e.case', 'physical-name'):
                rows = copy.deepcopy(original_rows)
                if field == 'physical-name':
                    rows[1]['Name'] = '/foreign'
                else:
                    rows[1]['Config']['Labels'][field] = 'foreign'
                with self.subTest(field=field), \
                        patch.object(contract, 'command', side_effect=command) as calls:
                    with self.assertRaises(ValueError):
                        contract.cleanup(root, run, case)
                    self.assertFalse(any('compose' in call.args for call in calls.call_args_list))


class ImmutableFaultPublicationTests(unittest.TestCase):
    def test_unique_private_temp_is_closed_before_publish_and_target_cannot_be_reused(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original_replace = Path.replace
            original_mkstemp = tempfile.mkstemp
            descriptors, names = [], []
            def create(*args, **kwargs):
                descriptor, name = original_mkstemp(*args, **kwargs)
                descriptors.append(descriptor)
                names.append(Path(name))
                self.assertEqual(os.fstat(descriptor).st_mode & 0o777, 0o600)
                return descriptor, name
            def replace(source, target):
                self.assertEqual(source.parent, target.parent)
                self.assertEqual(source.stat().st_mode & 0o777, 0o600)
                with self.assertRaises(OSError):
                    os.fstat(descriptors[-1])
                return original_replace(source, target)
            with patch.object(contract.tempfile, 'mkstemp', side_effect=create), \
                    patch.object(Path, 'replace', replace):
                contract.publish_fault_config(root / 'arm.json', {'container_id': 'captured'})
                contract.publish_fault_config(root / 'operation.json', {'operation_id': 'original'})
            self.assertEqual(len(set(names)), 2)
            self.assertTrue(all(not path.exists() for path in names))
            self.assertEqual((root / 'arm.json').stat().st_mode & 0o777, 0o600)
            before = (root / 'arm.json').read_bytes()
            with patch.object(contract.tempfile, 'mkstemp') as create:
                with self.assertRaises(ValueError):
                    contract.publish_fault_config(root / 'arm.json', {'container_id': 'replacement'})
                create.assert_not_called()
            self.assertEqual((root / 'arm.json').read_bytes(), before)

    def test_serialization_and_replace_errors_propagate_and_remove_the_temp(self):
        for failure in ('serialization', 'replace'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                path = root / 'arm.json'
                if failure == 'serialization':
                    with self.assertRaises(TypeError):
                        contract.publish_fault_config(path, {'unsupported': object()})
                else:
                    with patch.object(Path, 'replace', side_effect=OSError('publication failed')):
                        with self.assertRaises(OSError):
                            contract.publish_fault_config(path, {'container_id': 'captured'})
                self.assertEqual(list(root.iterdir()), [])

    def test_concurrent_reader_only_observes_the_complete_immutable_document(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'arm.json'
            observed, ready, stopped = threading.Event(), threading.Event(), threading.Event()
            errors, values = [], []
            expected = {'container_id': 'captured', 'payload': 'fixture' * 10000}
            def read():
                ready.set()
                while not stopped.is_set():
                    try:
                        value = json.loads(path.read_bytes())
                        values.append(value)
                        observed.set()
                    except FileNotFoundError:
                        continue
                    except Exception as error:
                        errors.append(type(error).__name__)
                        observed.set()
                        return
            worker = threading.Thread(target=read, daemon=True)
            worker.start()
            try:
                self.assertTrue(ready.wait(3))
                contract.publish_fault_config(path, expected)
                self.assertTrue(observed.wait(3))
            finally:
                stopped.set()
                worker.join(timeout=3)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            self.assertTrue(values)
            self.assertTrue(all(value == expected for value in values))


class ComputeStoppedFaultTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.arm = {'container_id': 'captured-id', 'lifecycle_id': 'life'}
        (self.root / 'compute-arm.json').write_text(json.dumps(self.arm))
        (self.root / 'compute-operation.json').write_text(json.dumps({'operation_id': 'op'}))
        self.operation = {'app_id': 'app', 'lifecycle_id': 'life', 'operation_id': 'op',
                          'state': 'running', 'stage': 'stopped'}
        self.actual = {'Id': 'captured-id', 'Config': {'Labels': {
            'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'life',
            'service-type': 'user-app-builder'}}, 'State': {'Running': False}}
        self.patch = patch.multiple(proxy, ROOT=self.root, APP='app', MODE='compute_stopped',
            OWNED_CONTAINERS={'captured-id': 'life'}, STOPPED_CONTAINERS={'captured-id'})
        self.patch.start()
        self.response = SimpleNamespace(status=200, read=lambda: json.dumps(self.actual).encode())
        self.closed = []
        self.connection = SimpleNamespace(close=lambda: self.closed.append(True))
        self.http = SimpleNamespace(__enter__=lambda s: s, __exit__=lambda *a: None)

    def tearDown(self):
        self.patch.stop()
        self.temporary.cleanup()

    def invoke(self, path='/v1.47/containers/rcoder-app-builder-app/json', method='GET'):
        from io import BytesIO
        class Envelope(BytesIO):
            def __enter__(self):
                return self
            def __exit__(self, *args):
                self.close()
        envelope = Envelope(json.dumps({'code': '0000', 'data': self.operation}).encode())
        with patch.object(proxy.urllib.request, 'urlopen', return_value=envelope) as observation, \
                patch.object(proxy, 'exchange', return_value=(self.connection, self.response)) as exchange:
            result = proxy.compute_fault(method, path)
            return result, observation.call_count, exchange.call_count

    def test_stopping_confirmation_is_real_and_stopped_diagnostic_is_repeatable(self):
        self.operation['stage'] = 'stopping'
        result, reads, docker = self.invoke()
        self.assertIsNone(result)
        self.assertEqual((reads, docker), (1, 0))
        self.assertFalse((self.root / 'compute-faults.jsonl').exists())
        self.operation['stage'] = 'stopped'
        for state in ('running', 'recovery_required'):
            self.operation['state'] = state
            self.assertEqual(self.invoke()[0], contract.MARKER)
        events = [json.loads(s) for s in (self.root / 'compute-faults.jsonl').read_text().splitlines()]
        self.assertEqual([e['operation']['stage'] for e in events], ['stopped', 'stopped'])
        self.assertEqual(len(self.closed), 2)

    def test_operation_identity_is_read_from_a_separate_immutable_publication(self):
        arm = dict(self.arm)
        path = self.root / 'compute-arm.json'
        path.write_text(json.dumps(arm))
        operation = self.root / 'compute-operation.json'
        operation.write_text(json.dumps({'operation_id': 'op'}))
        original = path.read_bytes(), operation.read_bytes()
        with patch.object(proxy.time, 'sleep', side_effect=AssertionError('published identity must not wait')):
            for _ in range(2):
                self.assertEqual(self.invoke()[0], contract.MARKER)
        self.assertEqual((path.read_bytes(), operation.read_bytes()), original)

    def test_missing_operation_publication_waits_with_the_original_budget(self):
        (self.root / 'compute-operation.json').unlink(missing_ok=True)
        with patch.object(proxy.time, 'monotonic', side_effect=[0, 16]):
            with self.assertRaises(TimeoutError):
                self.invoke()

    def test_invalid_published_operation_json_is_not_ignored(self):
        (self.root / 'compute-operation.json').write_text('{')
        with self.assertRaises(json.JSONDecodeError):
            self.invoke()

    def test_invalid_published_operation_identity_is_rejected(self):
        for value in ('', None, 123):
            (self.root / 'compute-operation.json').write_text(json.dumps({'operation_id': value}))
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.invoke()
        (self.root / 'compute-operation.json').write_text('[]')
        with self.assertRaises(ValueError):
            self.invoke()

    def test_revoked_arm_ends_operation_publication_wait_without_injecting(self):
        (self.root / 'compute-operation.json').unlink(missing_ok=True)
        with patch.object(proxy.time, 'monotonic', return_value=0), \
                patch.object(proxy.time, 'sleep', side_effect=lambda _: (self.root / 'compute-arm.json').unlink()):
            self.assertEqual(self.invoke(), (None, 0, 0))

    def test_every_other_phase_and_terminal_passes_without_fault(self):
        for stage in ('draining_previous', 'starting', 'verifying', 'completed'):
            self.operation['stage'] = stage
            with self.subTest(stage=stage):
                self.assertIsNone(self.invoke()[0])
        self.operation.update(stage='stopped', state='succeeded')
        self.assertIsNone(self.invoke()[0])

    def test_scope_and_stop_acknowledgement_are_required_before_observation(self):
        for path in ('/containers/foreign/json', '/containers/captured-id/logs', '/images/id/json'):
            self.assertEqual(self.invoke(path), (None, 0, 0))
        self.assertEqual(self.invoke(method='POST'), (None, 0, 0))
        with patch.object(proxy, 'STOPPED_CONTAINERS', set()):
            self.assertEqual(self.invoke(), (None, 0, 0))
        with patch.object(proxy, 'OWNED_CONTAINERS', {'captured-id': 'new-life'}):
            self.assertEqual(self.invoke(), (None, 0, 0))
        (self.root / 'compute-arm.json').unlink()
        self.assertEqual(self.invoke(), (None, 0, 0))

    def test_foreign_operation_or_physical_target_is_never_attributed(self):
        for field in ('operation_id', 'app_id', 'lifecycle_id'):
            old = self.operation[field]
            self.operation[field] = 'foreign'
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.invoke()
            self.operation[field] = old
        self.actual['State']['Running'] = True
        with self.assertRaises(ValueError):
            self.invoke()
        self.actual['State']['Running'] = False
        self.actual['Config']['Labels']['rcoder.io/lifecycle-id'] = 'replacement-life'
        with self.assertRaises(ValueError):
            self.invoke()

    def test_only_exact_docker_not_found_proves_auto_remove(self):
        self.response.status = 404
        self.assertEqual(self.invoke('/containers/captured-id/json')[0], contract.MARKER)
        for status in (401, 403, 409, 500):
            self.response.status = status
            with self.subTest(status=status), self.assertRaises(ValueError):
                self.invoke()

    def test_exec_mutation_requires_captured_exec_and_live_owned_container(self):
        with patch.object(proxy, 'OWNED_EXECS', {'exec-owned': 'captured-id'}), \
                patch.object(proxy, 'container_owned', return_value=True) as owned:
            self.assertTrue(proxy.allowed('POST', '/v1.47/exec/exec-owned/start', b''))
            owned.assert_called_once_with('captured-id')
            self.assertFalse(proxy.allowed('POST', '/exec/foreign/start', b''))
        with patch.object(proxy, 'OWNED_EXECS', {'exec-owned': 'captured-id'}), \
                patch.object(proxy, 'container_owned', return_value=False):
            self.assertFalse(proxy.allowed('POST', '/exec/exec-owned/start', b''))
        with patch.object(proxy, 'container_owned', return_value=True):
            self.assertTrue(proxy.allowed('POST', '/containers/captured-id/exec', b'{"AttachStdin":false}'))
            self.assertFalse(proxy.allowed('POST', '/containers/captured-id/exec', b'{"AttachStdin":true}'))

    def test_wire_fault_returns_a_real_http_error_with_the_unique_diagnostic(self):
        from io import BytesIO
        envelope = BytesIO(json.dumps({'code': '0000', 'data': self.operation}).encode())
        path = self.root / 'proxy.sock'
        with proxy.Server(str(path), proxy.Handler) as server, \
                patch.object(proxy.urllib.request, 'urlopen', return_value=envelope), \
                patch.object(proxy, 'exchange', return_value=(self.connection, self.response)):
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                    channel.settimeout(3)
                    channel.connect(str(path))
                    channel.sendall(b'GET /v1.47/containers/captured-id/json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n')
                    chunks = []
                    while chunk := channel.recv(4096):
                        chunks.append(chunk)
                raw = b''.join(chunks)
                head, body = raw.split(b'\r\n\r\n', 1)
                self.assertTrue(head.startswith(b'HTTP/1.1 500 '))
                self.assertEqual(json.loads(body), {'message': contract.MARKER})
                self.assertEqual(self.closed, [True])
            finally:
                server.shutdown()
                worker.join(timeout=3)

    def test_owned_exec_upgrade_retains_raw_stream_and_upgrade_headers(self):
        captured = self.actual
        frame = b'\x01\x00\x00\x00\x00\x00\x00\x05hello'
        observed_headers = []
        class DockerHandler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'
            def log_message(self, *_args):
                pass
            def do_GET(self):
                payload = json.dumps(captured).encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length', '0')))
                observed_headers.append(dict(self.headers))
                self.send_response(101)
                self.send_header('Connection', 'Upgrade')
                self.send_header('Upgrade', 'tcp')
                self.end_headers()
                self.wfile.write(frame)
                self.wfile.flush()
                self.close_connection = True
        upstream, proxy_path = self.root / 'upstream.sock', self.root / 'wire.sock'
        with proxy.Server(str(upstream), DockerHandler) as docker, \
                proxy.Server(str(proxy_path), proxy.Handler) as server, \
                patch.object(proxy, 'UPSTREAM', str(upstream)), \
                patch.object(proxy, 'OWNED_EXECS', {'owned-exec': 'captured-id'}):
            workers = [threading.Thread(target=value.serve_forever, daemon=True) for value in (docker, server)]
            for worker in workers:
                worker.start()
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                    channel.settimeout(3)
                    channel.connect(str(proxy_path))
                    channel.sendall(b'POST /exec/owned-exec/start HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n{}')
                    chunks = []
                    while chunk := channel.recv(4096):
                        chunks.append(chunk)
                headers, payload = b''.join(chunks).split(b'\r\n\r\n', 1)
                self.assertTrue(headers.startswith(b'HTTP/1.1 101 '))
                self.assertIn(b'Connection: Upgrade', headers)
                self.assertEqual(payload, frame)
                self.assertEqual(observed_headers[0].get('Connection'), 'Upgrade')
                self.assertEqual(observed_headers[0].get('Upgrade'), 'tcp')
            finally:
                docker.shutdown()
                server.shutdown()
                for worker in workers:
                    worker.join(timeout=3)

    def test_transport_failure_closes_wire_and_audits_only_safe_context(self):
        secrets = ['private-query-value', 'private-header-value', 'private-body-value',
                   'private-environment-value', 'private-exception-value']
        path = self.root / 'failed-wire.sock'
        error = ConnectionResetError(errno.ECONNRESET, secrets[-1])
        with proxy.Server(str(path), proxy.Handler) as server, \
                patch.dict('os.environ', {'PRIVATE_TEST_SECRET': secrets[-2]}), \
                patch.object(proxy, 'compute_fault', side_effect=error), \
                patch.object(proxy, 'exchange') as exchange:
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                    channel.settimeout(3)
                    channel.connect(str(path))
                    body = secrets[2].encode()
                    request = ('GET /v1.47/containers/captured-id/json?token=' + secrets[0]
                               + ' HTTP/1.1\r\nHost: localhost\r\nAuthorization: ' + secrets[1]
                               + '\r\nContent-Length: ' + str(len(body))
                               + '\r\nConnection: close\r\n\r\n').encode() + body
                    channel.sendall(request)
                    self.assertEqual(channel.recv(4096), b'', 'transport errors must not fabricate HTTP success')
                exchange.assert_not_called()
                audit = self.root / 'proxy-errors.jsonl'
                records = audit.read_text()
                self.assertEqual(json.loads(records), {
                    'method': 'GET', 'path': '/v1.47/containers/captured-id/json',
                    'error_type': 'ConnectionResetError', 'errno': errno.ECONNRESET})
                self.assertEqual(audit.stat().st_mode & 0o777, 0o600)
                for secret in secrets:
                    self.assertNotIn(secret, records)
            finally:
                server.shutdown()
                worker.join(timeout=3)


if __name__ == '__main__':
    unittest.main()
