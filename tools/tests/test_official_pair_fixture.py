"""Release-fixture diagnostics preserve real HTTP behavior and failure evidence."""
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request
import uuid
from unittest.mock import Mock, patch


SOURCE = Path(__file__).resolve().parents[1] / 'build/verify_official_pair.py'
spec = importlib.util.spec_from_file_location('official_pair_fixture', SOURCE)
pair = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pair)


class OfficialPairFixtureTests(unittest.TestCase):
    def owned_metadata(self, root, pid, port):
        """A scoped supervision protocol fixture, not a Rust-runtime assertion."""
        instance = pair.NativePair.__new__(pair.NativePair)
        instance.root, instance.workspace, instance.state = root, root / 'source', root / 'state'
        instance.workspace.mkdir()
        instance.state.mkdir()
        instance.logs = root / 'logs'
        instance.admin = port
        instance.owner = None
        instance.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        instance.result = {'error': 'original startup deadline', 'passed': False}
        supervisor, generation, guardian = [str(uuid.uuid4()) for _ in range(3)]
        (instance.workspace / 'release.lock.toml').write_text(
            f'[[services]]\nservice_id="web"\nport={port}\n', encoding='utf-8')
        (instance.state / 'supervisor.json').write_text(json.dumps({
            'instance': supervisor, 'token': 'DO-NOT-EXPORT-SUPERVISOR-TOKEN',
            'snapshot': {'supervisor_id': supervisor, 'generation': generation,
                         'binding': {'component': 'app-cli', 'resource': str(instance.workspace)}},
        }), encoding='utf-8')
        work = instance.state / 'work' / generation
        (work / 'guardians' / guardian).mkdir(parents=True)
        (work / 'generation.json').write_text(json.dumps({
            'version': 1, 'id': generation, 'supervisor': supervisor,
            'token': 'DO-NOT-EXPORT-GENERATION-TOKEN',
        }), encoding='utf-8')
        receipt = work / 'guardians' / guardian / 'receipt.json'
        receipt.write_text(json.dumps({
            'version': 2, 'id': guardian, 'instance_id': generation,
            'phase': 'Running', 'diagnostic_pid': pid, 'root_status': None,
            'command_digest': hashlib.sha256(b'owned protocol fixture').hexdigest(),
        }), encoding='utf-8')
        (root / 'owner.log').write_text(
            f'start web (web) on :{port} (pid={pid})\n'
            'APP-CLI-EVT {"event":"service_start_fail","service":"web","error":"protocol fixture startup failure"}\n',
            encoding='utf-8')
        return instance, receipt

    def test_business_fixture_emits_phases_and_serves_original_response(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            script = root / 'server.py'
            # A missing/hung reverse-DNS service must not prevent the real
            # loopback business listener from starting on a native CI host.
            script.write_text('import socket\n'
                'def forbidden_reverse_dns(*args, **kwargs):\n'
                ' raise RuntimeError("reverse DNS is unavailable in this fixture")\n'
                'socket.getfqdn=forbidden_reverse_dns\n' + pair.BUSINESS_SOURCE, encoding='utf-8')
            with socket.socket() as listener:
                listener.bind(('127.0.0.1', 0))
                port = listener.getsockname()[1]
            with (root / 'out.log').open('wb') as output:
                child = subprocess.Popen([sys.executable, str(script)],
                    env=dict(os.environ, PORT=str(port)), stdout=output, stderr=subprocess.STDOUT)
                try:
                    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                    deadline = time.monotonic() + 10
                    while True:
                        try:
                            with opener.open(f'http://127.0.0.1:{port}/health', timeout=1) as response:
                                self.assertEqual(response.status, 200)
                                self.assertEqual(response.read(), b'official-pair-business')
                            break
                        except OSError:
                            if time.monotonic() >= deadline:
                                self.fail('real fixture HTTP startup exceeded its unchanged 10s budget')
                            time.sleep(.05)
                finally:
                    child.terminate()
                    child.wait(timeout=5)
            text = (root / 'out.log').read_text(encoding='utf-8')
            for phase in ('python-entered', 'http-import', 'http-bind', 'http-listening'):
                self.assertIn('PAIR_FIXTURE_PHASE ' + phase, text)
            self.assertIn(sys.executable, text)

    def test_failure_snapshot_records_only_owned_inputs_and_preserves_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            instance = pair.NativePair.__new__(pair.NativePair)
            instance.root = root
            instance.workspace = root / 'source'
            instance.workspace.mkdir()
            (instance.workspace / 'workspace.manifest.toml').write_text('fixture input')
            instance.owner = Mock(pid=1234)
            instance.owner.poll.return_value = None
            instance.admin = 12345
            instance.http = Mock(side_effect=[(200, b'{"data":{"phase":"failed"}}'),
                OSError('owner status unavailable'), (503, b'not JSON')])
            instance.result = {'error': 'original startup deadline', 'passed': False}
            observed = subprocess.CompletedProcess(['ps'], 0, 'owned fixture process', '')
            with patch.object(pair.subprocess, 'run', return_value=observed), \
                 patch.object(pair.socket, 'create_connection', side_effect=ConnectionRefusedError('fixture')):
                instance.capture_failure_diagnostics()
            self.assertEqual(instance.result['error'], 'original startup deadline')
            self.assertFalse(instance.result['passed'])
            result = json.loads((root / 'failure-diagnostics.json').read_text())
            self.assertEqual(result['owner']['pid'], 1234)
            self.assertIsNone(result['owner']['exit_code'])
            self.assertEqual(result['owner']['os_observation']['stdout'], 'owned fixture process')
            self.assertEqual(result['inputs']['workspace.manifest.toml'], 'fixture input')
            self.assertEqual(result['observations']['/v1/runtime/identity']['status'], 200)
            self.assertIn('read_error', result['observations']['/v1/runtime/status'])
            self.assertNotIn('environment', result)
            self.assertTrue(all(call.kwargs['timeout'] == 2 for call in instance.http.call_args_list))

    def test_first_failure_snapshot_precedes_real_http_child_exit_and_survives_final_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ready = root / 'listening'
            script = root / 'short-http.py'
            script.write_text('''import http.server, pathlib, socketserver, sys
class Handler(http.server.BaseHTTPRequestHandler):
 def do_GET(self):
  body=b'controlled-startup-failure'
  self.send_response(503);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
  raise SystemExit(7)
server=socketserver.TCPServer(('127.0.0.1',0),Handler)
ready=pathlib.Path(sys.argv[1]);pending=ready.with_suffix('.pending')
pending.write_text(str(server.server_address[1]));pending.replace(ready)
server.serve_forever()
''', encoding='utf-8')
            with (root / 'http.log').open('wb') as output:
                child = subprocess.Popen([sys.executable, str(script), str(ready)],
                    stdout=output, stderr=subprocess.STDOUT)
                try:
                    deadline = time.monotonic() + 10
                    while not ready.is_file():
                        if child.poll() is not None or time.monotonic() >= deadline:
                            self.fail('owned HTTP failure fixture did not start')
                        time.sleep(.01)
                    port = int(ready.read_text())
                    instance, receipt = self.owned_metadata(root, child.pid, port)
                    instance.owner = child
                    first_seen = []

                    def original_business_predicate():
                        if not first_seen:
                            path = root / 'failure-diagnostics.json'
                            first_seen.append(json.loads(path.read_text()) if path.is_file() else None)
                            self.assertIsNone(child.poll(), 'snapshot must precede the real child exit')
                            status, body = instance.http(port, '/pair-business')
                            self.assertEqual((status, body), (503, b'controlled-startup-failure'))
                            self.assertEqual(child.wait(timeout=5), 7)
                            value = json.loads(receipt.read_text())
                            value.update(phase='Quiescent', root_status=7 << 8 if os.name == 'posix' else 7)
                            receipt.write_text(json.dumps(value), encoding='utf-8')
                        return False

                    with self.assertRaisesRegex(RuntimeError, 'wait exceeded'):
                        instance.wait(original_business_predicate, budget=.4)
                    self.assertIsNotNone(first_seen[0], 'first startup failure was lost until final cleanup')
                    first = first_seen[0]['first_startup_failure']
                    evidence = first['business']
                    self.assertTrue(evidence['binding_verified'])
                    self.assertFalse(evidence['pid_is_liveness_proof'])
                    self.assertEqual(evidence['commands'][0]['diagnostic_pid'], child.pid)
                    self.assertIsNone(evidence['commands'][0]['root_status'])
                    self.assertIn('guardian_id', evidence['commands'][0])
                    instance.http = Mock(side_effect=OSError('fixture owner unavailable'))
                    with patch.object(pair.socket, 'create_connection', side_effect=ConnectionRefusedError('fixture')):
                        instance.capture_failure_diagnostics()
                    final = json.loads((root / 'failure-diagnostics.json').read_text())
                    self.assertEqual(final['first_startup_failure'], first)
                    self.assertEqual(final['business']['commands'][0]['root_status'],
                                     7 << 8 if os.name == 'posix' else 7)
                    self.assertEqual(instance.result['error'], 'original startup deadline')
                    self.assertFalse(instance.result['passed'])
                    self.assertNotIn('DO-NOT-EXPORT', json.dumps(final))
                finally:
                    if child.poll() is None:
                        child.terminate()
                    child.wait(timeout=5)

    def test_log_pid_does_not_authorize_foreign_workspace_or_mismatched_guardian(self):
        for mismatch in ('workspace', 'guardian'):
            with self.subTest(mismatch=mismatch), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                instance, receipt = self.owned_metadata(root, os.getpid(), 4325)
                if mismatch == 'workspace':
                    path = instance.state / 'supervisor.json'
                    value = json.loads(path.read_text())
                    (root / 'another-workspace').mkdir()
                    value['snapshot']['binding']['resource'] = str(root / 'another-workspace')
                else:
                    path = receipt
                    value = json.loads(path.read_text())
                    value['id'] = str(uuid.uuid4())
                path.write_text(json.dumps(value), encoding='utf-8')
                with self.assertRaisesRegex(RuntimeError, 'wait exceeded'):
                    instance.wait(lambda: False, budget=.01)
                diagnostic = json.loads((root / 'failure-diagnostics.json').read_text())
                evidence = diagnostic['first_startup_failure']['business']
                self.assertFalse(evidence['binding_verified'])
                self.assertEqual(evidence['commands'], [])
                self.assertIn('differs', evidence['read_error'])
                self.assertNotIn('DO-NOT-EXPORT', json.dumps(diagnostic))

    def test_diagnostic_failure_cannot_replace_business_verdict_or_original_error(self):
        instance = pair.NativePair.__new__(pair.NativePair)
        instance.result = {'error': 'original startup error', 'passed': False}
        instance.observe_startup_failure = Mock(side_effect=RuntimeError('diagnostic storage unavailable'))
        self.assertTrue(instance.wait(lambda: True, budget=.1))
        self.assertEqual(instance.result['error'], 'original startup error')
        self.assertIn('diagnostic storage unavailable', instance.result['diagnostic_error'])


if __name__ == '__main__':
    unittest.main()
