#!/usr/bin/env python3
"""Native static-template owner smoke; no containers, PG or global installation.

Requires a previously built frontend artifact tar and matching Pingap binary.
This does not replace source-build/dev-server or file-server-proxy acceptance.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tarfile
import time
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--pingap', type=Path, required=True)
    parser.add_argument('--artifact', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    args = parser.parse_args()
    case = args.root.resolve() / ('nativeowner' + uuid.uuid4().hex[:12])
    workspace = case / 'workspace'
    workspace.mkdir(parents=True)
    with tarfile.open(args.artifact) as archive:
        archive.extractall(workspace, filter='data')
    # This case exercises a static service, not pnpm/Vite development. Preserve
    # the real built template, routes and assets, but omit its dev-only commands.
    lock = workspace / 'release.lock.toml'
    lines, omit = [], False
    for line in lock.read_text().splitlines(keepends=True):
        if line.lstrip().startswith('['):
            omit = line.strip() in {'[services.devbuild]', '[services.devrun]'}
        if not omit:
            lines.append(line)
    lock.write_text(''.join(lines))
    for port in (3018, 9080):
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', port))
    environment = {k: v for k, v in os.environ.items()
                   if not k.startswith(('APP_DEPLOY_', 'APP_CLI_'))}
    environment.update(PROJECT_ID=case.name, APP_CLI_STATE_ROOT=str(case / 'state'))
    command = [str(args.binary.resolve()), 'serve', '--workspace', str(workspace),
               '--log-dir', str(case / 'logs'), '--admin-addr', '127.0.0.1:0',
               '--pingap-bin', str(args.pingap.resolve())]
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    result = {'case': case.name,
              'binary_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest(),
              'artifact_sha256': hashlib.sha256(args.artifact.read_bytes()).hexdigest(),
              'passed': False}
    token, base, identity = None, None, None

    def request(path, body=None):
        headers = {'Content-Type': 'application/json'}
        if token:
            headers['X-Deploy-Token'] = token
        req = urllib.request.Request(base + path, headers=headers,
                                     data=None if body is None else json.dumps(body).encode())
        with opener.open(req, timeout=3) as response:
            return json.load(response)['data']

    def html(marker=None):
        with opener.open('http://127.0.0.1:9080/vue/', timeout=3) as response:
            body = response.read().decode()
            return response.status == 200 and '<html' in body and (marker is None or marker in body)

    def stop():
        status = request('/v1/runtime/status')
        op = 'stop' + uuid.uuid4().hex
        request('/v1/runtime/operations', {
            'operation_id': op, 'expected_runtime_instance_id': identity['runtime_instance_id'],
            'expected_revision': status['revision'], 'workspace_id': identity['workspace_id'],
            'kind': 'stop', 'profile': {'profile': 'source', 'input': {
                'workspace_id': identity['workspace_id']}}})
        deadline = time.monotonic() + 75
        while time.monotonic() < deadline:
            view = request('/v1/runtime/operations/' + op)
            if view['operation_id'] != op or view['runtime_instance_id'] != identity['runtime_instance_id']:
                raise RuntimeError('stop identity mismatch')
            if view['state'] == 'succeeded':
                return
            if view['state'] not in ('accepted', 'running'):
                raise RuntimeError('stop failed: ' + view['state'])
            time.sleep(.2)
        raise RuntimeError('stop deadline exceeded')

    with (case / 'owner.log').open('w') as log:
        owner = subprocess.Popen(command, env=environment, stdout=log, stderr=log)
        stopped = False
        try:
            deadline = time.monotonic() + 75
            while time.monotonic() < deadline:
                if owner.poll() is not None:
                    raise RuntimeError('owner exited during startup')
                try:
                    endpoint = json.loads((case / 'state/endpoint.json').read_text())
                    base = 'http://' + endpoint['address']
                    token = (case / 'state/token').read_text().strip()
                    identity = request('/v1/runtime/identity')
                    if endpoint['runtime_instance_id'] != identity['runtime_instance_id']:
                        raise RuntimeError('endpoint identity mismatch')
                    if token and html():
                        break
                except (OSError, ValueError):
                    pass
                time.sleep(.2)
            else:
                raise RuntimeError('frontend startup deadline exceeded')
            result['startup_html'] = True
            result['admin_address'] = endpoint['address']
            result['runtime_instance_id'] = identity['runtime_instance_id']
            index = workspace / 'frontend-vue3-vite/dist/index.html'
            index.write_text(index.read_text() + '\n<!-- native-owner-second-start -->\n')
            with (case / 'repeat.log').open('w') as output:
                repeat = subprocess.run(command, env=environment, stdout=output, stderr=output, timeout=150)
            if repeat.returncode:
                raise RuntimeError('repeated serve exited ' + str(repeat.returncode))
            after = request('/v1/runtime/identity')
            if owner.poll() is not None or after['runtime_instance_id'] != identity['runtime_instance_id']:
                raise RuntimeError('repeated serve replaced the owner')
            if not html('native-owner-second-start'):
                raise RuntimeError('repeated serve did not expose updated frontend')
            result['repeat_uses_original_owner'] = True
            result['updated_html'] = True
            stop()
            stopped = True
            if owner.poll() is not None or request('/v1/runtime/status')['desired'] != 'stopped':
                raise RuntimeError('Stop must retain an idle owner with desired=stopped')
            result['stop_succeeded_owner_alive'] = True
        except Exception as error:
            result['error'] = str(error)
        finally:
            if not stopped and identity and token and owner.poll() is None:
                try:
                    stop()
                except Exception as error:
                    result['cleanup_stop_error'] = str(error)
            if owner.poll() is None:
                owner.terminate()
            try:
                result['owner_exit'] = owner.wait(timeout=45)
            except subprocess.TimeoutExpired:
                owner.kill()
                owner.wait(timeout=5)
                result['cleanup_error'] = 'owner required forced termination'
    ports = [3018, 9080]
    if base:
        ports.append(int(base.rsplit(':', 1)[1]))
    result['closed_ports'] = []
    for port in ports:
        with socket.socket() as probe:
            probe.settimeout(1)
            if probe.connect_ex(('127.0.0.1', port)) != 0:
                result['closed_ports'].append(port)
    result['passed'] = (result.get('updated_html', False)
                        and result.get('stop_succeeded_owner_alive', False)
                        and len(result['closed_ports']) == len(ports)
                        and not any(key.endswith('error') for key in result))
    (case / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
