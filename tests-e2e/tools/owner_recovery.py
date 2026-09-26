#!/usr/bin/env python3
"""Real Docker component E2E for file-server + app-cli owner recovery.

Uses current Linux binaries, real supervisord/Pingap and an HTTP application.
No LLM or RCoder control-plane mock. Leaves the test workspace volume intact.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default='dev-rcoder-agent-runner:latest')
    parser.add_argument('--app-cli', required=True, type=Path)
    parser.add_argument('--file-server-proxy', required=True, type=Path)
    parser.add_argument('--report', required=True, type=Path)
    args = parser.parse_args()
    for binary in (args.app_cli, args.file_server_proxy):
        if not binary.is_file():
            parser.error(f'missing binary: {binary}')
    app = 'recover' + uuid.uuid4().hex[:12]
    name = 'rcoder-owner-recovery-' + app
    volume = name + '-workspace'
    workspace = '/home/user/' + app
    report = {'app_id': app, 'volume': volume, 'checks': [], 'containers': [],
              'binaries': {str(p.resolve()): hashlib.sha256(p.read_bytes()).hexdigest()
                           for p in (args.app_cli, args.file_server_proxy)}}
    cid = None

    def docker(*argv, check=True, timeout=180):
        return subprocess.run(['docker', *argv], capture_output=True, text=True,
                              check=check, timeout=timeout)

    def execute(command, *argv, check=True):
        return docker('exec', cid, 'sh', '-ec', command, '--', *argv, check=check)

    def check(label, passed, detail=None):
        report['checks'].append({'name': label, 'ok': bool(passed), 'detail': detail})
        print(label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise RuntimeError(label)

    def write(files):
        code = ('import json,pathlib,sys; '
                '[(pathlib.Path(p).parent.mkdir(parents=True,exist_ok=True),'
                'pathlib.Path(p).write_text(t)) for p,t in json.loads(sys.argv[1]).items()]')
        docker('exec', cid, 'python3', '-c', code, json.dumps(files))

    def get(path, port=60000):
        body = json.loads(execute('curl -fsS --max-time 10 "$1"', f'http://127.0.0.1:{port}{path}').stdout)
        if not body.get('success'):
            raise RuntimeError(f'GET {path}: {body}')
        return body

    def post(action):
        data = json.dumps({'app_id': app})
        body = json.loads(execute('curl -fsS --max-time 150 -H "content-type: application/json" '
                                 '--data "$1" "$2"', data,
                                 'http://127.0.0.1:60000/api/v1/userapp/dev/' + action).stdout)
        if not body.get('success'):
            raise RuntimeError(f'{action}: {body}')
        return body['data']

    def ready():
        return execute('curl -fsS --max-time 2 http://127.0.0.1:9080/health', check=False).returncode == 0

    def start(action='start'):
        task = post(action)['task_id']
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            body = get('/api/v1/userapp/tasks/' + task + '?app_id=' + app)
            data = body.get('data') or {}
            status = data.get('status')
            if status == 'completed':
                check(action + ' task and actual HTTP ready', ready(), data)
                return
            if status in ('failed', 'cancelled'):
                raise RuntimeError(f'{action} task: {data}')
            time.sleep(0.4)
        raise RuntimeError(f'{action} task timeout: {body}')

    def identity():
        return get('/v1/runtime/identity', 3010)['data']['runtime_instance_id']

    def wait_control():
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                return identity()
            except (subprocess.CalledProcessError, ValueError, KeyError):
                time.sleep(0.2)
        raise RuntimeError('management API did not initialize')

    def new_container():
        nonlocal cid
        image_id = docker('image', 'inspect', '--format', '{{.Id}}', args.image).stdout.strip()
        cid = docker('run', '-d', '--name', name, '--mount', f'type=volume,src={volume},dst=/home/user,volume-nocopy',
                     '-e', f'PROJECT_ID={app}', '-e', f'USERAPP_SINGLE_APP_ID={app}',
                     '-e', f'USERAPP_WORKSPACE_DIR={workspace}', '-e', 'LOG_BASE_DIR=/home/user/logs',
                     '-e', f'APP_CLI_STATE_ROOT=/home/user/.app-cli-state/{app}',
                     '-e', f'RCODER_RUNTIME_IMAGE_DIGEST={image_id}',
                     '-e', 'FILE_SERVER_LOG_DIR=/home/user/proxy-logs',
                     '-e', 'FILE_SERVER_APP_CLI_BIN=/usr/local/bin/app-cli',
                     '--entrypoint', 'sleep', image_id, 'infinity').stdout.strip()
        report['containers'].append({'id': cid, 'image': docker('inspect', '--format', '{{.Image}}', cid).stdout.strip()})
        docker('cp', str(args.app_cli.resolve()), f'{cid}:/usr/local/bin/app-cli')
        docker('cp', str(args.file_server_proxy.resolve()), f'{cid}:/usr/local/bin/file-server-proxy')
        write({'/tmp/owner-recovery-supervisor.conf': '''[unix_http_server]
file=/var/run/supervisor.sock
[supervisord]
nodaemon=true
logfile=/tmp/owner-recovery-supervisor.log
pidfile=/tmp/owner-recovery-supervisor.pid
[rpcinterface:supervisor]
supervisor.rpcinterface_factory=supervisor.rpcinterface:make_main_rpcinterface
[include]
files=/etc/supervisor/conf.d/50-app-services.conf
'''})
        docker('exec', '-d', cid, 'supervisord', '-n', '-c', '/tmp/owner-recovery-supervisor.conf')
        execute('mkdir -p "$1" /home/user/logs', workspace)
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec file-server-proxy --embed --policy all_rust --port 60000 > /tmp/proxy.log 2>&1')
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if execute('test -S /var/run/supervisor.sock && curl -fsS --max-time 2 '
                       'http://127.0.0.1:60000/health', check=False).returncode == 0:
                return
            time.sleep(0.2)
        raise RuntimeError('file-server or supervisord did not initialize')

    try:
        docker('volume', 'create', volume)
        new_container()
        manifest = '''schema_version = 1
[project]
service_id = "web"
name = "Recovery fixture"
type = "python"
[build]
command = ["python3", "-m", "zipfile", "-c", "artifact.zip", "main.py"]
artifact = "artifact.zip"
[run]
command = ["python3", "main.py"]
[health]
readiness_path = "/health"
[proxy]
path = "/"
strip_prefix = false
'''
        devrun = '\n[devrun]\ncommand = ["python3", "main.py"]\n'
        write({workspace + '/workspace.manifest.toml': 'schema_version=1\n[workspace]\nname="recovery"\n',
               workspace + '/web/project.manifest.toml': manifest + devrun,
               workspace + '/web/main.py': 'import os\nfrom http.server import BaseHTTPRequestHandler,HTTPServer\n'
               'class H(BaseHTTPRequestHandler):\n def do_GET(self):\n  self.send_response(200)\n  self.end_headers()\n'
               '  self.wfile.write(b"owner-recovery-ok")\nHTTPServer(("0.0.0.0",int(os.environ["PORT"])),H).serve_forever()\n',
               workspace + '/sentinel': app})
        # Independent manual owner, then file-server creates its durable registration.
        docker('exec', '-d', cid, 'sh', '-ec', 'echo $$ > /tmp/original-owner.pid; '
               'exec app-cli serve --control-only --workspace "$1" > /tmp/manual-owner.log 2>&1', '--', workspace)
        original = wait_control()
        start()
        execute('kill -KILL "$(cat /tmp/original-owner.pid)"')
        check('owner death leaves supervised business running', ready())
        result = post('stop')
        replacement = identity()
        check('stop recovers management and stops old children',
              result.get('message') == 'Stopped' and not ready() and replacement != original, result)
        check('repeat stop is idempotent', post('stop').get('message') == 'Stopped' and not ready())
        start()
        check('start reuses recovered owner', identity() == replacement)
        # Same retained volume, genuinely new PID namespace/container.
        docker('rm', '-f', cid)
        cid = None
        new_container()
        start()
        check('container replacement recovers stale registration', identity() != replacement)
        check('original workspace data retained', execute('cat "$1/sentinel"', workspace).stdout == app)
        # No devrun: use actual build ZIP -> owner Deploy(ArtifactId) -> .run.
        write({workspace + '/web/project.manifest.toml': manifest})
        start('restart')
        check('artifact restart activates .run', execute('test -f "$1/.run/web/main.py"', workspace, check=False).returncode == 0)
        post('stop')
        check('final stop leaves management alive and business stopped', bool(identity()) and not ready())
        state = json.loads(execute('cat /home/user/logs/dev-server-external.json').stdout)
        check('old registration evidence retained', len(state.get('retired', {})) >= 2 and bool(state.get('completed')))
        report['success'] = True
    except (Exception, KeyboardInterrupt) as error:
        report.update(success=False, error=str(error))
        if cid:
            report['logs'] = docker('exec', cid, 'python3', '-c',
                'from pathlib import Path; '
                'files=[Path("/tmp/proxy.log"),Path("/tmp/manual-owner.log")]+list(Path("/home/user/logs").rglob("*.log")); '
                '[print(str(p),p.read_text(errors="replace")[-8000:]) for p in files if p.is_file()]',
                check=False).stdout
    finally:
        if cid:
            cleanup = docker('rm', '-f', cid, check=False)
            report['cleanup_ok'] = cleanup.returncode == 0
            report['success'] = report.get('success', False) and report['cleanup_ok']
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report.get('success') else 1


if __name__ == '__main__':
    raise SystemExit(main())
