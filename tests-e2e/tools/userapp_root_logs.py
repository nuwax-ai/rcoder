#!/usr/bin/env python3
"""Isolated source-root, stopped-log and pkill recovery contract; no LLM or shared deployment."""
import argparse
import hashlib
import json
import subprocess
import tempfile
import time
import uuid
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default='dev-rcoder-agent-runner:latest')
    parser.add_argument('--app-cli', required=True, type=Path)
    parser.add_argument('--file-server-proxy', required=True, type=Path)
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--source-dir', default='.', type=Path)
    parser.add_argument('--build-source', required=True, type=Path)
    args = parser.parse_args()
    app = 'core' + uuid.uuid4().hex[:10]
    name = 'rcoder-root-logs-' + app
    volume = name + '-workspace'
    workspace = '/home/user/' + app
    state_root = workspace + '/state/' + app
    cid = None
    report = {'app_id': app, 'volume': volume, 'checks': [], 'tasks': []}

    def run(argv, check=True, timeout=180):
        return subprocess.run(argv, capture_output=True, text=True,
                              check=check, timeout=timeout)

    def docker(*argv, check=True, timeout=180):
        return run(['docker', *argv], check=check, timeout=timeout)

    def execute(command, *argv, check=True, timeout=60):
        return docker('exec', cid, 'sh', '-ec', command, '--', *argv,
                      check=check, timeout=timeout)

    def assert_check(label, passed, evidence=None):
        report['checks'].append({'name': label, 'ok': bool(passed),
                                 'evidence': evidence})
        print(label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise RuntimeError(label)

    def write(files):
        code = ('import json,pathlib,sys\n'
                'for p,t in json.loads(sys.argv[1]).items():\n'
                ' f=pathlib.Path(p);f.parent.mkdir(parents=True,exist_ok=True);'
                'f.write_text(t)\n')
        docker('exec', cid, 'python3', '-c', code, json.dumps(files))

    def request(path, data=None, port=60000, require_success=True):
        url = f'http://127.0.0.1:{port}{path}'
        command = 'curl -fsS --max-time 90 "$1"'
        values = [url]
        if data is not None:
            command = ('curl -fsS --max-time 90 -H "content-type: application/json" '
                       '--data "$2" "$1"')
            values.append(json.dumps(data))
        body = json.loads(execute(command, *values, timeout=120).stdout)
        if not require_success:
            return body
        if not body.get('success'):
            raise RuntimeError(f'{path}: {body}')
        return body['data']

    def task_events(task_id):
        body = execute('curl -fsS --max-time 10 "$1"',
                       f'http://127.0.0.1:60000/api/v1/userapp/tasks/{task_id}/logs/stream?app_id={app}&from_seq=0').stdout
        return [json.loads(line[6:]) for line in body.splitlines() if line.startswith('data: ')]

    def precheck_failure(label, expected_code):
        result = request('/api/v1/userapp/dev/restart', {'app_id': app}, require_success=False)
        assert_check(label + ' returns immediate failure with real task',
                     not result['success'] and bool(result.get('data', {}).get('task_id')), result)
        task_id = result['data']['task_id']
        task = request('/api/v1/userapp/tasks/' + task_id + '?app_id=' + app)
        assert_check(label + ' snapshot is Failed with typed diagnosis',
                     task['status'] == 'failed' and any(d['code'] == expected_code for d in task['diagnostics']), task)
        events = task_events(task_id)
        assert_check(label + ' log precedes unique terminal failure',
                     bool(events) and events[0]['event'] == 'log'
                     and events[-1]['event'] == 'failed'
                     and sum(e['event'] == 'failed' for e in events) == 1, events)
        assert_check(label + ' does not start business services', content() is None)
        report['tasks'].append(task)

    def wait_for(label, probe, timeout=90):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            try:
                result = probe()
                if result:
                    return result
            except (subprocess.CalledProcessError, ValueError, KeyError) as error:
                last = str(error)
            time.sleep(0.3)
        raise RuntimeError(f'{label} timed out: {last}')

    def wait_management():
        def probe():
            request('/v1/deploy/status', port=3010)
            return request('/v1/runtime/identity', port=3010)
        return wait_for('management', probe, 90)

    def content():
        result = execute('curl -fsS --max-time 3 http://127.0.0.1:9080/',
                         check=False, timeout=10)
        return result.stdout if result.returncode == 0 else None

    def start(action):
        accepted = request('/api/v1/userapp/dev/' + action, {'app_id': app})
        task_id = accepted['task_id']
        def probe():
            data = request('/api/v1/userapp/tasks/' + task_id + '?app_id=' + app)
            status = data.get('status')
            if status == 'completed':
                report['tasks'].append(data)
                return data
            if status in ('failed', 'cancelled'):
                report['tasks'].append(data)
                raise RuntimeError(f'{action} failed: {data}')
            return None
        return wait_for(action + ' task', probe, 150)

    def main_py(marker):
        return ('import os\n'
                'from http.server import BaseHTTPRequestHandler, HTTPServer\n'
                'class H(BaseHTTPRequestHandler):\n'
                ' def do_GET(self):\n'
                '  self.send_response(200);self.end_headers();'
                f'self.wfile.write({marker!r}.encode())\n'
                'HTTPServer(("0.0.0.0",int(os.environ["PORT"])),H).serve_forever()\n')

    def snapshots():
        code = ('import pathlib,json\n'
                f'root=pathlib.Path({state_root!r})\n'
                'out={str(p.relative_to(root)):json.loads(p.read_text()) '
                'for p in root.rglob("*.json") if p.name in '
                '("generation.json","supervisor.json","physical-exit.json")}\n'
                'print(json.dumps(out))\n')
        return json.loads(docker('exec', cid, 'python3', '-c', code).stdout)

    def pid_identities(pids):
        code = ('import pathlib,json,sys\n'
                'out={}\n'
                'for pid in json.loads(sys.argv[1]):\n'
                ' p=pathlib.Path("/proc")/str(pid)/"stat"\n'
                ' try:\n'
                '  s=p.read_text();f=s[s.rfind(")")+2:].split();'
                'out[str(pid)]={"state":f[0],"start_time":f[19]}\n'
                ' except FileNotFoundError:out[str(pid)]=None\n'
                'print(json.dumps(out))\n')
        return json.loads(docker('exec', cid, 'python3', '-c', code, json.dumps(pids)).stdout)

    try:
        repo = args.source_dir.resolve()
        def git(*argv):
            return run(['git', '-C', str(repo), *argv]).stdout
        def source_inputs_hash():
            paths = set(git('ls-files').splitlines() +
                        git('ls-files', '--others', '--exclude-standard').splitlines())
            digest = hashlib.sha256()
            for path in sorted(paths):
                if not (path.startswith('crates/') or path in ('Cargo.toml', 'Cargo.lock')):
                    continue
                if Path(path).suffix not in ('.rs', '.toml', '.lock', '.proto', '.yml', '.json'):
                    continue
                absolute = repo / path
                if absolute.is_file():
                    digest.update(path.encode() + b'\0' + absolute.read_bytes() + b'\0')
            return digest.hexdigest()
        report['source'] = {'commit': git('rev-parse', 'HEAD').strip(),
                            'status': git('status', '--short'),
                            'diff_sha256': hashlib.sha256(git('diff', 'HEAD').encode()).hexdigest(),
                            'source_inputs_sha256': source_inputs_hash()}
        report['build_source'] = json.loads(args.build_source.read_text())
        assert_check('binary build source matches current input snapshot',
                     report['build_source']['source_inputs_sha256'] ==
                     report['source']['source_inputs_sha256'])
        for binary in (args.app_cli, args.file_server_proxy):
            if not binary.is_file():
                raise RuntimeError(f'missing binary: {binary}')
        report['binaries'] = {str(p.resolve()): hashlib.sha256(p.read_bytes()).hexdigest()
                              for p in (args.app_cli, args.file_server_proxy)}
        image_id = docker('image', 'inspect', '--format', '{{.Id}}', args.image).stdout.strip()
        architecture = docker('image', 'inspect', '--format', '{{.Architecture}}', image_id).stdout.strip()
        expected_machine = {'amd64': 62, 'arm64': 183}.get(architecture)
        if expected_machine is None:
            raise RuntimeError('unsupported fixture image architecture: ' + architecture)
        for binary in [args.app_cli, args.file_server_proxy]:
            header = binary.read_bytes()[:20]
            if header[:4] != b'\x7fELF' or len(header) < 20 or int.from_bytes(header[18:20], 'little') != expected_machine:
                raise RuntimeError('binary architecture differs from fixture image: ' + str(binary))
        report['architecture'] = architecture
        report['test_tool_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        report['image_id'] = image_id
        docker('volume', 'create', volume)
        conf_dir = Path(tempfile.mkdtemp(prefix='rcoder-root-logs-conf-'))
        (conf_dir / '40-recovery.conf').write_text(f'''[program:app-cli]
command=/usr/local/bin/app-cli serve --control-only --workspace {workspace}/code
directory={workspace}
autostart=true
exitcodes=0
autorestart=unexpected
startsecs=0
startretries=10
stopsignal=TERM
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
        domain = {'authority': 'app-cli-core-pkill', 'volume': volume,
                  'instance_source_env': 'RCODER_PHYSICAL_POD_UID', 'instance': ''}
        physical_instance = str(uuid.uuid4())
        cid = docker('create', '--name', name,
                     '--mount', f'type=volume,src={volume},dst=/home/user,volume-nocopy',
                     '--mount', f'type=bind,src={conf_dir}/40-recovery.conf,dst=/etc/supervisor/conf.d/40-recovery.conf',
                     '-e', f'PROJECT_ID={app}', '-e', f'USERAPP_SINGLE_APP_ID={app}',
                     '-e', f'USERAPP_WORKSPACE_DIR={workspace}',
                     '-e', 'LOG_BASE_DIR=/home/user/logs',
                     '-e', f'APP_CLI_STATE_ROOT={state_root}',
                     '-e', f'RCODER_RUNTIME_IMAGE_DIGEST={image_id}',
                     '-e', 'RCODER_EXECUTION_DOMAIN=' + json.dumps(domain),
                     '-e', f'RCODER_PHYSICAL_POD_UID={physical_instance}',
                     '-e', 'FILE_SERVER_LOG_DIR=/home/user/proxy-logs',
                     '-e', 'FILE_SERVER_APP_CLI_BIN=/usr/local/bin/app-cli',
                     '-e', 'APP_CLI_MANAGED=1',
                     '-e', 'SERVICE_TYPE=user-app-builder',
                     '-e', f'APP_CLI_RUNTIME_WORKSPACE={workspace}/code',
                     '-e', f'APP_CLI_DEPLOY_TOKEN={app}-token',
                     '--entrypoint', 'sh', image_id, '-ec',
                     'rm -f /var/run/supervisor.sock /var/run/supervisord.pid; '
                     # Same first-boot directory preparation as prepare_pg()
                     # in the image entrypoint; this private empty volume has
                     # never had PGDATA, and no existing database is edited.
                     'install -d -o postgres -g postgres "${PGDATA:-/home/user/.pgdata}"; '
                     f'mkdir -p /app/logs /home/user/logs {workspace}/code; '
                     'exec supervisord -n -c /etc/supervisor/supervisord.conf').stdout.strip()
        report['container_id'] = cid
        report['physical_instance'] = physical_instance
        docker('cp', str(args.app_cli.resolve()), f'{cid}:/usr/local/bin/app-cli')
        docker('cp', str(args.file_server_proxy.resolve()), f'{cid}:/usr/local/bin/file-server-proxy')
        docker('start', cid)
        wait_for('file-server', lambda: execute('curl -fsS --max-time 2 http://127.0.0.1:60000/health', check=False).returncode == 0)
        def pg_probe():
            result = execute('PGPASSWORD="$POSTGRES_PASSWORD" PGCONNECT_TIMEOUT=2 '
                             'psql -X -w -h 127.0.0.1 -U "$POSTGRES_USER" '
                             '-d "$POSTGRES_DB" -qAt -c "SELECT 1"',
                             check=False, timeout=10)
            return result.returncode == 0 and result.stdout.strip() == '1'
        wait_for('fixture PostgreSQL TCP login', pg_probe, 90)
        assert_check('fixture PostgreSQL is TCP ready before business start', pg_probe())
        before = wait_management()
        report['identity_before'] = before
        assert_check('production service type and legacy code launch normalize to source owner',
                     before['source_root'] == workspace, before)
        def no_management_process():
            probe = docker('exec', cid, 'python3', '-c',
                           'import socket; s=socket.socket(); s.settimeout(1); print(s.connect_ex(("127.0.0.1",3010)))',
                           check=False, timeout=5)
            return probe.returncode == 0 and probe.stdout.strip() == '111'
        stopped = execute('supervisorctl -c /etc/supervisor/supervisord.conf stop app-cli', check=False, timeout=15)
        assert_check('management stop accepted for read-only log counterexample', stopped.returncode == 0, stopped.stdout)
        wait_for('management port closed', no_management_process, 15)
        diagnostic = '/home/user/logs/' + app + '/app-cli/owner-recovery.log'
        write({diagnostic: 'management startup failed: preserved diagnostic before HTTP bind\n'})
        sources = request(f'/api/v1/userapp/{app}/dev/logs/sources/query', {})
        assert_check('sources query works with 3010 absent',
                     any(source['source_id'] == 'owner-recovery' for source in sources), sources)
        logs = request(f'/api/v1/userapp/{app}/dev/logs/query', {})
        assert_check('snapshot exposes early management failure without owner',
                     any('preserved diagnostic' in row['line'] for row in logs['logs']), logs)
        stream = execute('curl -fsS --max-time 2 -H "content-type: application/json" --data "{}" "$1"',
                         f'http://127.0.0.1:60000/api/v1/userapp/{app}/dev/logs/stream', check=False, timeout=6)
        assert_check('SSE exposes early management failure without owner',
                     stream.returncode in (0, 28) and 'preserved diagnostic' in stream.stdout, stream.stdout[-4000:])
        assert_check('all three log reads leave management stopped', no_management_process())
        assert_check('log reads do not start business', content() is None)
        execute('supervisorctl -c /etc/supervisor/supervisord.conf start app-cli', timeout=15)
        before = wait_management()
        project_manifest = '''schema_version=1
[project]
service_id="web"
name="Core recovery"
type="python"
[build]
command=["python3","build.py"]
artifact="artifact.zip"
[devbuild]
command=["python3","build.py"]
[run]
command=["python3","main.py"]
[devrun]
command=["python3","main.py"]
[health]
readiness_path="/"
[proxy]
path="/"
strip_prefix=false
'''
        precheck_failure('workspace containing only management state', 'no_services')
        write({workspace + '/code/workspace.manifest.toml': 'schema_version=1\n[workspace]\nname="nested"\n',
               workspace + '/code/web/project.manifest.toml': project_manifest,
               workspace + '/code/web/main.py': main_py('nested-original'),
               workspace + '/code/sentinel': 'do-not-overwrite-imported-source'})
        precheck_failure('nested workspace', 'workspace_root_mismatch')
        write({workspace + '/web/project.manifest.toml': project_manifest})
        precheck_failure('missing root manifest', 'workspace_manifest_missing')
        write({workspace + '/workspace.manifest.toml': 'schema_version=1\n[workspace]\nname="core"\n',
               workspace + '/web/project.manifest.toml': project_manifest,
               workspace + '/web/main.py': main_py('before-pkill'),
               workspace + '/web/build.py': 'from pathlib import Path\nimport zipfile\nPath("builds.log").open("a").write("build\\n")\nwith zipfile.ZipFile("artifact.zip","w") as z:z.write("main.py")\n',
               workspace + '/sentinel': 'preserve-original-data'})
        # Real build failure: the project is valid, but the command fails.
        build_script = execute('cat "$1/web/build.py"', workspace).stdout
        write({workspace + '/web/build.py': 'import sys\nprint("core-build-error-37",file=sys.stderr)\nsys.exit(37)\n'})
        failed = request('/api/v1/userapp/dev/start', {'app_id': app})
        def failed_build():
            task = request('/api/v1/userapp/tasks/' + failed['task_id'] + '?app_id=' + app)
            return task if task['status'] in ('failed', 'completed', 'cancelled') else None
        task = wait_for('failed real build', failed_build, 90)
        events = task_events(failed['task_id'])
        assert_check('build failure keeps module, phase and command stderr',
                     task['status'] == 'failed' and any(d['phase'] == 'build' for d in task['diagnostics'])
                     and any(e['event'] == 'log' and e.get('service') == 'web'
                             and 'core-build-error-37' in e.get('line','') for e in events), {'task':task,'events':events})
        report['tasks'].append(task)
        write({workspace + '/web/build.py': build_script})
        first_task = start('start')
        events = task_events(first_task['id'])
        assert_check('startup result includes visible module log',
                     any(e['event'] == 'log' and e.get('service') == 'web' and '启动成功' in e.get('line','') for e in events),events)
        wait_for('first HTTP', lambda: content() == 'before-pkill')
        assert_check('initial real build and HTTP', content() == 'before-pkill', content())
        manifest_text = execute('cat "$1/workspace.manifest.toml"', workspace).stdout
        write({workspace + '/workspace.manifest.toml': '[broken-current-source'})
        rejected = request('/api/v1/userapp/dev/restart', {'app_id': app}, require_success=False)
        failed_id = rejected.get('data', {}).get('task_id')
        assert_check('invalid new source returns diagnostic task while old service keeps serving',
                     not rejected['success'] and bool(failed_id) and content() == 'before-pkill', rejected)
        write({workspace + '/workspace.manifest.toml': manifest_text})
        report['records_before'] = snapshots()
        report['processes_before'] = execute('ps -eo pid,ppid,args').stdout
        before_pids = execute('pgrep -f "[a]pp-cli"', check=False).stdout.strip().splitlines()
        assert_check('real app-cli process exists before injection', bool(before_pids), before_pids)
        before_pid_identities = pid_identities(before_pids)
        report['killed_process_identities'] = before_pid_identities
        write({workspace + '/web/main.py': main_py('after-pkill')})
        # Pattern does not match the invoking shell or PID1(supervisord).
        killed_at = time.monotonic()
        execute('pkill -9 -f "[a]pp-cli"')
        after = wait_management()
        report['management_recovery_seconds'] = time.monotonic() - killed_at
        report['identity_after'] = after
        after_pid_identities = pid_identities(before_pids)
        assert_check('every captured app-cli process was terminated',
                     all(current is None or current['state'] == 'Z'
                         or current['start_time'] != before_pid_identities[pid]['start_time']
                         for pid, current in after_pid_identities.items()),
                     after_pid_identities)
        assert_check('all-app-cli SIGKILL produces a new management owner',
                     before['runtime_instance_id'] != after['runtime_instance_id'], after)
        start('restart')
        wait_for('rebuilt HTTP', lambda: content() == 'after-pkill')
        assert_check('file-server restart rebuilt and serves new content', content() == 'after-pkill', content())
        builds = execute('cat "$1/web/builds.log"', workspace).stdout.splitlines()
        assert_check('real build executed again', len(builds) >= 2, builds)
        stop = request('/api/v1/userapp/dev/stop', {'app_id': app})
        def stopped_listener():
            result = execute('python3 -c \'import errno,socket; s=socket.socket(); s.settimeout(1); code=s.connect_ex(("127.0.0.1",9080)); s.close(); print(code)\'')
            code = int(result.stdout.strip())
            if code not in (0, 111):
                raise RuntimeError(f'Cannot confirm listener closure: errno={code}')
            return code == 111
        closed = wait_for('actual stopped TCP listener', stopped_listener, 10)
        assert_check('Stop completes and TCP listener refuses connections',
                     stop.get('message') == 'Stopped' and closed, {'stop': stop, 'tcp_errno': 111})
        assert_check('management remains available after Stop', bool(request('/v1/runtime/identity', port=3010)))
        write({workspace + '/web/main.py': main_py('after-stop-start')})
        start('start')
        wait_for('last HTTP', lambda: content() == 'after-stop-start')
        assert_check('Start after Stop rebuilds and provides HTTP', content() == 'after-stop-start', content())
        final_id = docker('inspect', '--format', '{{.Id}}', cid).stdout.strip()
        mount = json.loads(docker('inspect', '--format', '{{json .Mounts}}', cid).stdout)
        assert_check('container identity unchanged throughout', final_id == cid, final_id)
        assert_check('same workspace volume mounted', any(m.get('Name') == volume and m.get('Destination') == '/home/user' for m in mount), mount)
        assert_check('workspace sentinel retained', execute('cat "$1/sentinel"', workspace).stdout == 'preserve-original-data')
        report['records_after'] = snapshots()
        running_before = {path for path, data in report['records_before'].items()
                          if path.endswith('generation.json') and data.get('phase') == 'Running'}
        old_phases = {path: report['records_after'].get(path, {}).get('phase')
                      for path in running_before}
        assert_check('killed owner generation is durably quiescent',
                     bool(old_phases) and all(phase == 'Quiescent' for phase in old_phases.values()),
                     old_phases)
        assert_check('source stayed unchanged during real verification',
                     source_inputs_hash() == report['source']['source_inputs_sha256'])
        report['processes_after'] = execute('ps -eo pid,ppid,args').stdout
        report['success'] = True
    except (Exception, KeyboardInterrupt) as error:
        report.update(success=False, error=str(error))
        if cid:
            report['failure_processes'] = execute('ps -eo pid,ppid,args', check=False).stdout
            code = ('from pathlib import Path\n'
                    'for name in ("/tmp/proxy.log","/home/user/logs/app-cli.out.log",'
                    '"/app/logs/pg.err.log"):\n'
                    ' p=Path(name)\n'
                    ' if p.is_file():print(name);print(p.read_text(errors="replace")[-7000:])\n')
            report['failure_logs'] = docker('exec', cid, 'python3', '-c', code, check=False).stdout
    finally:
        if cid:
            docker('rm', '-f', cid, check=False)
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
        print('REPORT:', args.report, flush=True)
    raise SystemExit(0 if report.get('success') else 1)


if __name__ == '__main__':
    main()
