"""Run-owned Docker Unix proxy for precise crash windows; no AI substitution.

Run inside the isolated Compose project. Writes are limited to containers whose
real labels match the one reserved application, and project-prefixed networks.
Creation/start crash gates and the exact completed-Stop reply gate leave all
allowed mutations reaching real Docker; no successful response is invented.
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
import urllib.request
import uuid

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
OWNED_EXECS = {}
OWNED_ALIASES = {}
STOPPED_CONTAINERS = set()
COMPUTE_MODES = frozenset({'compute_stopped', 'compute_stop_timeout'})
STOP_REPLY_HOLD_SECONDS = 180


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


def container_owned(identifier, *, allow_known_absent=False):
    connection, response = exchange('GET', '/containers/' + urllib.parse.quote(identifier, safe='') + '/json')
    try:
        if response.status == 404 and allow_known_absent:
            with GATE_LOCK:
                known = len(identifier) == 64 and identifier in OWNED_CONTAINERS
                if known:
                    OWNED_ALIASES[identifier] = identifier
            return known
        if response.status != 200:
            return False
        info = json.loads(response.read())
        labels = info['Config'].get('Labels') or {}
        with GATE_LOCK:
            lifecycle = OWNED_CONTAINERS.get(info['Id'])
        # owner-id 标签已随用户绑定移除退役；物理身份锚定 application-id +
        # lifecycle-id + service-type（与 owned_builder_rows 同一模型）
        owned = (labels.get('rcoder.io/application-id') == APP
                and labels.get('service-type') == 'user-app-builder' and lifecycle is not None
                and labels.get('rcoder.io/lifecycle-id') == lifecycle)
        if owned:
            with GATE_LOCK:
                OWNED_ALIASES[identifier] = info['Id']
        return owned
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
        if (MODE in COMPUTE_MODES and segments[-1] == 'exec'
                and json.loads(body).get('AttachStdin') is True):
            return False  # this fixture relays only noninteractive exec output
        if MODE == 'compute_stop_timeout' and method == 'DELETE':
            if (len(segments) != 2 or len(segments[1]) != 64
                    or urllib.parse.parse_qs(urllib.parse.urlsplit(path).query).get('v') != ['false']):
                return False
            return container_owned(segments[1], allow_known_absent=True)
        return container_owned(segments[1])
    if MODE in COMPUTE_MODES and len(segments) == 3 and segments[0] == 'exec':
        with GATE_LOCK:
            container = OWNED_EXECS.get(segments[1])
        return container is not None and container_owned(container)
    if segments == ['networks', 'create']:
        return json.loads(body).get('Name', '').startswith(PROJECT + '_')
    # Exec mutation is never needed to acknowledge a newly started builder;
    # rejecting an unexpected mutator makes fixture assumptions explicit.
    return False


def compute_fault(method, path):
    """Fail a read only after the actual control plane committed Stopped.

    The stop acknowledgement alone is insufficient: Docker's internal stop
    confirmation must continue normally while the durable phase is Stopping.
    No fixture edits the database or fabricates a successful runtime response.
    """
    arm_path = ROOT / 'compute-arm.json'
    if MODE != 'compute_stopped' or method != 'GET' or not arm_path.exists():
        return None
    try:
        arm = json.loads(arm_path.read_text())
    except FileNotFoundError:
        return None  # the test revoked the read fault between exists/read
    identifier = urllib.parse.urlsplit(path).path.strip('/').split('/')
    if identifier and identifier[0].startswith('v1.'):
        identifier = identifier[1:]
    cid, life = arm['container_id'], arm['lifecycle_id']
    if identifier not in (['containers', cid, 'json'],
                          ['containers', 'rcoder-app-builder-' + APP, 'json']):
        return None
    with GATE_LOCK:
        ready = OWNED_CONTAINERS.get(cid) == life and cid in STOPPED_CONTAINERS
    if not ready:
        return None
    deadline = time.monotonic() + 15
    operation_path = ROOT / 'compute-operation.json'
    while True:
        if not arm_path.exists():
            return None
        if time.monotonic() >= deadline:
            raise TimeoutError('compute fault operation was not recorded')
        try:
            operation_identity = json.loads(operation_path.read_text())
        except FileNotFoundError:
            time.sleep(0.02)
            continue
        if not isinstance(operation_identity, dict):
            raise ValueError('compute fault operation identity must be an object')
        operation_id = operation_identity['operation_id']
        if not isinstance(operation_id, str) or not operation_id:
            raise ValueError('compute fault operation identity is invalid')
        break
    url = 'http://rcoder:8090/computer/pod/operations/' + urllib.parse.quote(APP, safe='') + '/' + urllib.parse.quote(operation_id, safe='')
    with urllib.request.urlopen(url, timeout=5) as response:
        envelope = json.load(response)
    operation = envelope.get('data') or {}
    if (envelope.get('code') != '0000' or operation.get('app_id') != APP
            or operation.get('lifecycle_id') != life or operation.get('operation_id') != operation_id):
        raise ValueError('compute fault operation identity changed')
    if operation.get('stage') != 'stopped' or operation.get('state') not in ('running', 'recovery_required'):
        return None
    # Verify this is still the captured physical object, or its acknowledged
    # AutoRemove absence. A replacement is never attributed to the old target.
    connection, response = exchange('GET', '/containers/' + urllib.parse.quote(cid, safe='') + '/json')
    try:
        if response.status == 200:
            actual = json.loads(response.read())
            labels = actual['Config'].get('Labels') or {}
            if (actual['Id'] != cid or labels.get('rcoder.io/application-id') != APP
                    or labels.get('rcoder.io/lifecycle-id') != life
                    or labels.get('service-type') != 'user-app-builder'
                    or actual['State'].get('Running')):
                raise ValueError('compute fault target physical identity is not stopped')
        elif response.status != 404:
            raise ValueError('compute fault target absence is unconfirmed')
        marker = 'owned-compute-stopped-inspect-fault'
        event = {'operation': operation, 'container_id': cid, 'lifecycle_id': life,
                 'method': method, 'path': path, 'physical_status': response.status,
                 'marker': marker}
        with GATE_LOCK:
            with (ROOT / 'compute-faults.jsonl').open('a') as output:
                output.write(json.dumps(event) + '\n')
        return marker
    finally:
        connection.close()


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


def hold_compute_stop_reply(method, path, status):
    """Withhold only an already-completed real Stop response, never its write.

    The unchanged Docker client must reach its own timeout. The test then
    releases this response explicitly; no success/error response is invented.
    A distinct settled receipt also lets cleanup drain the held transport after
    stopping the original controller, before removing its owned builder.
    """
    if MODE != 'compute_stop_timeout' or method != 'POST':
        return
    segments = urllib.parse.urlsplit(path).path.strip('/').split('/')
    if segments and segments[0].startswith('v1.'):
        segments = segments[1:]
    arm_path = ROOT / 'compute-arm.json'
    if len(segments) != 3 or segments[0] != 'containers' or segments[2] != 'stop' or not arm_path.exists():
        return
    arm = json.loads(arm_path.read_text())
    cid, life = arm['container_id'], arm['lifecycle_id']
    if segments[1] != cid or status not in (204, 304):
        raise ValueError('Stop timeout must capture the exact successful real Stop')
    with GATE_LOCK:
        if OWNED_CONTAINERS.get(cid) != life or cid not in STOPPED_CONTAINERS:
            raise ValueError('Stop timeout physical ownership is unconfirmed')
    publication_deadline = time.monotonic() + 15
    operation_path = ROOT / 'compute-operation.json'
    while not operation_path.exists():
        if time.monotonic() >= publication_deadline:
            raise TimeoutError('Stop timeout operation identity was not published')
        time.sleep(.02)
    operation_id = json.loads(operation_path.read_text()).get('operation_id')
    if not isinstance(operation_id, str) or not operation_id:
        raise ValueError('Stop timeout operation identity is invalid')
    url = ('http://rcoder:8090/computer/pod/operations/' + urllib.parse.quote(APP, safe='')
           + '/' + urllib.parse.quote(operation_id, safe=''))
    with urllib.request.urlopen(url, timeout=5) as response:
        envelope = json.load(response)
    operation = envelope.get('data') or {}
    if (envelope.get('code') != '0000' or operation.get('app_id') != APP
            or operation.get('lifecycle_id') != life or operation.get('operation_id') != operation_id
            or operation.get('stage') != 'stopping' or operation.get('state') != 'running'):
        raise ValueError('Stop timeout does not belong to the original running Stopping phase')
    event = {'app_id': APP, 'container_id': cid, 'lifecycle_id': life,
             'operation': operation, 'method': method, 'path': path,
             'upstream_status': status, 'upstream_completed': True,
             'response_released': False, 'held_at_monotonic': time.monotonic()}
    with GATE_LOCK:
        # A second actual Stop must be visible as a failure, not silently gated.
        held = ROOT / 'compute-stop-held.json'
        if held.exists():
            raise ValueError('More than one Stop reached the held reply boundary')
        temporary = ROOT / 'compute-stop-held.tmp'
        with temporary.open('x') as output:
            json.dump(event, output)
        temporary.chmod(0o600)
        temporary.replace(held)
    release_path = ROOT / 'compute-stop-release.json'
    deadline = time.monotonic() + STOP_REPLY_HOLD_SECONDS
    try:
        while not release_path.exists():
            if time.monotonic() >= deadline:
                raise TimeoutError('Stop reply was not explicitly released within the fixture budget')
            time.sleep(.02)
        action = json.loads(release_path.read_text()).get('action')
        if action not in ('continue', 'discard'):
            raise ValueError('Stop reply release action is invalid')
        event.update(response_released=action == 'continue', release_action=action)
        if action == 'discard':
            raise ConnectionAbortedError('Stopped controller no longer consumes its Stop reply')
    finally:
        event['settled_at_monotonic'] = time.monotonic()
        settled = ROOT / 'compute-stop-settled.json'
        temporary = ROOT / 'compute-stop-settled.tmp'
        with temporary.open('x') as output:
            json.dump(event, output)
        temporary.chmod(0o600)
        temporary.replace(settled)


def audit_proxy_error(method, path, error):
    """Keep private transport context without exception text or request secrets."""
    if MODE not in COMPUTE_MODES:
        return
    event = {'method': method, 'path': urllib.parse.urlsplit(path).path,
             'error_type': type(error).__name__}
    if isinstance(getattr(error, 'errno', None), int):
        event['errno'] = error.errno
    with GATE_LOCK:
        descriptor = os.open(ROOT / 'proxy-errors.jsonl', os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        with os.fdopen(descriptor, 'a') as audit:
            audit.write(json.dumps(event) + '\n')


def compute_write_attempt(method, path):
    """Record each dispatched write, including refused writes and SDK retries."""
    if MODE not in COMPUTE_MODES or method in ('GET', 'HEAD'):
        return None
    parsed = urllib.parse.urlsplit(path)
    query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)
    event = {'request_id': uuid.uuid4().hex, 'method': method, 'path': parsed.path,
             'query': {key: query[key] for key in ('v', 'force', 'name', 't') if key in query}}
    with GATE_LOCK:
        descriptor = os.open(ROOT / 'compute-writes.jsonl', os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        with os.fdopen(descriptor, 'a') as audit:
            audit.write(json.dumps(event) + '\n')
    return event


def compute_write_result(attempt, outcome, status=None, body=b'', payload=None):
    """Join the actual upstream result without logging payloads or credentials."""
    if attempt is None:
        return
    event = {'request_id': attempt['request_id'], 'outcome': outcome, 'status': status}
    segments = attempt['path'].strip('/').split('/')
    if segments and segments[0].startswith('v1.'):
        segments = segments[1:]
    if outcome == 'upstream':
        with GATE_LOCK:
            if len(segments) >= 2 and segments[0] == 'containers' and segments[1] != 'create':
                event['container_id'] = OWNED_ALIASES.get(segments[1])
            elif len(segments) == 3 and segments[0] == 'exec':
                event['container_id'] = OWNED_EXECS.get(segments[1])
        if segments == ['containers', 'create']:
            labels = json.loads(body).get('Labels') or {}
            event['creation'] = {key: labels.get(key) for key in (
                'rcoder.io/application-id', 'rcoder.io/lifecycle-id', 'service-type')}
            if status == 201:
                event['created_container_id'] = json.loads(payload)['Id']
        if len(segments) == 3 and segments[0] == 'containers' and segments[2] == 'exec' and status == 201:
            event['created_exec_id'] = json.loads(payload)['Id']
    with GATE_LOCK:
        descriptor = os.open(ROOT / 'compute-write-results.jsonl', os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        with os.fdopen(descriptor, 'a') as audit:
            audit.write(json.dumps(event) + '\n')


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def setup(self):
        super().setup()
        self.connection.settimeout(120)

    def log_message(self, *_args):
        pass  # never log request bodies, environment or auth material

    def handle_request(self):
        connection = None
        attempt = None
        result_recorded = False
        try:
            attempt = compute_write_attempt(self.command, self.path)
            if self.headers.get('Transfer-Encoding'):
                compute_write_result(attempt, 'denied', 411)
                result_recorded = True
                self.send_error(411, 'Fixture requires explicit request length')
                return
            length = int(self.headers.get('Content-Length', '0'))
            body = self.rfile.read(length) if length else b''
            if not allowed(self.command, self.path, body):
                compute_write_result(attempt, 'denied', 403)
                result_recorded = True
                self.send_error(403, 'Mutation outside the owned fault scenario')
                return
            marker = compute_fault(self.command, self.path)
            if marker:
                payload = json.dumps({'message': marker}).encode()
                self.send_response(500)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(payload)))
                self.send_header('Connection', 'close')
                self.end_headers()
                self.wfile.write(payload)
                self.close_connection = True
                return
            path = urllib.parse.urlsplit(self.path).path
            if self.command == 'POST' and path.endswith('/containers/create'):
                with GATE_LOCK:
                    with (ROOT / 'create-attempts.jsonl').open('a') as audit:
                        audit.write(json.dumps({'app_id': APP, 'path': self.path}) + '\n')
                gate('before_create', self.path)
            headers = {key: value for key, value in self.headers.items()
                       if key.lower() not in ('connection', 'host', 'transfer-encoding')}
            owned_exec_start = (MODE in COMPUTE_MODES and self.command == 'POST'
                                and '/exec/' in path and path.endswith('/start'))
            if owned_exec_start and self.headers.get('Connection'):
                headers['Connection'] = self.headers['Connection']
            connection, response = exchange(self.command, self.path, body, headers)
            payload = None
            if self.command == 'POST' and path.endswith('/containers/create') and response.status == 201:
                payload = response.read()
                created_id = json.loads(payload)['Id']
                lifecycle = json.loads(body)['Labels']['rcoder.io/lifecycle-id']
                with GATE_LOCK:
                    OWNED_CONTAINERS[created_id] = lifecycle
                    OWNED_ALIASES[created_id] = created_id
            if MODE in COMPUTE_MODES and self.command == 'POST' and '/containers/' in path:
                if path.endswith('/exec') and response.status == 201:
                    payload = response.read()
                    with GATE_LOCK:
                        OWNED_EXECS[json.loads(payload)['Id']] = OWNED_ALIASES.get(path.split('/')[-2])
                if path.endswith('/stop') and response.status in (204, 304):
                    with GATE_LOCK:
                        STOPPED_CONTAINERS.add(path.split('/')[-2])
            # This is the real daemon result, before any held-response fault.
            compute_write_result(attempt, 'upstream', response.status, body, payload)
            result_recorded = True
            hold_compute_stop_reply(self.command, self.path, response.status)
            if self.command == 'POST' and '/containers/' in path and path.endswith('/start') and 200 <= response.status < 300:
                gate('after_start', self.path, response.status)
            # Docker exec start upgrades HTTP to a raw multiplexed stream.
            # HTTPResponse.read() treats all 1xx as bodyless and would end the
            # client stream before the real command exits. Only this fixture's
            # already-authorized, noninteractive exec receives the raw relay.
            upgraded_exec = owned_exec_start and response.status == 101
            if response.status == 101 and not upgraded_exec:
                raise ValueError('Unexpected Docker protocol upgrade')
            self.send_response(response.status)
            for key, value in response.getheaders():
                if key.lower() not in ('connection', 'transfer-encoding', 'content-length'):
                    self.send_header(key, value)
            self.send_header('Connection', 'Upgrade' if upgraded_exec else 'close')
            self.end_headers()
            if upgraded_exec:
                deadline = time.monotonic() + 180
                while chunk := response.fp.read1(65536):
                    if time.monotonic() >= deadline:
                        raise TimeoutError('owned exec stream exceeded total budget')
                    self.wfile.write(chunk)
                    self.wfile.flush()
            elif payload is not None:
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
        except (OSError, ValueError, KeyError, http.client.HTTPException) as error:
            try:
                if not result_recorded:
                    compute_write_result(attempt, 'transport_error')
                audit_proxy_error(self.command, self.path, error)
            except (OSError, ValueError):
                pass  # diagnostics cannot replace the original transport failure
            self.close_connection = True
        finally:
            if connection is not None:
                connection.close()

    do_GET = do_HEAD = do_POST = do_PUT = do_PATCH = do_DELETE = handle_request


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


if __name__ == '__main__':
    if not APP or not OWNER or not PROJECT or MODE not in ('before_create', 'after_start', *COMPUTE_MODES):
        raise SystemExit('Complete owned fault scenario configuration is required')
    ROOT.mkdir(parents=True, exist_ok=True)
    Path(PROXY).parent.mkdir(parents=True, exist_ok=True)
    with Server(PROXY, Handler) as server:
        os.chmod(PROXY, 0o666)
        server.serve_forever()
