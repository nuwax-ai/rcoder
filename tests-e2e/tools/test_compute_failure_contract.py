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
import time
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


class StopTimeoutContractTests(unittest.TestCase):
    def test_mode_and_immutable_offline_observer_are_required_before_fixture_work(self):
        self.assertEqual(contract.failure_mode({}), 'stopped_read')
        self.assertEqual(contract.failure_mode({'E2E_COMPUTE_FAILURE_MODE': 'stop_reply_timeout'}), 'stop_reply_timeout')
        self.assertIsNone(contract.required_checkpoint_observer({}, 'stopped_read'))
        for mode in ('', 'compute_stopped', 'future_mode'):
            with self.subTest(mode=mode), self.assertRaises(ValueError):
                contract.failure_mode({'E2E_COMPUTE_FAILURE_MODE': mode})
        for value in (None, '', 'relative-reader', 'a' * 63, 'g' * 64):
            with self.subTest(value=value), self.assertRaises(ValueError):
                contract.required_checkpoint_observer({'rcoder': {'observer_sha256': value}}, 'stop_reply_timeout')
        self.assertEqual(contract.required_checkpoint_observer({'rcoder': {'observer_sha256': 'a' * 64}},
                                                               'stop_reply_timeout'), 'a' * 64)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / 'reports'
            receipt = Path(temporary) / 'build.json'
            source = {'origin_head': 'frozen', 'worktree_sha256': 'a' * 64}
            build = {'version': 1, 'source': source,
                     'rcoder': {'image_id': 'sha256:' + 'b' * 64, 'binary_sha256': 'c' * 64},
                     'builder': {'image_id': 'sha256:' + 'd' * 64,
                                 'agent_runner_sha256': 'e' * 64, 'app_cli_sha256': 'f' * 64}}
            receipt.write_text(json.dumps(build))
            environment = {'E2E_REPORT_DIR': str(directory), 'E2E_COMPUTE_BUILD_RECEIPT': str(receipt),
                           'E2E_COMPUTE_APP_ENTRY_PORT': '31234', 'E2E_COMPUTE_FAILURE_MODE': 'stop_reply_timeout'}
            with patch.dict('os.environ', environment, clear=True), \
                    patch.object(contract, 'source_identity', return_value=source), \
                    patch.object(contract, 'command') as docker:
                with self.assertRaisesRegex(ValueError, 'observer_sha256'):
                    contract.main()
                docker.assert_not_called()
                self.assertFalse(directory.exists())

    def test_mutation_audit_counts_stop_and_forbidden_starts_not_exec_or_network(self):
        rows = [
            {'method': 'POST', 'path': '/v1.47/containers/owned/stop?t=30'},
            {'method': 'POST', 'path': '/exec/owned-exec/start'},
            {'method': 'POST', 'path': '/networks/create'},
            {'method': 'GET', 'path': '/containers/owned/json'},
        ]
        self.assertEqual(contract.compute_write_counts(rows), (1, 0))
        for suffix in ('/start', '/restart'):
            self.assertEqual(contract.compute_write_counts(rows + [{'method': 'POST', 'path': '/containers/owned' + suffix}]), (1, 1))
        self.assertEqual(contract.compute_write_counts(rows + [{'method': 'POST', 'path': '/v1.47/containers/create?name=owned'}]), (1, 1))

    def test_offline_checkpoint_requires_original_operation_target_executor_and_lease(self):
        record = {'app_id': 'app', 'lifecycle_id': 'life', 'operation_id': 'op',
                  'request_id': 'request', 'scope': 'Dev', 'action': 'restart',
                  'state': 'recovery_required', 'stage': 'stopping', 'executor_id': 'executor',
                  'request_fingerprint': 'a' * 64, 'error_code': 'ERR_BACKEND_ERROR',
                  'error_message': 'Docker request timed out',
                  'checkpoint': {'context': {'app_id': 'app', 'lifecycle_id': 'life',
                      'operation_id': 'op', 'executor_id': 'executor', 'request_fingerprint': 'a' * 64},
                      'workload': {'uid': 'captured-id'}},
                  'lease': {'runtime': 'docker', 'inode': 123, 'token': 'executor'}}
        snapshot = {'version': 1, 'read_only': True, 'controller_stopped': True, 'record': record}
        kwargs = {'app': 'app', 'lifecycle': 'life', 'operation': 'op', 'request': 'request', 'captured_id': 'captured-id'}
        before = copy.deepcopy(snapshot)
        self.assertEqual(contract.validate_timeout_checkpoint(snapshot, **kwargs), record)
        diagnostic = (record['error_code'], record['error_message'])
        self.assertEqual(contract.validate_timeout_checkpoint(snapshot, **kwargs, original_error=diagnostic), record)
        with self.assertRaisesRegex(ValueError, 'original failure diagnostic'):
            contract.validate_timeout_checkpoint(snapshot, **kwargs, original_error=(diagnostic[0], 'replaced error'))
        self.assertEqual(snapshot, before, 'observation must not repair the durable checkpoint')
        mutations = [
            ('app_id', 'foreign'), ('lifecycle_id', 'foreign'), ('operation_id', 'foreign'),
            ('request_id', 'new-request'), ('scope', 'Prod'), ('action', 'stop'),
            ('state', 'succeeded'), ('stage', 'stopped'), ('checkpoint', None),
            ('checkpoint', []), ('lease', {}), ('lease', {'runtime': 'docker', 'inode': True, 'token': 'executor'}),
        ]
        for key, value in mutations:
            changed = copy.deepcopy(snapshot); changed['record'][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                contract.validate_timeout_checkpoint(changed, **kwargs)
        for key in ('app_id', 'lifecycle_id', 'operation_id', 'executor_id', 'request_fingerprint'):
            changed = copy.deepcopy(snapshot); changed['record']['checkpoint']['context'][key] = 'foreign'
            with self.subTest(context=key), self.assertRaises(ValueError):
                contract.validate_timeout_checkpoint(changed, **kwargs)
        changed = copy.deepcopy(snapshot); changed['record']['checkpoint']['workload']['uid'] = 'replacement'
        with self.assertRaises(ValueError):
            contract.validate_timeout_checkpoint(changed, **kwargs)
        for key in ('read_only', 'controller_stopped'):
            changed = copy.deepcopy(snapshot); changed[key] = False
            with self.subTest(key=key), self.assertRaises(ValueError):
                contract.validate_timeout_checkpoint(changed, **kwargs)

    def test_reply_cleanup_does_not_overwrite_an_already_published_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.assertIsNone(contract.settle_stop_reply(root, 'discard', contract.PhaseDeadline(1, 'unused')))
            self.assertEqual(list(root.iterdir()), [])
            (root / 'compute-stop-held.json').write_text('{}')
            contract.publish_fault_config(root / 'compute-stop-release.json', {'action': 'continue'})
            (root / 'compute-stop-settled.json').write_text('{"response_released":true}')
            before = (root / 'compute-stop-release.json').read_bytes()
            self.assertEqual(contract.settle_stop_reply(root, 'discard', contract.PhaseDeadline(1, 'settled')), {'response_released': True})
            self.assertEqual((root / 'compute-stop-release.json').read_bytes(), before)

    def test_cleanup_confirms_executor_stopped_before_releasing_its_held_reply(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve();run, case = 'run', 'a' * 32
            project = 'rcoder-cfailure-' + 'b' * 16
            labels = {'com.docker.compose.project': project, 'rcoder.e2e.run': run,
                      'rcoder.e2e.case': case, 'com.docker.compose.service': 'rcoder'}
            row = {'Id': 'controller', 'Name': '/' + project + '-rcoder-1',
                   'Config': {'Labels': labels}, 'State': {'Running': False}}
            receipt = {'root': str(root), 'run_id': run, 'case_id': case, 'project': project,
                       'app_id': 'cf' + case[:10] + 'c' * 10, 'creation_started': True,
                       'fault_mode': 'stop_reply_timeout', 'volume_names': []}
            (root / 'ownership.json').write_text(json.dumps(receipt));calls = []
            def command(*args, **kwargs):
                calls.append(args)
                if args[:3] == ('docker', 'ps', '-aq'):return 'controller'
                if args[:2] == ('docker', 'inspect'):return json.dumps([row])
                return ''
            def settle(*args):
                self.assertTrue(any(call[-2:] == ('stop', 'rcoder') for call in calls))
                self.assertEqual(args[:2], (root / 'control', 'discard'))
            with patch.object(contract, 'command', side_effect=command), \
                    patch.object(contract, 'builder_rows', return_value=[]), \
                    patch.object(contract, 'settle_stop_reply', side_effect=settle) as drained:
                self.assertTrue(contract.cleanup(root, run, case)['ok'])
                drained.assert_called_once()
            row['State']['Running'] = True
            with patch.object(contract, 'command', side_effect=command), \
                    patch.object(contract, 'builder_rows', return_value=[]), \
                    patch.object(contract, 'settle_stop_reply') as drained:
                with self.assertRaises(RuntimeError):contract.cleanup(root, run, case)
                drained.assert_not_called()


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


class ComputeStopReplyTimeoutTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.arm = {'container_id': 'captured-id', 'lifecycle_id': 'life'}
        contract.publish_fault_config(self.root / 'compute-arm.json', self.arm)
        contract.publish_fault_config(self.root / 'compute-operation.json', {'operation_id': 'op'})
        self.operation = {'app_id': 'app', 'lifecycle_id': 'life', 'operation_id': 'op',
                          'state': 'running', 'stage': 'stopping'}
        self.patch = patch.multiple(proxy, ROOT=self.root, APP='app', MODE='compute_stop_timeout',
            OWNED_CONTAINERS={'captured-id': 'life'}, STOPPED_CONTAINERS={'captured-id'})
        self.patch.start()

    def tearDown(self):
        self.patch.stop()
        self.temporary.cleanup()

    def operation_response(self, *_args, **_kwargs):
        from io import BytesIO
        return BytesIO(json.dumps({'code': '0000', 'data': self.operation}).encode())

    def test_wrong_phase_or_identity_cannot_borrow_the_stop_reply_barrier(self):
        for key, value in [('app_id', 'foreign'), ('lifecycle_id', 'foreign'),
                           ('operation_id', 'foreign'), ('stage', 'stopped'),
                           ('state', 'recovery_required')]:
            previous = self.operation[key];self.operation[key] = value
            with self.subTest(field=key), patch.object(proxy.urllib.request, 'urlopen', self.operation_response), \
                    self.assertRaises(ValueError):
                proxy.hold_compute_stop_reply('POST', '/v1.47/containers/captured-id/stop?t=30', 204)
            self.operation[key] = previous
        self.assertFalse((self.root / 'compute-stop-held.json').exists())

    def test_only_the_exact_successful_forwarded_stop_can_hold_a_reply(self):
        with patch.object(proxy.urllib.request, 'urlopen') as observed:
            for method, path in [('GET', '/containers/captured-id/json'),
                                 ('POST', '/containers/captured-id/start')]:
                self.assertIsNone(proxy.hold_compute_stop_reply(method, path, 204))
            observed.assert_not_called()
        arm = (self.root / 'compute-arm.json').read_bytes()
        (self.root / 'compute-arm.json').unlink()
        with patch.object(proxy.urllib.request, 'urlopen') as observed:
            self.assertIsNone(proxy.hold_compute_stop_reply('POST', '/containers/captured-id/stop', 204))
            observed.assert_not_called()
        (self.root / 'compute-arm.json').write_bytes(arm)
        for status in (200, 404, 500):
            with self.subTest(status=status), self.assertRaises(ValueError):
                proxy.hold_compute_stop_reply('POST', '/containers/captured-id/stop', status)
        with patch.object(proxy, 'STOPPED_CONTAINERS', set()), self.assertRaises(ValueError):
            proxy.hold_compute_stop_reply('POST', '/containers/captured-id/stop', 204)
        with patch.object(proxy, 'OWNED_CONTAINERS', {'captured-id': 'replacement-life'}), self.assertRaises(ValueError):
            proxy.hold_compute_stop_reply('POST', '/containers/captured-id/stop', 204)

    def test_second_stop_cannot_overwrite_the_original_held_operation(self):
        held = self.root / 'compute-stop-held.json';held.write_text('{"original":"retained"}')
        before = held.read_bytes()
        with patch.object(proxy.urllib.request, 'urlopen', self.operation_response), self.assertRaises(ValueError):
            proxy.hold_compute_stop_reply('POST', '/containers/captured-id/stop', 204)
        self.assertEqual(held.read_bytes(), before)
        self.assertFalse((self.root / 'compute-stop-settled.json').exists())

    def test_expired_hold_retains_the_upstream_receipt_without_fabricating_release(self):
        with patch.object(proxy.urllib.request, 'urlopen', self.operation_response), \
                patch.object(proxy.time, 'monotonic', side_effect=[0, 1, 2, 183, 184]), \
                self.assertRaises(TimeoutError):
            proxy.hold_compute_stop_reply('POST', '/containers/captured-id/stop', 204)
        held = json.loads((self.root / 'compute-stop-held.json').read_text())
        settled = json.loads((self.root / 'compute-stop-settled.json').read_text())
        self.assertTrue(held['upstream_completed'])
        self.assertFalse(settled['response_released'])
        self.assertNotIn('release_action', settled)
        self.assertFalse((self.root / 'compute-stop-release.json').exists())

    def test_wire_forwards_stop_then_withholds_all_headers_until_explicit_release(self):
        self.wire_release('continue')

    def test_wire_cleanup_discards_only_the_completed_reply_without_sending_success(self):
        self.wire_release('discard')

    def test_denied_start_is_still_audited_so_proxy_guards_cannot_hide_dispatches(self):
        endpoint = self.root / 'denied.sock'
        with proxy.Server(str(endpoint), proxy.Handler) as server, \
                patch.object(proxy, 'allowed', return_value=False), \
                patch.object(proxy, 'exchange') as upstream:
            worker = threading.Thread(target=server.serve_forever, daemon=True);worker.start()
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                    channel.settimeout(3);channel.connect(str(endpoint))
                    channel.sendall(b'POST /containers/foreign/start?token=private HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
                    chunks = []
                    while chunk := channel.recv(4096):chunks.append(chunk)
                    self.assertTrue(b''.join(chunks).startswith(b'HTTP/1.1 403 '))
                upstream.assert_not_called()
                audit = self.root / 'compute-writes.jsonl'
                records = [json.loads(s) for s in audit.read_text().splitlines()]
                self.assertEqual(len(records), 1)
                self.assertEqual({key: value for key, value in records[0].items() if key != 'request_id'},
                    {'method': 'POST', 'path': '/containers/foreign/start', 'query': {}})
                self.assertRegex(records[0]['request_id'], r'^[0-9a-f]{32}$')
                result = json.loads((self.root / 'compute-write-results.jsonl').read_text())
                self.assertEqual(result, {'request_id': records[0]['request_id'], 'outcome': 'denied', 'status': 403})
                self.assertEqual(contract.compute_write_counts(records), (0, 1))
                self.assertNotIn('private', audit.read_text())
                self.assertEqual(audit.stat().st_mode & 0o777, 0o600)
            finally:
                server.shutdown();worker.join(timeout=3)

    def wire_release(self, action):
        captured = {'Id': 'captured-id', 'Config': {'Labels': {
            'rcoder.io/application-id': 'app', 'rcoder.io/lifecycle-id': 'life',
            'service-type': 'user-app-builder'}}, 'State': {'Running': True}}
        writes = []
        class Upstream(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'
            def log_message(self, *_args):
                pass
            def do_GET(self):
                payload = json.dumps(captured).encode();self.send_response(200)
                self.send_header('Content-Length', str(len(payload)));self.end_headers();self.wfile.write(payload)
            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length', '0')))
                writes.append(self.path);captured['State']['Running'] = False
                self.send_response(204);self.send_header('Content-Length', '0');self.end_headers()
        upstream, endpoint = self.root / 'upstream.sock', self.root / 'proxy.sock'
        with proxy.Server(str(upstream), Upstream) as docker, \
                proxy.Server(str(endpoint), proxy.Handler) as server, \
                patch.object(proxy, 'UPSTREAM', str(upstream)), \
                patch.object(proxy.urllib.request, 'urlopen', self.operation_response):
            workers = [threading.Thread(target=value.serve_forever, daemon=True) for value in (docker, server)]
            for worker in workers:worker.start()
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                    channel.settimeout(.1);channel.connect(str(endpoint))
                    channel.sendall(b'POST /v1.47/containers/captured-id/stop?t=30 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
                    with self.assertRaises(socket.timeout):
                        channel.recv(4096)
                    deadline = time.monotonic() + 3
                    while not (self.root / 'compute-stop-held.json').exists():
                        self.assertLess(time.monotonic(), deadline);time.sleep(.01)
                    self.assertEqual(writes, ['/v1.47/containers/captured-id/stop?t=30'])
                    self.assertFalse(captured['State']['Running'])
                    held = json.loads((self.root / 'compute-stop-held.json').read_text())
                    self.assertEqual(held['upstream_status'], 204)
                    self.assertEqual(held['operation']['stage'], 'stopping')
                    self.assertFalse(held['response_released'])
                    contract.publish_fault_config(self.root / 'compute-stop-release.json', {'action': action})
                    channel.settimeout(3);chunks = []
                    while chunk := channel.recv(4096):chunks.append(chunk)
                    raw = b''.join(chunks)
                    if action == 'continue':
                        self.assertTrue(raw.startswith(b'HTTP/1.1 204 '))
                        self.assertEqual(raw.split(b'\r\n\r\n', 1)[1], b'')
                    else:self.assertEqual(raw, b'', 'discard must not fabricate any HTTP success')
                settled = json.loads((self.root / 'compute-stop-settled.json').read_text())
                self.assertEqual(settled['release_action'], action)
                self.assertEqual(settled['response_released'], action == 'continue')
                self.assertEqual((self.root / 'compute-stop-held.json').stat().st_mode & 0o777, 0o600)
                self.assertEqual((self.root / 'compute-stop-settled.json').stat().st_mode & 0o777, 0o600)
                mutations = [json.loads(s) for s in (self.root / 'compute-writes.jsonl').read_text().splitlines()]
                self.assertEqual(contract.compute_write_counts(mutations), (1, 0))
            finally:
                if not (self.root / 'compute-stop-release.json').exists():
                    contract.publish_fault_config(self.root / 'compute-stop-release.json', {'action': 'discard'})
                docker.shutdown();server.shutdown()
                for worker in workers:worker.join(timeout=3)


class FullTimeoutWriteAuditTests(unittest.TestCase):
    old, new, app, life = 'a' * 64, 'b' * 64, 'owned-app', 'owned-life'

    def sequence(self, *, replacement=True, remove=True):
        writes, results = [], []
        target = self.new if replacement else self.old
        def add(method, path, status, **fields):
            key = str(len(writes))
            query = fields.pop('query', {})
            writes.append({'request_id': key, 'method': method, 'path': path, 'query': query})
            results.append({'request_id': key, 'outcome': 'upstream', 'status': status, **fields})
        add('POST', '/v1.47/containers/' + self.old + '/stop', 204, container_id=self.old)
        if replacement:
            if remove:
                add('DELETE', '/containers/' + self.old, 204, container_id=self.old, query={'v': ['false']})
            add('POST', '/containers/create', 201, created_container_id=self.new,
                query={'name': ['rcoder-app-builder-' + self.app]},
                creation={'rcoder.io/application-id': self.app, 'rcoder.io/lifecycle-id': self.life,
                          'service-type': 'user-app-builder'})
        add('POST', '/containers/' + target + '/start', 204, container_id=target)
        add('POST', '/containers/rcoder-app-builder-' + self.app + '/exec', 201,
            container_id=target, created_exec_id='owned-exec')
        add('POST', '/exec/owned-exec/start', 101, container_id=target)
        return writes, results

    def validate(self, writes, results, replacement=True):
        return contract.validate_timeout_write_window(writes, results, old_id=self.old,
            new_id=self.new if replacement else self.old, app=self.app, lifecycle=self.life)

    def test_complete_window_accepts_one_exact_remove_fence_and_bound_exec(self):
        writes, results = self.sequence()
        audit = self.validate(writes, results)
        self.assertEqual(audit['counts'], {'stop': 1, 'create': 1, 'start': 1, 'remove': 1,
            'exec_create': 1, 'exec_start': 1})
        self.assertTrue(audit['old_cid_remove_fence'])
        self.assertEqual(audit['sdk_retry_allowances'], [])

    def test_retained_container_and_acknowledged_autoremove_paths_remain_distinct(self):
        for replacement, remove in ((False, False), (True, False)):
            with self.subTest(replacement=replacement):
                writes, results = self.sequence(replacement=replacement, remove=remove)
                self.assertFalse(self.validate(writes, results, replacement)['old_cid_remove_fence'])

    def test_late_stop_replay_or_duplicate_create_start_is_not_a_sdk_exemption(self):
        for suffix in ('/stop', '/start', '/create'):
            writes, results = self.sequence()
            index = next(i for i, row in enumerate(writes) if row['path'].endswith(suffix))
            replay, reply = copy.deepcopy(writes[index]), copy.deepcopy(results[index])
            replay['request_id'] = reply['request_id'] = 'replayed'
            writes.append(replay);results.append(reply)
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                self.validate(writes, results)

    def test_remove_must_use_full_old_id_and_explicit_false_volume_flag(self):
        for changes in ({'path': '/containers/rcoder-app-builder-' + self.app},
                        {'path': '/containers/' + self.new}, {'query': {'v': ['true']}},
                        {'query': {}}, {'query': {'v': ['false', 'true']}}):
            writes, results = self.sequence()
            writes[1].update(changes)
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.validate(writes, results)

    def test_foreign_create_start_stop_and_exec_targets_are_rejected(self):
        for index, field, value in ((0, 'container_id', self.new), (2, 'created_container_id', 'c' * 64),
                                    (3, 'container_id', self.old), (4, 'container_id', 'c' * 64),
                                    (5, 'container_id', self.old)):
            writes, results = self.sequence()
            results[index][field] = value
            with self.subTest(index=index), self.assertRaises(ValueError):
                self.validate(writes, results)

    def test_missing_denied_unknown_and_duplicate_result_cannot_pass(self):
        for outcome in ('missing', 'denied', 'transport_error', 'duplicate'):
            writes, results = self.sequence()
            if outcome == 'missing':results.pop()
            elif outcome == 'duplicate':results.append(copy.deepcopy(results[0]))
            else:results[-1]['outcome'] = outcome
            with self.subTest(outcome=outcome), self.assertRaises(ValueError):
                self.validate(writes, results)

    def test_restart_network_or_other_mutation_is_not_part_of_original_recovery(self):
        for method, path in (('POST', '/containers/' + self.new + '/restart'),
                             ('POST', '/networks/create'), ('DELETE', '/volumes/customer-data')):
            writes, results = self.sequence()
            writes.append({'request_id': 'foreign', 'method': method, 'path': path, 'query': {}})
            results.append({'request_id': 'foreign', 'outcome': 'upstream', 'status': 204})
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.validate(writes, results)

    def test_exec_without_receipt_or_replayed_exec_start_is_rejected(self):
        for mode in ('unknown-exec', 'replay'):
            writes, results = self.sequence()
            if mode == 'unknown-exec':writes[-1]['path'] = '/exec/unknown/start'
            else:
                replay, reply = copy.deepcopy(writes[-1]), copy.deepcopy(results[-1])
                replay['request_id'] = reply['request_id'] = 'replayed-exec'
                writes.append(replay);results.append(reply)
            with self.subTest(mode=mode), self.assertRaises(ValueError):self.validate(writes, results)

    def test_replacement_labels_and_reserved_name_must_match_original_lifecycle(self):
        for mode in ('app', 'life', 'name'):
            writes, results = self.sequence()
            if mode == 'name':writes[2]['query']['name'] = ['foreign']
            else:results[2]['creation']['rcoder.io/' + ('application-id' if mode == 'app' else 'lifecycle-id')] = 'foreign'
            with self.subTest(mode=mode), self.assertRaises(ValueError):self.validate(writes, results)


if __name__ == '__main__':
    unittest.main()
