import contextlib
import http.server
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch
import docker_fault_proxy as proxy


class FaultProxyScopeTests(unittest.TestCase):
    def test_creation_requires_exact_name_family_owner_and_lifecycle(self):
        labels = {'rcoder.io/application-id': 'app',
                  'service-type': 'user-app-builder', 'rcoder.io/lifecycle-id': 'life'}
        with patch.multiple(proxy, APP='app', OWNER='owner'):
            path = '/v1.47/containers/create?name=rcoder-app-builder-app'
            self.assertTrue(proxy.allowed('POST', path, json.dumps({'Labels': labels})))
            self.assertFalse(proxy.allowed('POST', path.replace('builder-app', 'builder-other'), json.dumps({'Labels': labels})))
            for field in labels:
                wrong = dict(labels)
                wrong[field] = ''
                self.assertFalse(proxy.allowed('POST', path, json.dumps({'Labels': wrong})))
            self.assertFalse(proxy.allowed('POST', '/images/create', b''))
            self.assertFalse(proxy.allowed('DELETE', '/networks/foreign', b''))

    def test_container_write_is_bound_to_recorded_physical_id_and_lifecycle(self):
        from types import SimpleNamespace
        labels = {'rcoder.io/application-id': 'app',
                  'service-type': 'user-app-builder', 'rcoder.io/lifecycle-id': 'life'}
        row = {'Id': 'created-id', 'Config': {'Labels': labels}}
        closed = []
        response = SimpleNamespace(status=200, read=lambda: json.dumps(row).encode())
        connection = SimpleNamespace(close=lambda: closed.append(True))
        with patch.multiple(proxy, APP='app', OWNER='owner', OWNED_CONTAINERS={'created-id': 'life'}), patch.object(proxy, 'exchange', return_value=(connection, response)):
            self.assertTrue(proxy.container_owned('created-id'))
            for field in ('service-type', 'rcoder.io/lifecycle-id'):
                old = labels[field]
                labels[field] = 'foreign'
                self.assertFalse(proxy.container_owned('created-id'))
                labels[field] = old
            row['Id'] = 'replacement-id'
            self.assertFalse(proxy.container_owned('created-id'))
        self.assertEqual(len(closed), 4)


@unittest.skipUnless(hasattr(socket, 'AF_UNIX'), 'requires local Unix HTTP sockets')
class AfterStartExecProtocolTests(unittest.TestCase):
    """Real Unix HTTP relay to a stateful daemon fixture, not Docker acceptance."""
    app, life = 'crash-owned-app', 'crash-owned-life'
    cid, exec_id, preexisting, foreign = 'a' * 64, 'b' * 64, 'c' * 64, 'f' * 64
    frames = (b'\x01\x00\x00\x00\x00\x00\x00\x07stdout\n'
              b'\x02\x00\x00\x00\x00\x00\x00\x07stderr\n')

    def setUp(self):
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
        self.mutations, self.start_headers = [], []
        self.rows = {
            cid: {'Id': cid, 'Config': {'Labels': self.labels()}, 'State': {'Running': True}}
            for cid in (self.cid, self.preexisting, self.foreign)
        }
        self.rows[self.foreign]['Config']['Labels']['rcoder.io/application-id'] = 'another-app'
        case = self

        class Daemon(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *_args):
                pass

            def respond(self, status, payload):
                encoded = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

            def do_GET(self):
                path = proxy.urllib.parse.urlsplit(self.path).path
                if path.startswith('/v1.47/'):
                    path = path[len('/v1.47'):]
                if path.startswith('/containers/') and path.endswith('/json'):
                    row = case.rows.get(path.split('/')[2])
                    self.respond(200 if row else 404, row or {})
                elif path == '/exec/' + case.exec_id + '/json':
                    self.respond(200, {'ID': case.exec_id, 'ContainerID': case.cid,
                                       'Running': False, 'ExitCode': 23})
                else:
                    self.respond(404, {})

            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length', '0')))
                path = proxy.urllib.parse.urlsplit(self.path).path
                if path.startswith('/v1.47/'):
                    path = path[len('/v1.47'):]
                case.mutations.append((self.command, path))
                if path == '/containers/create':
                    self.respond(201, {'Id': case.cid, 'Warnings': []})
                elif path == '/containers/' + case.cid + '/exec':
                    self.respond(201, {'Id': case.exec_id})
                elif path == '/exec/' + case.exec_id + '/start':
                    case.start_headers.append(dict(self.headers))
                    self.send_response(101)
                    self.send_header('Connection', 'Upgrade')
                    self.send_header('Upgrade', 'tcp')
                    self.end_headers()
                    self.wfile.write(case.frames)
                    self.wfile.flush()
                    self.close_connection = True
                else:
                    self.respond(202, {'unexpected_mutation_reached_upstream': True})

            do_PUT = do_DELETE = do_PATCH = do_POST

        upstream, self.endpoint = self.root / 'daemon.sock', self.root / 'proxy.sock'
        self.stack.enter_context(patch.multiple(
            proxy, ROOT=self.root, APP=self.app, PROJECT='owned-project', MODE='after_start',
            UPSTREAM=str(upstream), GATED=True, GATE_LOCK=threading.Lock(),
            OWNED_CONTAINERS={}, OWNED_EXECS={}, OWNED_ALIASES={}, OWNED_AUTO_REMOVE={},
            STOPPED_CONTAINERS=set()))
        daemon = self.stack.enter_context(proxy.Server(str(upstream), Daemon))
        server = self.stack.enter_context(proxy.Server(str(self.endpoint), proxy.Handler))
        for running in (daemon, server):
            thread = threading.Thread(target=running.serve_forever,
                                      kwargs={'poll_interval': .01}, daemon=True)
            thread.start()
            self.stack.callback(thread.join, 3)
            self.stack.callback(running.shutdown)

    def labels(self):
        return {'rcoder.io/application-id': self.app,
                'rcoder.io/lifecycle-id': self.life, 'service-type': 'user-app-builder'}

    def wire(self, method, path, payload=None, upgrade=False):
        body = json.dumps(payload).encode() if payload is not None else b''
        request = (method + ' ' + path + ' HTTP/1.1\r\nHost: localhost\r\n'
                   + 'Content-Length: ' + str(len(body)) + '\r\nConnection: '
                   + ('Upgrade\r\nUpgrade: tcp' if upgrade else 'close') + '\r\n\r\n').encode() + body
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
            channel.settimeout(3)
            channel.connect(str(self.endpoint))
            channel.sendall(request)
            chunks = []
            while chunk := channel.recv(4096):
                chunks.append(chunk)
        headers, result = b''.join(chunks).split(b'\r\n\r\n', 1)
        return int(headers.split(b' ', 2)[1]), headers, result

    def create_container(self):
        status, _, payload = self.wire(
            'POST', '/v1.47/containers/create?name=rcoder-app-builder-' + self.app,
            {'Labels': self.labels()})
        self.assertEqual(status, 201)
        self.assertEqual(json.loads(payload)['Id'], self.cid)
        self.assertEqual(proxy.OWNED_CONTAINERS, {self.cid: self.life})

    def create_exec(self):
        status, _, payload = self.wire(
            'POST', '/v1.47/containers/' + self.cid + '/exec',
            {'AttachStdin': False, 'Tty': False, 'AttachStdout': True, 'AttachStderr': True,
             'Cmd': ['fixture-read']})
        self.assertEqual(status, 201)
        self.assertEqual(json.loads(payload)['Id'], self.exec_id)

    def test_created_owned_noninteractive_exec_relays_real_upgrade_frames_and_exit_status(self):
        self.create_container()
        self.create_exec()
        status, headers, output = self.wire(
            'POST', '/v1.47/exec/' + self.exec_id + '/start',
            {'Detach': False, 'Tty': False}, upgrade=True)
        self.assertEqual(status, 101)
        self.assertEqual(proxy.OWNED_EXECS, {self.exec_id: self.cid})
        self.assertIn(b'Connection: Upgrade', headers)
        self.assertIn(b'Upgrade: tcp', headers)
        self.assertEqual(output, self.frames)
        self.assertEqual(self.start_headers[0].get('Connection'), 'Upgrade')
        self.assertEqual(self.start_headers[0].get('Upgrade'), 'tcp')
        status, _, payload = self.wire('GET', '/v1.47/exec/' + self.exec_id + '/json')
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(payload), {'ID': self.exec_id, 'ContainerID': self.cid,
                                               'Running': False, 'ExitCode': 23})
        self.assertEqual(self.mutations, [
            ('POST', '/containers/create'), ('POST', '/containers/' + self.cid + '/exec'),
            ('POST', '/exec/' + self.exec_id + '/start')])

    def test_sdk_omitted_noninteractive_create_flags_keep_the_owned_relay(self):
        self.create_container()
        status, _, payload = self.wire(
            'POST', '/v1.47/containers/' + self.cid + '/exec',
            {'AttachStdout': True, 'AttachStderr': True, 'Cmd': ['fixture-read']})
        self.assertEqual(status, 201)
        self.assertEqual(json.loads(payload)['Id'], self.exec_id)
        status, _, output = self.wire(
            'POST', '/v1.47/exec/' + self.exec_id + '/start',
            {'Detach': False, 'Tty': False}, upgrade=True)
        self.assertEqual(status, 101)
        self.assertEqual(output, self.frames)

    def test_interactive_foreign_unrecorded_and_other_mutations_never_reach_upstream(self):
        self.create_container()
        self.create_exec()
        baseline = list(self.mutations)
        rejected = [
            ('POST', '/containers/' + self.cid + '/exec', {'AttachStdin': True}),
            ('POST', '/containers/' + self.cid + '/exec', {'Tty': True}),
            ('POST', '/containers/' + self.cid + '/exec', {'AttachStdin': 'false'}),
            ('POST', '/containers/' + self.preexisting + '/exec', {'AttachStdin': False}),
            ('POST', '/containers/' + self.foreign + '/exec', {'AttachStdin': False}),
            ('POST', '/exec/' + self.exec_id + '/start', {'Tty': True}),
            ('POST', '/exec/' + self.exec_id + '/start', {'AttachStdin': True}),
            ('POST', '/exec/' + self.exec_id + '/start', {'Detach': True}),
            ('POST', '/exec/unrecorded/start', {}),
            ('POST', '/exec/' + self.exec_id + '/resize', {}),
            ('PUT', '/exec/' + self.exec_id + '/start', {}),
            ('DELETE', '/exec/' + self.exec_id + '/start', {}),
            ('POST', '/containers/' + self.cid + '/exec/unexpected', {}),
            ('POST', '/images/create', {}),
            ('DELETE', '/volumes/foreign-data', None),
        ]
        for method, path, payload in rejected:
            with self.subTest(method=method, path=path, payload=payload):
                self.assertEqual(self.wire(method, path, payload)[0], 403)
                self.assertEqual(self.mutations, baseline)

    def test_recorded_exec_loses_authority_when_physical_lifecycle_changes(self):
        self.create_container()
        self.create_exec()
        baseline = list(self.mutations)
        for field in ('rcoder.io/application-id', 'rcoder.io/lifecycle-id', 'service-type'):
            labels = self.rows[self.cid]['Config']['Labels']
            original = labels[field]
            labels[field] = 'foreign-or-replaced'
            with self.subTest(field=field):
                self.assertEqual(self.wire('POST', '/exec/' + self.exec_id + '/start', {})[0], 403)
                self.assertEqual(self.mutations, baseline)
            labels[field] = original

    def test_unrelated_fault_modes_do_not_gain_exec_mutation_authority(self):
        self.create_container()
        self.create_exec()
        baseline = list(self.mutations)
        for mode in ('before_create', 'unsupported'):
            with patch.object(proxy, 'MODE', mode), self.subTest(mode=mode):
                for path in ('/containers/' + self.cid + '/exec', '/exec/' + self.exec_id + '/start'):
                    self.assertEqual(self.wire('POST', path, {})[0], 403)
                self.assertEqual(self.mutations, baseline)


if __name__ == '__main__':
    unittest.main()
