#!/usr/bin/env python3
"""隔离 Docker 的迁移失败验收：真实 app-cli、任务 GET/SSE、HTTP 与原操作。

不构建、不发布、不使用 AI，不修改已有容器；300 秒迁移超时必须真实等待。
只清理本轮创建的容器，保留独立工作区卷供取证。
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tempfile
import time
import uuid

from isolated_docker import require_local_docker_endpoint
from userapp_root_logs import source_snapshot


MIGRATION = r'''import json,os,signal,subprocess,sys,time
from pathlib import Path
mode=Path("scenario.txt").read_text().strip()
def process(pid):
    if sys.platform=="linux":
        fields=Path(f"/proc/{pid}/stat").read_text().rsplit(")",1)[1].split()
        return {"pid":pid,"start_time":fields[19],"pgid":int(fields[2])}
    if sys.platform=="darwin":
        fields=subprocess.check_output(["ps","-p",str(pid),"-o","lstart=","-o","pgid="],text=True).split()
        return {"pid":pid,"start_time":" ".join(fields[:-1]),"pgid":int(fields[-1])}
    raise RuntimeError("process fixture requires Linux or macOS")
members=[process(os.getpid())]
if mode in ("timeout","stop"):
    child=subprocess.Popen([sys.executable,"-c","import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(900)"])
    members.append(process(child.pid))
root=Path(os.environ["APP_CLI_STATE_ROOT"])
receipt=json.loads((root/".deploy-operation.json").read_text())
proof={"scenario":mode,"operation_id":receipt["operation"]["operation_id"],"started_monotonic":time.monotonic(),"members":members}
Path("migration-process.json").write_text(json.dumps(proof))
print("MIGRATION-STDOUT-BEGIN",flush=True)
print(os.environ["POSTGRES_PASSWORD"],flush=True)
print("MIGRATION-STDERR-BEGIN"+"x"*24576+"MIGRATION-STDERR-END",file=sys.stderr,flush=True)
print(os.environ["POSTGRES_PASSWORD"],file=sys.stderr,flush=True)
if mode=="nonzero": raise SystemExit(1)
while True: time.sleep(1)
'''

BUSINESS = '''import os
from pathlib import Path
from http.server import BaseHTTPRequestHandler,HTTPServer
mode=Path("scenario.txt").read_text().strip()
with Path("business-launches.log").open("a") as log: log.write(mode+"\\n")
class Handler(BaseHTTPRequestHandler):
 def do_GET(self):
  self.send_response(200);self.end_headers();self.wfile.write(("migration-v2-"+mode).encode())
HTTPServer(("0.0.0.0",int(os.environ["PORT"])),Handler).serve_forever()
'''


def safe_report(value, private_values):
    """报告是展示出口；私有凭据文件不会读入报告。"""
    if isinstance(value, dict):
        return {key: '[REDACTED]' if re.search(r'password|passwd|secret|token', key, re.I)
                else safe_report(item, private_values) for key, item in value.items()}
    if isinstance(value, list):
        return [safe_report(item, private_values) for item in value]
    if isinstance(value, str):
        for secret in private_values:
            value = value.replace(secret, '[REDACTED]')
        value = re.sub(r'([a-z][a-z0-9+.-]*://)[^\s/@]+(?::[^\s/@]*)?@', r'\1[REDACTED]@', value, flags=re.I)
    return value


def write_private_file(path, text):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as file:
        file.write(text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    parser.add_argument('--app-cli', required=True, type=Path)
    parser.add_argument('--file-server-proxy', required=True, type=Path)
    parser.add_argument('--build-source', required=True, type=Path)
    parser.add_argument('--source-dir', type=Path, default=Path('.'))
    parser.add_argument('--report', required=True, type=Path)
    args = parser.parse_args()
    endpoint = require_local_docker_endpoint()
    app = 'migration' + uuid.uuid4().hex[:12]
    name = 'rcoder-migration-v2-' + app
    volume = name + '-workspace'
    workspace = '/home/user/' + app
    state_root = workspace + '/state/' + app
    private_password = secrets.token_urlsafe(32)
    token = app + '-control'
    report = {'success': False, 'app_id': app, 'volume': volume,
              'checks': [], 'scenarios': {}, 'docker_endpoint': endpoint}
    cid = None
    scratch = tempfile.TemporaryDirectory(prefix='userapp-migration-v2-')

    def run(argv, timeout=60, check=True):
        result = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, check=False)
        if check and result.returncode:
            # subprocess exceptions can include command argv and HTTP headers.
            detail = safe_report(result.stderr[-2000:], [private_password, token])
            raise RuntimeError(f'{Path(argv[0]).name} exited {result.returncode}: {detail}')
        return result

    def docker(*argv, timeout=60, check=True):
        return run(['docker', '--host', endpoint, *argv], timeout, check)

    def execute(command, *values, timeout=60, check=True):
        return docker('exec', cid, 'sh', '-ec', command, '--', *values, timeout=timeout, check=check)

    def write(files):
        code = 'import json,sys\nfrom pathlib import Path\nfor name,text in json.loads(sys.argv[1]).items():\n p=Path(name);p.parent.mkdir(parents=True,exist_ok=True);p.write_text(text)\n'
        docker('exec', cid, 'python3', '-c', code, json.dumps(files))

    def check(label, passed, evidence=None):
        report['checks'].append({'name': label, 'ok': bool(passed),
                                 'evidence': safe_report(evidence, [private_password, token])})
        print(label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise RuntimeError(label)

    def request(path, data=None, port=60000):
        argv = [f'http://127.0.0.1:{port}{path}', token]
        command = 'curl -fsS --max-time 120 -H "x-deploy-token: $2" "$1"'
        if data is not None:
            command += ' -H "content-type: application/json" --data "$3"'
            argv.append(json.dumps(data))
        body = json.loads(execute(command, *argv, timeout=130).stdout)
        if not body.get('success'):
            raise RuntimeError(f'{path} returned {body.get("code")}')
        return body['data']

    def wait(label, probe, budget=90):
        deadline = time.monotonic() + budget
        last = None
        while time.monotonic() < deadline:
            try:
                value = probe()
                if value:
                    return value
            except (ValueError, OSError, RuntimeError) as error:
                last = str(error)
            time.sleep(0.3)
        raise RuntimeError(f'{label} timed out: {last}')

    def task_events(task_id, live=False):
        result = execute('curl -fsS --max-time "$2" "$1"',
                         f'http://127.0.0.1:60000/api/v1/userapp/tasks/{task_id}/logs/stream?app_id={app}&from_seq=0',
                         '1' if live else '20', timeout=25, check=False)
        if result.returncode != 0 and not (live and result.returncode == 28):
            raise RuntimeError('task SSE read failed with exit ' + str(result.returncode))
        events = []
        for line in result.stdout.splitlines():
            if line.startswith('data: '):
                try:
                    events.append(json.loads(line[6:]))
                except ValueError:
                    if not live:
                        raise
                    # A timeout may end inside one live frame; only complete
                    # frames count as evidence, and the full terminal read is strict.
        return events

    def task_terminal(task_id, budget=660):
        def probe():
            task = request(f'/api/v1/userapp/tasks/{task_id}?app_id={app}')
            return task if task.get('status') in ('completed', 'failed', 'cancelled') else None
        return wait('original task terminal', probe, budget)

    def original_operation():
        code = 'import json,sys\nfrom pathlib import Path\nr=json.loads(Path(sys.argv[1]).read_text());print(json.dumps({"operation_id":r["operation"]["operation_id"]}))'
        return json.loads(docker('exec', cid, 'python3', '-c', code, state_root + '/.deploy-operation.json').stdout)['operation_id']

    def runtime_operation(operation_id):
        return request('/v1/runtime/operations/' + operation_id, port=3010)

    def process_proof(mode):
        result = execute('cat "$1"', workspace + '/web/migration-process.json', check=False)
        if result.returncode:
            return None
        proof = json.loads(result.stdout)
        return proof if proof.get('scenario') == mode else None

    def process_observations(proof):
        code = '''import json,sys,time
from pathlib import Path
proof=json.loads(sys.argv[1]);observed=[]
for member in proof["members"]:
 p=Path(f'/proc/{member["pid"]}/stat')
 if p.exists():
  f=p.read_text().rsplit(")",1)[1].split();observed.append({"pid":member["pid"],"original_alive":f[19]==member["start_time"],"state":f[0]})
 else: observed.append({"pid":member["pid"],"original_alive":False,"state":None})
print(json.dumps({"members":observed,"elapsed_seconds":time.monotonic()-proof["started_monotonic"]}))'''
        return json.loads(docker('exec', cid, 'python3', '-c', code, json.dumps(proof)).stdout)

    def launches():
        result = execute('cat "$1"', workspace + '/web/business-launches.log', check=False)
        return result.stdout.splitlines() if result.returncode == 0 else []

    def invariant():
        check('same container identity', docker('inspect', '--format', '{{.Id}}', cid).stdout.strip() == cid)
        mounts = json.loads(docker('inspect', '--format', '{{json .Mounts}}', cid).stdout)
        check('same private volume', any(m.get('Name') == volume and m.get('Destination') == '/home/user' for m in mounts), mounts)
        check('volume sentinel retained', execute('cat "$1"', workspace + '/sentinel').stdout == app)
        identity = request('/v1/runtime/identity', port=3010)
        check('same management owner', identity == report['identity'], identity)

    def configure(mode):
        request('/api/v1/userapp/dev/stop', {'app_id': app})
        migrate = '["/definitely-missing/migration-executable"]' if mode == 'spawn' else '["python3", "migrate.py"]'
        manifest = f'''schema_version=1
[project]
service_id="web"
name="Migration advisory Docker fixture"
type="python"
[build]
command=["python3","build.py"]
artifact="artifact.zip"
[devbuild]
command=["python3","build.py"]
[run]
command=["python3","main.py"]
migrate={migrate}
shutdown_timeout_seconds=3
[devrun]
command=["python3","main.py"]
[health]
readiness_path="/"
[proxy]
path="/"
strip_prefix=false
'''
        write({workspace + '/web/project.manifest.toml': manifest,
               workspace + '/web/scenario.txt': mode})

    def proof_of_task(mode, task, operation_id, expected='completed'):
        events = task_events(task['id'])
        operation = runtime_operation(operation_id)
        runtime_events = request(f'/v1/runtime/operations/{operation_id}/events?after_seq=0', port=3010)['events']
        code = 'import json,sys\nfrom pathlib import Path\nr=json.loads(Path(sys.argv[1]).read_text())["request"];print(json.dumps({"operation_id":r["operation_id"],"request_context":r.get("request_context")}))'
        linkage = json.loads(docker('exec', cid, 'python3', '-c', code, state_root + '/operations/' + operation_id + '.json').stdout)
        check(mode + ': original task is bound to original runtime request', linkage['operation_id'] == operation_id and linkage['request_context'] == task['id'], linkage)
        check(mode + ': original task and operation terminals', task['status'] == expected
              and operation['operation_id'] == operation_id
              and operation['runtime_instance_id'] == report['identity']['runtime_instance_id']
              and operation['state'] == ('succeeded' if expected == 'completed' else 'cancelled'),
              {'task': task, 'operation': operation})
        terminals = [event for event in events if event.get('event') in ('completed', 'failed', 'cancelled')]
        check(mode + ': unique final task SSE terminal', len(terminals) == 1
              and terminals[0]['event'] == expected and events[-1]['event'] == expected, events)
        check(mode + ': every runtime event keeps original identity', bool(runtime_events)
              and all(event['operation_id'] == operation_id
                      and event['runtime_instance_id'] == report['identity']['runtime_instance_id'] for event in runtime_events))
        exposed = json.dumps([task, events, operation, runtime_events])
        check(mode + ': public GET and SSE hide PostgreSQL password', private_password not in exposed)
        report['scenarios'][mode] = {'task': task, 'operation': operation, 'task_events': events,
                                     'runtime_events': runtime_events}
        return '\n'.join(event.get('line', '') for event in events if event.get('event') == 'log')

    try:
        source = source_snapshot(args.source_dir.resolve())
        receipt = json.loads(args.build_source.read_text())
        check('registered binaries match frozen source', receipt['source_inputs_sha256'] == source['source_inputs_sha256'])
        hashes = {label: hashlib.sha256(path.read_bytes()).hexdigest() for label, path in
                  [('app-cli', args.app_cli), ('file-server-proxy', args.file_server_proxy)]}
        check('registered binary hashes match executable inputs', receipt.get('binaries') == hashes)
        report.update(source=source, binaries=hashes, test_tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
        image = docker('image', 'inspect', '--format', '{{.Id}}', args.image).stdout.strip()
        architecture = docker('image', 'inspect', '--format', '{{.Architecture}}', image).stdout.strip()
        machine = {'amd64': 62, 'arm64': 183}.get(architecture)
        for path in (args.app_cli, args.file_server_proxy):
            header = path.read_bytes()[:20]
            check('Linux binary architecture: ' + path.name, machine is not None and header[:4] == b'\x7fELF' and int.from_bytes(header[18:20], 'little') == machine)
        report.update(image_id=image, architecture=architecture)
        docker('volume', 'create', '--label', 'rcoder.fixture=' + app, volume)
        folder = Path(scratch.name)
        credentials = folder / 'private.env'
        write_private_file(credentials, 'POSTGRES_USER=dev\nPOSTGRES_DB=dev\nPOSTGRES_PASSWORD=' + private_password + '\n')
        conf = folder / '40-migration.conf'
        conf.write_text(f'''[program:app-cli]
command=/usr/local/bin/app-cli serve --control-only --workspace {workspace}/code
directory={workspace}
autostart=true
exitcodes=0
autorestart=unexpected
startsecs=0
stopasgroup=true
killasgroup=true
stopwaitsecs=3
stdout_logfile=/home/user/logs/app-cli.out.log
redirect_stderr=true
[program:file-server-proxy]
command=/usr/local/bin/file-server-proxy --embed --policy all_rust --port 60000
autostart=true
autorestart=unexpected
startsecs=0
stdout_logfile=/tmp/proxy.log
redirect_stderr=true
''')
        domain = {'authority': 'migration-advisory-v2', 'volume': volume,
                  'instance_source_env': 'RCODER_PHYSICAL_POD_UID', 'instance': ''}
        environment = {'PROJECT_ID': app, 'USERAPP_SINGLE_APP_ID': app, 'APP_ID': app,
                       'USERAPP_WORKSPACE_DIR': workspace, 'LOG_BASE_DIR': '/home/user/logs',
                       'APP_CLI_STATE_ROOT': state_root, 'RCODER_RUNTIME_IMAGE_DIGEST': image,
                       'RCODER_EXECUTION_DOMAIN': json.dumps(domain), 'RCODER_PHYSICAL_POD_UID': str(uuid.uuid4()),
                       'FILE_SERVER_LOG_DIR': '/home/user/proxy-logs', 'FILE_SERVER_APP_CLI_BIN': '/usr/local/bin/app-cli',
                       'APP_CLI_MANAGED': '1', 'SERVICE_TYPE': 'user-app-builder',
                       'APP_CLI_RUNTIME_WORKSPACE': workspace + '/code', 'APP_CLI_DEPLOY_TOKEN': token}
        flags = [item for key, value in environment.items() for item in ('-e', key + '=' + value)]
        cid = docker('create', '--name', name, '--label', 'rcoder.fixture=' + app,
                     '--mount', f'type=volume,src={volume},dst=/home/user,volume-nocopy',
                     '--mount', f'type=bind,src={conf},dst=/etc/supervisor/conf.d/40-migration.conf,readonly',
                     '--env-file', str(credentials), *flags, '--entrypoint', 'sh', image, '-ec',
                     'rm -f /var/run/supervisor.sock /var/run/supervisord.pid; '
                     'install -d -o postgres -g postgres "${PGDATA:-/home/user/.pgdata}"; '
                     f'mkdir -p /app/logs /home/user/logs {workspace}/code; '
                     'exec supervisord -n -c /etc/supervisor/supervisord.conf').stdout.strip()
        report['container_id'] = cid
        docker('cp', str(args.app_cli.resolve()), cid + ':/usr/local/bin/app-cli')
        docker('cp', str(args.file_server_proxy.resolve()), cid + ':/usr/local/bin/file-server-proxy')
        docker('start', cid)
        wait('file-server health', lambda: execute('curl -fsS --max-time 2 http://127.0.0.1:60000/health', check=False).returncode == 0)
        wait('private PostgreSQL login', lambda: execute('PGPASSWORD="$POSTGRES_PASSWORD" PGCONNECT_TIMEOUT=2 psql -X -w -h 127.0.0.1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -qAt -c "SELECT 1"', check=False).stdout.strip() == '1')
        report['identity'] = wait('management identity', lambda: request('/v1/runtime/identity', port=3010))
        check('owner binds actual source root', report['identity']['source_root'] == workspace)
        write({workspace + '/workspace.manifest.toml': 'schema_version=1\n[workspace]\nname="migration-v2"\n',
               workspace + '/web/main.py': BUSINESS, workspace + '/web/migrate.py': MIGRATION,
               workspace + '/web/build.py': 'import zipfile\nwith zipfile.ZipFile("artifact.zip","w") as z:z.write("main.py")\n',
               workspace + '/sentinel': app})
        for mode in ('nonzero', 'spawn', 'timeout', 'stop'):
            configure(mode)
            before_launches = launches()
            accepted = request('/api/v1/userapp/dev/start', {'app_id': app})
            task_id = accepted['task_id']
            if mode in ('timeout', 'stop'):
                proof = wait(mode + ': actual migration process', lambda: process_proof(mode), 90)
                operation_id = proof['operation_id']
                op = runtime_operation(operation_id)
                check(mode + ': original operation remains accepted before control', op['state'] == 'accepted')
                observed = process_observations(proof)
                check(mode + ': original tree is alive', all(member['original_alive'] and member['state'] != 'Z' for member in observed['members']), observed)
                check(mode + ': business has not launched', launches() == before_launches)
                if mode == 'stop':
                    def visible_output():
                        events = task_events(task_id, live=True)
                        lines = '\n'.join(event.get('line', '') for event in events if event.get('event') == 'log')
                        return events if 'MIGRATION-STDOUT-BEGIN' in lines and 'MIGRATION-STDERR-BEGIN' + 'x' * 24576 + 'MIGRATION-STDERR-END' in lines else None
                    live = wait('stop: full migration output reaches task before cancellation', visible_output, 15)
                    check('stop: live task SSE hides PostgreSQL password', private_password not in json.dumps(live))
                    check('stop: original Source operation is still accepted', runtime_operation(operation_id)['state'] == 'accepted')
                    request('/api/v1/userapp/dev/stop', {'app_id': app})
            task = task_terminal(task_id)
            check(mode + ': task GET returns the accepted original id', task['id'] == task_id)
            if mode not in ('timeout', 'stop'):
                operation_id = original_operation()
            lines = proof_of_task(mode, task, operation_id, 'cancelled' if mode == 'stop' else 'completed')
            if mode in ('nonzero', 'timeout', 'stop'):
                check(mode + ': full stdout and 24KiB stderr reach original task SSE', 'MIGRATION-STDOUT-BEGIN' in lines and 'MIGRATION-STDERR-BEGIN' + 'x' * 24576 + 'MIGRATION-STDERR-END' in lines)
            if mode == 'nonzero':
                check('nonzero: error retains real exit code', 'ERROR run.migrate Exit' in lines and 'exit_code=1' in lines)
            if mode == 'spawn':
                check('spawn: captured guardian/executable failure is visible', 'ERROR run.migrate Spawn' in lines and ('No such file' in lines or 'not found' in lines))
            if mode in ('timeout', 'stop'):
                gone = wait(mode + ': original process tree is gone', lambda: (observation if not any(member['original_alive'] for member in observation['members']) else None) if (observation := process_observations(proof)) else None, 15)
                report['scenarios'][mode]['process_cleanup'] = gone
                if mode == 'timeout':
                    check('timeout: real production 300s budget was exercised', gone['elapsed_seconds'] >= 300 and 'timed out after 300000ms' in lines, gone)
                else:
                    check('stop: no late business startup', launches() == before_launches)
            if mode != 'stop':
                code = 'import json,sys,tomllib\nfrom pathlib import Path\ns=tomllib.loads(Path(sys.argv[1]).read_text())["services"];print(next(x["port"] for x in s if x["service_id"]=="web"))'
                port = docker('exec', cid, 'python3', '-c', code, workspace + '/release.lock.toml').stdout.strip()
                body = execute('curl -fsS --max-time 5 "$1"', 'http://127.0.0.1:' + port + '/').stdout
                check(mode + ': actual business HTTP started', body == 'migration-v2-' + mode, body)
                check(mode + ': exactly one business launch', launches() == before_launches + [mode])
            invariant()
        check('source stayed frozen for every scenario', source_snapshot(args.source_dir.resolve())['source_inputs_sha256'] == source['source_inputs_sha256'])
        check('all four real Docker scenarios completed', set(report['scenarios']) == {'nonzero', 'spawn', 'timeout', 'stop'})
        report['success'] = True
    except (Exception, KeyboardInterrupt) as error:
        report.update(success=False, error=safe_report(str(error), [private_password, token]))
    finally:
        if cid:
            try:
                owned = docker('inspect', '--format', '{{index .Config.Labels "rcoder.fixture"}}', cid).stdout.strip() == app
                cleanup = docker('rm', '-f', cid, check=False) if owned else None
                report['cleanup_ok'] = cleanup is not None and cleanup.returncode == 0
            except (Exception, KeyboardInterrupt) as error:
                report.update(cleanup_ok=False, cleanup_error=type(error).__name__)
            report['success'] = report.get('success', False) and report['cleanup_ok']
        scratch.cleanup()
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(safe_report(report, [private_password, token]), ensure_ascii=False, indent=2) + '\n')
        print('REPORT:', args.report, flush=True)
    return 0 if report['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
