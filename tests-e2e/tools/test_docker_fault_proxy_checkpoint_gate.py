"""Real private Unix HTTP requests; never connect to Docker or run a scenario."""
import errno
import http.client
import http.server
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch

import docker_fault_proxy as proxy


@unittest.skipUnless(hasattr(socket, 'AF_UNIX'), 'fault proxy uses Unix sockets')
class CheckpointReadGateTests(unittest.TestCase):
    def setUp(self):
        # Short /tmp paths also fit macOS's Unix socket path limit.
        self.directory = tempfile.TemporaryDirectory(prefix='rcoder-fproxy-', dir='/tmp')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.arm = self.root / 'compute-arm.json'
        self.upstream_requests = []
        self.errors = []
        self.proxy_socket = self.root / 'proxy.sock'
        self.upstream_socket = self.root / 'upstream.sock'
        self.context = patch.multiple(
            proxy, ROOT=self.root, MODE='compute_stop_timeout', APP='owned-app',
            OWNER='fixture-owner', PROJECT='fixture-project',
            UPSTREAM=str(self.upstream_socket), PROXY=str(self.proxy_socket),
        )
        self.context.start()
        self.addCleanup(self.context.stop)
        requests = self.upstream_requests

        class Upstream(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def do_GET(self):
                requests.append(self.path)
                body = b'OK' if self.path.endswith('/_ping') else b'{}'
                self.send_response(200)
                self.send_header('Content-Length', str(len(body)))
                self.send_header('Connection', 'close')
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        for address, handler in [(self.upstream_socket, Upstream),
                                 (self.proxy_socket, proxy.Handler)]:
            server = proxy.Server(str(address), handler)
            thread = threading.Thread(target=server.serve_forever,
                                      kwargs={'poll_interval': 0.01}, daemon=True)
            thread.start()
            self.addCleanup(self.close_server, server, thread)
        original_audit = proxy.audit_proxy_error

        def record_error(method, path, error):
            self.errors.append({'method': method, 'path': path,
                                'type': type(error).__name__,
                                'errno': getattr(error, 'errno', None),
                                'filename': getattr(error, 'filename', None)})
            original_audit(method, path, error)

        self.audit_patch = patch.object(proxy, 'audit_proxy_error', record_error)
        self.audit_patch.start()
        self.addCleanup(self.audit_patch.stop)

    @staticmethod
    def close_server(server, thread):
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    def request(self, path):
        connection = http.client.HTTPConnection('localhost', timeout=2)
        connection.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        connection.sock.settimeout(2)
        connection.sock.connect(str(self.proxy_socket))
        try:
            connection.request('GET', path)
            response = connection.getresponse()
            return response.status, response.read(), None
        except (OSError, http.client.HTTPException) as error:
            return None, None, type(error).__name__
        finally:
            connection.close()

    def revoke_after_exists(self):
        original_read = Path.read_text

        def read(path, *args, **kwargs):
            # The old helper has already observed exists=True. Withdraw the
            # real private file just before its actual open, without replacing
            # the resulting OS exception or any proxy/upstream response.
            if path == self.arm:
                path.unlink(missing_ok=True)
            return original_read(path, *args, **kwargs)

        return patch.object(Path, 'read_text', read)

    def test_health_ping_is_not_broken_by_arm_revocation(self):
        for path in ['/_ping', '/v1.47/_ping']:
            with self.subTest(path=path):
                self.arm.write_text('{"observe_timeout_checkpoint":true}')
                with self.revoke_after_exists():
                    status, body, error = self.request(path)
                self.assertIsNone(error, f'{self.errors}; upstream={self.upstream_requests}')
                self.assertEqual((status, body), (200, b'OK'))
                self.assertIn(path, self.upstream_requests)
        self.assertEqual(self.errors, [])

    def test_health_ping_does_not_read_unrelated_invalid_arm(self):
        self.arm.write_text('not-json')
        status, body, error = self.request('/_ping')
        self.assertIsNone(error, f'{self.errors}; upstream={self.upstream_requests}')
        self.assertEqual((status, body), (200, b'OK'))
        self.assertEqual(self.errors, [])

    def test_target_inspect_treats_revoked_arm_as_unarmed(self):
        self.arm.write_text('{"observe_timeout_checkpoint":true}')
        with self.revoke_after_exists():
            status, body, error = self.request('/containers/captured-id/json')
        self.assertIsNone(error, f'{self.errors}; upstream={self.upstream_requests}')
        self.assertEqual((status, body), (200, b'{}'))
        self.assertEqual(self.upstream_requests, ['/containers/captured-id/json'])
        self.assertEqual(self.errors, [])

    def test_target_invalid_json_is_not_silently_ignored(self):
        self.arm.write_text('not-json')
        status, _body, error = self.request('/containers/captured-id/json')
        self.assertIsNone(status)
        self.assertEqual(error, 'RemoteDisconnected')
        self.assertEqual(self.upstream_requests, [])
        self.assertEqual(self.errors[0]['type'], 'JSONDecodeError')

    def test_target_read_permission_and_io_errors_are_not_unarmed(self):
        original_read = Path.read_text
        self.arm.write_text('{"observe_timeout_checkpoint":true}')
        for code in [errno.EACCES, errno.EIO]:
            with self.subTest(errno=code):
                def read(path, *args, **kwargs):
                    if path == self.arm:
                        raise OSError(code, 'private fixture read failure', str(path))
                    return original_read(path, *args, **kwargs)

                with patch.object(Path, 'read_text', read):
                    status, _body, error = self.request('/containers/captured-id/json')
                self.assertIsNone(status)
                self.assertEqual(error, 'RemoteDisconnected')
                self.assertEqual(self.upstream_requests, [])
                self.assertEqual(self.errors[-1]['errno'], code)
                self.assertEqual(self.errors[-1]['filename'], str(self.arm))


if __name__ == '__main__':
    unittest.main()
