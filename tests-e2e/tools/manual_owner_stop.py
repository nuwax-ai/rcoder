#!/usr/bin/env python3
"""One real Compose reproduction: manual app-cli -> RCoder dev/stop.
Creates a unique disposable builder; preserves its workspace volume on cleanup.
No mocked owner, LLM, shared app, database reset or process-name killing.
"""
import argparse
import json
import subprocess
import time
import uuid
from pathlib import Path


def docker(*args, check=True):
    return subprocess.run(['docker', *args], text=True, capture_output=True,
                          check=check, timeout=180)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rcoder', default='http://127.0.0.1:8090')
    parser.add_argument('--report', required=True)
    args = parser.parse_args()
    app = 'manual' + uuid.uuid4().hex[:14]
    name = 'rcoder-app-builder-' + app
    records = {'app_id': app, 'checks': [], 'container_id': None}
    cid = None

    def check(label, passed, detail=None):
        records['checks'].append({'name': label, 'ok': bool(passed), 'detail': detail})
        print(label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise RuntimeError(label)

    def post(path):
        # Every request is issued by a fresh OS process, not a reused client.
        result = subprocess.run(
            ['curl', '--silent', '--show-error', '--fail-with-body',
             '--max-time', '180', '-H', 'Content-Type: application/json',
             '-H', 'X-App-Id: ' + app, '--data', json.dumps({'app_id': app}),
             args.rcoder.rstrip('/') + path],
            text=True, capture_output=True, timeout=185, check=True)
        return json.loads(result.stdout)

    def execute(command, *params, check=True):
        return docker('exec', cid, 'sh', '-ec', command, '--', *params, check=check)

    try:
        check('fresh resource name', docker('inspect', name, check=False).returncode != 0)
        body = post('/api/v1/userapp/workspace')
        check('RCoder creates builder', body.get('code') == '0000', body.get('code'))
        cid = docker('inspect', '--format', '{{.Id}}', name).stdout.strip()
        records['container_id'] = cid
        records['image_id'] = docker('inspect', '--format', '{{.Image}}', cid).stdout.strip()
        ws = '/home/user/' + app
        # This container requires PostgreSQL before app-cli orchestration.
        # Complete real cold initialization before testing ownership transfer;
        # a missing database must fail setup, not masquerade as a Stop failure.
        pg_deadline = time.monotonic() + 180
        while True:
            pg = execute('PGPASSWORD="$POSTGRES_PASSWORD" PGCONNECT_TIMEOUT=2 '
                         'psql -X -w -h 127.0.0.1 -U "$POSTGRES_USER" '
                         '-d "$POSTGRES_DB" -qAt -c "SELECT 1"', check=False)
            if pg.returncode == 0 and pg.stdout.strip() == '1':
                break
            if time.monotonic() >= pg_deadline:
                raise RuntimeError('fixture PostgreSQL initialization/login did not complete')
            time.sleep(2)
        check('real PostgreSQL prerequisite ready', True)
        files = {
            'workspace.manifest.toml': 'schema_version = 1\n[workspace]\nname = "manual-stop"\n',
            'backend/project.manifest.toml': '''schema_version = 1
[project]
service_id = "backend-go"
name = "Manual fixture"
type = "go"
kind = "web"
enabled = true
[build]
command = ["sh", "-c", "zip -q artifact.zip start.sh"]
artifact = "artifact.zip"
[run]
command = ["sh", "-c", "touch ready && exec python3 -m http.server $PORT --bind 0.0.0.0"]
[health]
readiness_path = "/ready"
[proxy]
path = "/api/go/"
strip_prefix = true
''',
            'backend/start.sh': '#!/bin/sh\nexit 0\n',
        }
        writer = ('import json,pathlib,sys; root=pathlib.Path(sys.argv[1]); '
                  'files=json.loads(sys.argv[2]); '
                  '[( (root/p).parent.mkdir(parents=True,exist_ok=True), '
                  '(root/p).write_text(text)) for p,text in files.items()]')
        docker('exec', cid, 'python3', '-c', writer, ws, json.dumps(files))
        execute('app-cli gen-lock --workspace "$1" && app-cli build --workspace "$1" --deploy-dir "$1/.local-deploy"', ws)
        # Reproduce old agent-created directories, which have no origin metadata.
        execute('rm -f "$1/.local-deploy/.app-cli-project-origin.json"; '
                'setsid app-cli serve --workspace "$1/.local-deploy" '
                '> /tmp/manual-owner.log 2>&1 < /dev/null & echo $! > /tmp/manual-owner.pid', ws)
        def service_ready():
            deadline = time.monotonic() + 120
            while time.monotonic() < deadline:
                if execute('curl -fsS --max-time 2 http://127.0.0.1:9080/api/go/ready', check=False).returncode == 0:
                    return True
                time.sleep(2)
            return False

        def owner_identity():
            return json.loads(execute('curl -fsS --max-time 5 http://127.0.0.1:3010/v1/runtime/identity').stdout)['data']

        def supervisor_identity():
            return json.loads(execute('app-cli owner status --workspace "$1/.local-deploy"', ws).stdout)

        def single_owner(snapshot):
            # The approved architecture has a parent, worker and guardians.
            # Count active execution generations, not every app-cli OS process.
            probe = """
import json, pathlib, sys
expected = json.loads(sys.argv[1])
parent = pathlib.Path('/tmp/manual-owner.pid').read_text().strip()
roots = set()
for fd in pathlib.Path('/proc', parent, 'fd').iterdir():
    try:
        path = pathlib.Path(fd.readlink())
        if path.name == 'owner.lock' and (path.parent/'supervisor.json').is_file():
            roots.add(path.parent)
    except FileNotFoundError:
        pass
assert len(roots) == 1, 'original supervisor no longer owns its scope'
root = roots.pop()
discovery = json.loads((root/'supervisor.json').read_text())
assert discovery['instance'] == expected['supervisor_id']
active = []
for path in (root/'work').glob('*/generation.json'):
    value = json.loads(path.read_text())
    if value['phase'] in ('Running', 'Draining'):
        active.append(value)
assert len(active) == 1 and active[0]['id'] == expected['generation']
worker = pathlib.Path('/proc', str(active[0]['worker_pid']))
assert worker.exists(), 'active worker absent'
print(json.dumps({'supervisor_id': discovery['instance'], 'generation': active[0]['id'],
                  'worker_pid': active[0]['worker_pid']}))
"""
            result = docker('exec', cid, 'python3', '-c', probe, json.dumps(snapshot), check=False)
            return result.returncode == 0, result.stdout if result.returncode == 0 else result.stderr

        check('manual service serves actual HTTP', service_ready())
        identity = owner_identity()
        records['owner_before'] = identity
        supervisor_before = supervisor_identity()
        records['supervisor_before'] = supervisor_before
        for cycle in range(1, 3):
            body = post('/api/v1/userapp/dev/stop')
            check(f'cycle {cycle}: RCoder stops owner services', (body.get('data') or {}).get('message') == 'Stopped', body)
            check(f'cycle {cycle}: service stopped', execute('curl -fsS --max-time 2 http://127.0.0.1:9080/api/go/ready', check=False).returncode != 0)
            body = post('/api/v1/userapp/dev/stop')
            check(f'cycle {cycle}: independent caller repeats stop', (body.get('data') or {}).get('message') == 'Stopped', body)
            # A new app-cli process forwards Start to the existing owner and exits.
            result = execute('app-cli run --workspace "$1/.local-deploy"', ws, check=False)
            check(f'cycle {cycle}: new CLI client starts services', result.returncode == 0, result.stderr[-2000:])
            check(f'cycle {cycle}: actual HTTP restored', service_ready())
            check(f'cycle {cycle}: same owner retained', identity['runtime_instance_id'] == owner_identity()['runtime_instance_id'])
            snapshot = supervisor_identity()
            sole_owner, details = single_owner(snapshot)
            check(f'cycle {cycle}: single owner execution retained', sole_owner
                  and snapshot['phase'] == 'ready'
                  and snapshot['supervisor_id'] == supervisor_before['supervisor_id']
                  and snapshot['generation'] == supervisor_before['generation'], details)
        body = post('/api/v1/userapp/dev/stop')
        check('final services stopped', (body.get('data') or {}).get('message') == 'Stopped'
              and execute('curl -fsS --max-time 2 http://127.0.0.1:9080/api/go/ready', check=False).returncode != 0)
        check('same owner remains alive', identity['runtime_instance_id'] == owner_identity()['runtime_instance_id'])
        check('same container retained', docker('inspect', '--format', '{{.Id}}', name).stdout.strip() == cid)
        check('workspace retained', execute('test -f "$1/backend/project.manifest.toml"', ws, check=False).returncode == 0)
        records['success'] = True
    except Exception as error:
        records['success'] = False
        records['error'] = str(error)
        if cid:
            records['owner_log_tail'] = execute('tail -n 50 /tmp/manual-owner.log', check=False).stdout
            records['pg_log_tail'] = execute(
                'tail -n 50 /app/logs/pg.out.log /app/logs/pg.err.log', check=False).stdout
    finally:
        if cid:
            cleanup = docker('rm', '-f', cid, check=False)
            records['cleanup_ok'] = cleanup.returncode == 0
            records['success'] = records.get('success', False) and records['cleanup_ok']
        Path(args.report).parent.mkdir(parents=True, exist_ok=True)
        Path(args.report).write_text(json.dumps(records, ensure_ascii=False, indent=2))
    return 0 if records.get('success') else 1


if __name__ == '__main__':
    raise SystemExit(main())
