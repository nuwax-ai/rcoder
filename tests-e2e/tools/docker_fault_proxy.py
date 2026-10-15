"""Run-owned Docker Unix proxy for precise crash windows; no AI substitution.

Run inside the isolated Compose project. Writes are limited to containers whose
real labels match the one reserved application, and project-prefixed networks.
Only the selected first creation/start is gated; all allowed APIs reach Docker.
"""
import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import socketserver
import threading
import time
import urllib.parse

ROOT = Path(os.environ.get('FAULT_CONTROL_DIR', '/control'))
APP = os.environ.get('FAULT_APP_ID', '')
OWNER = os.environ.get('FAULT_USER_ID', '')
PROJECT = os.environ.get('FAULT_PROJECT', '')
MODE = os.environ.get('FAULT_MODE', '')
UPSTREAM = os.environ.get('FAULT_DOCKER_SOCKET', '/var/run/docker.sock')
PROXY = os.environ.get('FAULT_PROXY_SOCKET', '/proxy/docker.sock')
GATE_LOCK = threading.Lock()
GATED = False
OWNED_CONTAINERS = {}


class DockerConnection(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(120)
        self.sock.connect(UPSTREAM)


def exchange(method, path, body=None, headers=None):
    connection = DockerConnection('localhost', timeout=120)
    connection.request(method, path, body=body, headers=headers or {})
    response = connection.getresponse()
    return connection, response


def container_owned(identifier):
    connection, response = exchange('GET', '/containers/' + urllib.parse.quote(identifier, safe='') + '/json')
    try:
        if response.status != 200:
            return False
        info = json.loads(response.read())
        labels = info['Config'].get('Labels') or {}
        with GATE_LOCK:
            lifecycle = OWNED_CONTAINERS.get(info['Id'])
        # owner-id 标签已随用户绑定移除退役；物理身份锚定 application-id +
        # lifecycle-id + service-type（与 owned_builder_rows 同一模型）
        return (labels.get('rcoder.io/application-id') == APP
                and labels.get('service-type') == 'user-app-builder' and lifecycle is not None
                and labels.get('rcoder.io/lifecycle-id') == lifecycle)
    finally:
        connection.close()


def allowed(method, path, body):
    segments = urllib.parse.urlsplit(path).path.strip('/').split('/')
    if segments and segments[0].startswith('v1.'):
        segments = segments[1:]
    if method in ('GET', 'HEAD'):
        return True
    if segments == ['containers', 'create']:
        labels = json.loads(body).get('Labels') or {}
        return (urllib.parse.parse_qs(urllib.parse.urlsplit(path).query).get('name') == ['rcoder-app-builder-' + APP]
                and bool(labels.get('rcoder.io/lifecycle-id')) and labels.get('rcoder.io/application-id') == APP
                and labels.get('service-type') == 'user-app-builder')
    if len(segments) >= 2 and segments[0] == 'containers':
        return container_owned(segments[1])
    if segments == ['networks', 'create']:
        return json.loads(body).get('Name', '').startswith(PROJECT + '_')
    # Exec mutation is never needed to acknowledge a newly started builder;
    # rejecting an unexpected mutator makes fixture assumptions explicit.
    return False


def gate(stage, path, status=None):
    global GATED
    with GATE_LOCK:
        if GATED or MODE != stage:
            return
        GATED = True
    event = {'stage': stage, 'app_id': APP, 'path': path, 'remote_status': status}
    temporary = ROOT / 'barrier.tmp'
    temporary.write_text(json.dumps(event))
    temporary.replace(ROOT / 'barrier.json')
    deadline = time.monotonic() + 180
    while not (ROOT / 'release').exists():
        if time.monotonic() >= deadline:
            raise TimeoutError('fault controller did not settle the barrier')
        time.sleep(0.02)
    (ROOT / 'barrier-settled').write_text(stage)
    if (ROOT / 'release').read_text() != 'continue':
        raise ConnectionAbortedError('fault controller discarded this request')


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def setup(self):
        super().setup()
        self.connection.settimeout(120)

    def log_message(self, *_args):
        pass  # never log request bodies, environment or auth material

    def handle_request(self):
        connection = None
        try:
            if self.headers.get('Transfer-Encoding'):
                self.send_error(411, 'Fixture requires explicit request length')
                return
            length = int(self.headers.get('Content-Length', '0'))
            body = self.rfile.read(length) if length else b''
            if not allowed(self.command, self.path, body):
                self.send_error(403, 'Mutation outside the owned fault scenario')
                return
            path = urllib.parse.urlsplit(self.path).path
            if self.command == 'POST' and path.endswith('/containers/create'):
                with GATE_LOCK:
                    with (ROOT / 'create-attempts.jsonl').open('a') as audit:
                        audit.write(json.dumps({'app_id': APP, 'path': self.path}) + '\n')
                gate('before_create', self.path)
            headers = {key: value for key, value in self.headers.items()
                       if key.lower() not in ('connection', 'host', 'transfer-encoding')}
            connection, response = exchange(self.command, self.path, body, headers)
            payload = None
            if self.command == 'POST' and path.endswith('/containers/create') and response.status == 201:
                payload = response.read()
                created_id = json.loads(payload)['Id']
                lifecycle = json.loads(body)['Labels']['rcoder.io/lifecycle-id']
                with GATE_LOCK:
                    OWNED_CONTAINERS[created_id] = lifecycle
            if self.command == 'POST' and '/containers/' in path and path.endswith('/start') and 200 <= response.status < 300:
                gate('after_start', self.path, response.status)
            self.send_response(response.status)
            for key, value in response.getheaders():
                if key.lower() not in ('connection', 'transfer-encoding', 'content-length'):
                    self.send_header(key, value)
            self.send_header('Connection', 'close')
            self.end_headers()
            if payload is not None:
                self.wfile.write(payload)
                self.wfile.flush()
            else:
                deadline = time.monotonic() + 180
                while chunk := response.read(65536):
                    if time.monotonic() >= deadline:
                        raise TimeoutError('fault proxy response exceeded total budget')
                    self.wfile.write(chunk)
                    self.wfile.flush()
            self.close_connection = True
        except (OSError, ValueError, KeyError, http.client.HTTPException):
            self.close_connection = True
        finally:
            if connection is not None:
                connection.close()

    do_GET = do_HEAD = do_POST = do_PUT = do_PATCH = do_DELETE = handle_request


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


if __name__ == '__main__':
    if not APP or not OWNER or not PROJECT or MODE not in ('before_create', 'after_start'):
        raise SystemExit('Complete owned fault scenario configuration is required')
    ROOT.mkdir(parents=True, exist_ok=True)
    Path(PROXY).parent.mkdir(parents=True, exist_ok=True)
    with Server(PROXY, Handler) as server:
        os.chmod(PROXY, 0o666)
        server.serve_forever()
