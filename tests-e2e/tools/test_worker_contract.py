"""Source fixture inputs only; real two-engine acceptance runs separately."""
import os
import http.server
import json
from pathlib import Path
import re
import runpy
import sys
import tempfile
import threading
import time
import tomllib
import unittest
import urllib.request
from unittest.mock import patch


class OperationServer:
    def __init__(self, rejection_count=2, post_error=None, existing=False,
                 owner='original-owner', accepted_on_error=False, recovery_delay=0,
                 recovery_initializing=0, post_delay=0):
        self.posts = []
        self.gets = []
        self.accepted = 0
        self.rejection_count = rejection_count
        self.post_error = post_error
        self.existing = existing
        self.owner = owner
        self.accepted_on_error = accepted_on_error
        self.recovery_delay = recovery_delay
        self.recovery_initializing = recovery_initializing
        self.post_delay = post_delay
        state = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, value):
                body = value if isinstance(value, bytes) else json.dumps(value).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def do_POST(self):
                raw = self.rfile.read(int(self.headers['Content-Length']))
                state.posts.append(raw)
                if state.post_error:
                    if state.accepted_on_error:
                        state.accepted += 1
                    self.reply(*state.post_error)
                elif len(state.posts) <= state.rejection_count:
                    self.reply(409, {'success': False, 'code': 'ERR_INITIALIZING', 'data': None})
                else:
                    state.accepted += 1
                    request = json.loads(raw)
                    operation = request['operation_id']
                    time.sleep(state.post_delay)
                    self.reply(202, {'success': True, 'code': 'OK', 'data': {
                        'operation_id': operation, 'state': 'accepted',
                        'poll': '/v1/runtime/operations/' + operation}})

            def do_GET(self):
                state.gets.append(self.path)
                if self.path == '/v1/runtime/identity':
                    self.reply(200, {'success': True, 'code': 'OK', 'data': {
                        'runtime_instance_id': state.owner, 'workspace_id': 'workspace'}})
                elif self.path == '/v1/runtime/recovery':
                    time.sleep(state.recovery_delay)
                    if state.recovery_initializing:
                        state.recovery_initializing -= 1
                        self.reply(503, {'success': False, 'code': 'ERR_INITIALIZING'})
                    else:
                        self.reply(200, {'success': True, 'code': 'OK', 'data': {
                            'runtime_instance_id': state.owner}})
                elif self.path.startswith('/v1/runtime/operations/'):
                    if state.existing:
                        self.reply(200, {'success': True, 'code': 'OK', 'data': {
                            'operation_id': self.path.rsplit('/', 1)[1],
                            'runtime_instance_id': state.owner, 'kind': 'deploy',
                            'request_digest': 'already-recorded', 'state': 'accepted'}})
                    else:
                        self.reply(404, {'success': False, 'code': 'ERR_NOT_FOUND'})
                else:
                    self.reply(404, {'success': False, 'code': 'ERR_NOT_FOUND'})

        self.http = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.http.daemon_threads = True
        self.thread = threading.Thread(
            target=lambda: self.http.serve_forever(poll_interval=.01), daemon=True)
        self.thread.start()
        self.base = f'http://127.0.0.1:{self.http.server_port}'

    def close(self):
        self.http.shutdown()
        self.http.server_close()
        self.thread.join(timeout=2)


class WorkerAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.report = tempfile.TemporaryDirectory()
        self.addCleanup(self.report.cleanup)
        with patch.dict(os.environ, {'E2E_REPORT_DIR': self.report.name,
                                     'E2E_RUN_ID': 'worker-admission'}), patch.dict(sys.modules):
            sys.modules.pop('hot_contract', None)
            self.module = runpy.run_path(str(Path(__file__).with_name('worker_contract.py')))
        self.h = self.module['h']
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        local_http = patch.object(self.h.urllib.request, 'urlopen', opener.open)
        local_http.start()
        self.addCleanup(local_http.stop)
        self.request = {
            'operation_id': 'original-operation', 'expected_runtime_instance_id': 'original-owner',
            'expected_revision': 7, 'workspace_id': 'workspace', 'kind': 'deploy',
            'profile': {'profile': 'source', 'input': {'workspace_id': 'workspace'}}}

    def server(self, **kwargs):
        server = OperationServer(**kwargs)
        self.addCleanup(server.close)
        return server

    def submit(self, server, timeout=2):
        return self.module['submit_operation'](
            server.base, self.request, time.monotonic() + timeout)

    def test_ready_read_then_two_zero_write_rejections_admits_the_original_body_once(self):
        server = self.server()
        self.assertEqual(self.h.request_http(server.base, '/v1/runtime/identity')[0], 200)
        self.assertEqual(self.h.request_http(server.base, '/v1/runtime/recovery')[0], 200)
        status, body = self.submit(server)
        self.assertEqual(status, 202, body)
        self.assertEqual(len(server.posts), 3)
        self.assertEqual(server.accepted, 1)
        self.assertTrue(all(raw == server.posts[0] for raw in server.posts))
        self.assertEqual(json.loads(server.posts[0]), self.request)
        operation = '/v1/runtime/operations/original-operation'
        self.assertEqual(server.gets[2:], [operation, '/v1/runtime/recovery', operation] * 2)

    def test_other_rejections_and_ambiguous_writes_are_never_reposted(self):
        failures = [
            (409, {'success': False, 'code': 'ERR_OPERATION_IN_PROGRESS'}),
            (409, {'success': False, 'code': 'ERR_REVISION_MISMATCH'}),
            (409, {'success': False, 'code': 'ERR_RECOVERY_REQUIRED'}),
            (409, {'success': True, 'code': 'ERR_INITIALIZING'}),
            (409, {'code': 'ERR_INITIALIZING'}),
            (409, {'success': False, 'code': 'ERR_INITIALIZING', 'data': {'operation_id': 'owned'}}),
            (409, b'not-json'),
            (503, {'success': False, 'code': 'ERR_INITIALIZING'}),
            (500, {'success': False, 'code': 'ERR_INITIALIZING'}),
            (500, {'success': False, 'code': 'ERR_BACKEND_ERROR'}),
        ]
        for status, body in failures:
            with self.subTest(status=status, body=body):
                server = self.server(post_error=(status, body), accepted_on_error=status == 500)
                result, _ = self.submit(server)
                self.assertEqual(result, status)
                self.assertEqual(len(server.posts), 1)
                self.assertEqual(server.gets, [])
                if status == 500:
                    self.assertEqual(server.accepted, 1, 'an ambiguous write must not be duplicated')

    def test_owner_change_or_existing_operation_stops_before_reposting(self):
        for kwargs in ({'owner': 'replacement-owner'}, {'existing': True}):
            with self.subTest(kwargs=kwargs):
                server = self.server(**kwargs)
                with self.assertRaises(AssertionError):
                    self.submit(server)
                self.assertEqual(len(server.posts), 1)
                self.assertEqual(server.accepted, 0)

    def test_readonly_initialization_waits_without_another_post_until_same_owner_is_ready(self):
        server = self.server(rejection_count=1, recovery_initializing=1)
        status, _ = self.submit(server)
        self.assertEqual(status, 202)
        self.assertEqual(len(server.posts), 2)
        self.assertEqual(server.accepted, 1)
        self.assertEqual(server.posts[0], server.posts[1])

    def test_ambiguous_accepted_receipt_does_not_authorize_another_post(self):
        server = self.server(post_error=(202, {'success': True, 'data': {
            'operation_id': 'different-operation', 'poll': '/wrong'}}), accepted_on_error=True)
        with self.assertRaises(AssertionError):
            self.submit(server)
        self.assertEqual(len(server.posts), 1)
        self.assertEqual(server.accepted, 1)

    def test_deadline_is_shared_across_rejections_and_does_not_turn_into_success(self):
        server = self.server(rejection_count=100)
        with self.assertRaises(TimeoutError):
            self.submit(server, timeout=.05)
        self.assertEqual(server.accepted, 0)
        attempts = len(server.posts)
        time.sleep(.02)
        self.assertEqual(len(server.posts), attempts)

    def test_expired_deadline_does_not_dispatch_a_first_post(self):
        server = self.server(rejection_count=0)
        with self.assertRaises(TimeoutError):
            self.submit(server, timeout=-1)
        self.assertEqual(server.posts, [])

    def test_slow_recovery_observation_cannot_extend_the_original_deadline(self):
        server = self.server(recovery_delay=.1)
        started = time.monotonic()
        with self.assertRaises((TimeoutError, OSError)):
            self.submit(server, timeout=.03)
        self.assertLess(time.monotonic() - started, .5)
        self.assertEqual(len(server.posts), 1)
        self.assertEqual(server.accepted, 0)

    def test_lost_reply_after_acceptance_is_not_replayed_when_deadline_expires(self):
        server = self.server(rejection_count=0, post_delay=.1)
        with self.assertRaises((TimeoutError, OSError)):
            self.submit(server, timeout=.03)
        self.assertEqual(len(server.posts), 1)
        self.assertEqual(server.accepted, 1)


class WorkerSourceInputTests(unittest.TestCase):
    def test_source_declares_actual_workspace_projects_and_runs_current_source(self):
        with tempfile.TemporaryDirectory() as report, patch.dict(
                os.environ, {'E2E_REPORT_DIR': report, 'E2E_RUN_ID': 'worker-source-inputs'}), \
                patch.dict(sys.modules):
            sys.modules.pop('hot_contract', None)
            module = runpy.run_path(str(Path(__file__).with_name('worker_contract.py')))
            files = module['source_files']()
        workspace = tomllib.loads(files['workspace.manifest.toml'])
        self.assertEqual(workspace['schema_version'], 1)
        self.assertNotIn('release.lock.toml', files)
        self.assertNotIn('web/dependency.js', files, 'deployed real symlink must remain intact')
        types = (Path(__file__).resolve().parents[2] / 'crates/workspace-manifest/src/types.rs').read_text()
        def fields(name):
            declaration = re.search(r'pub struct ' + name + r'\s*\{(.*?)\n\}', types, re.S)
            self.assertIsNotNone(declaration)
            return set(re.findall(r'^\s*pub (?:r#)?(\w+)\s*:', declaration.group(1), re.M))
        self.assertLessEqual(set(workspace), fields('WorkspaceManifest'))
        for name, probe in [('web', 'http'), ('worker', 'process')]:
            project = tomllib.loads(files[name + '/project.manifest.toml'])
            self.assertLessEqual(set(project), fields('ProjectManifest'))
            self.assertLessEqual(set(project['project']), fields('ProjectMeta'))
            self.assertEqual(project['project']['service_id'], name)
            self.assertEqual(project['health']['startup_probe'], probe)
            self.assertEqual(project['run']['command'], project['devrun']['command'])
            self.assertGreater(project['run']['shutdown_timeout_seconds'], 0)
        self.assertEqual(tomllib.loads(files['worker/project.manifest.toml'])['project']['kind'], 'worker')
        compile(files['worker/main.py'], 'worker/main.py', 'exec')
        self.assertIn('Number(process.env.PORT)', files['web/server.js'])
        self.assertIn("require('./dependency.js')", files['web/server.js'])


if __name__ == '__main__':
    unittest.main()
