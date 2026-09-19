#!/usr/bin/env python3
"""Native embedded proxy HTTP smoke; not the full NT01-NT16 acceptance matrix."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--root', required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = args.root.resolve() / ('proxy' + secrets.token_hex(6))
    root.mkdir(parents=True)
    workspace = root / 'workspace space'
    workspace.mkdir()
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        port = probe.getsockname()[1]
    token = secrets.token_hex(24)
    env = os.environ.copy()
    env.update({
        'FILE_SERVER_PROXY_HOST': '127.0.0.1',
        'FILE_SERVER_PROXY_TOKEN': token,
        'FILE_SERVER_PROXY_STATE_DIR': str(root / 'state'),
        'FILE_SERVER_LOG_DIR': str(root / 'logs'),
        'PROJECT_SOURCE_DIR': str(root / 'projects'),
        'COMPUTER_WORKSPACE_DIR': str(root / 'computer'),
        'USERAPP_WORKSPACE_DIR': str(root / 'userapp'),
    })
    for key in ('FILE_SERVER_CONFIG', 'USERAPP_SINGLE_APP_ID'):
        env.pop(key, None)
    command = [str(binary), '--embed', '--policy', 'all_rust', '--port', str(port)]
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(path, payload=None, authenticated=True):
        headers = {'X-Proxy-Token': token} if authenticated else {}
        data = None
        if payload is not None:
            data = json.dumps(payload).encode()
            headers['Content-Type'] = 'application/json'
        req = urllib.request.Request(f'http://127.0.0.1:{port}' + path, data, headers)
        try:
            with opener.open(req, timeout=5) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()

    result = {'case': root.name, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'passed': False, 'checks': []}
    with (root / 'proxy.log').open('wb') as log:
        process = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 30
            while True:
                if process.poll() is not None:
                    raise RuntimeError(f'proxy exited early: {process.returncode}')
                try:
                    status, body = request('/api/computer/fs/roots')
                    assert status == 200, (status, body)
                    assert json.loads(body)['success'] is True
                    break
                except (OSError, AssertionError):
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(0.1)
            result['checks'].append('embedded filesystem roots')
            assert request('/api/computer/fs/roots', authenticated=False)[0] == 401
            result['checks'].append('missing token rejected')
            scope = {'userId': 'native', 'cId': 'test', 'workspacePath': str(workspace)}
            status, body = request('/api/computer/files-update', {**scope, 'files': [
                {'operation': 'create', 'name': 'hello.txt', 'contents': 'native proxy data'}]})
            assert status == 200 and json.loads(body)['success'], (status, body)
            assert (workspace / 'hello.txt').read_text() == 'native proxy data'
            status, body = request('/api/computer/get-file-list?' + urllib.parse.urlencode(scope))
            assert status == 200 and b'hello.txt' in body, (status, body)
            result['checks'].append('HTTP write and file listing in space path')
            with (root / 'duplicate.log').open('wb') as duplicate_log:
                duplicate = subprocess.run(command, env=env, stdout=duplicate_log,
                                           stderr=subprocess.STDOUT, timeout=15)
            assert duplicate.returncode != 0
            assert process.poll() is None and request('/api/computer/fs/roots')[0] == 200
            result['checks'].append('duplicate fixed listener fails without harming original')
            result['passed'] = True
        except Exception as error:
            result['error'] = str(error)
        finally:
            process.terminate()
            try:
                result['exit_code'] = process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill()
                result['exit_code'] = process.wait()
                result['passed'] = False
                result['error'] = 'proxy did not exit within cleanup budget'
            if os.name != 'nt' and result['exit_code'] != 0:
                result['passed'] = False
            with socket.socket() as probe:
                probe.settimeout(2)
                result['port_closed'] = probe.connect_ex(('127.0.0.1', port)) != 0
            result['passed'] &= result['port_closed']
    (root / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
