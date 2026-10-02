#!/usr/bin/env python3
"""app-cli runtime recovery v2/v3 fault matrix (plan §11.2 + RV08 rework).

One container chain exercises the unified in-process owner against real
faults: concurrent callers (A), owner TERM/SIGKILL (B), pkill of every
app-cli process (C), Stop-execution admission barrier with zero
persistence (G), a frozen (SIGSTOP) owner with no second-owner takeover
(J), an app-11 style stale Running + RecoveryRequired fixture imported
while the owner is DOWN and read by the next boot (I), damaged-journal
degraded management with HTTP **and** native precise Stop (R3),
same-container restart through the image's real supervisord entrypoint
(E) and same-volume container replacement (D), and a real-identity
unconfirmed migration barrier with an execution counter (H).

The container's pid 1 is the image's own supervisord (foreground), with
the fixture programs mounted through /etc/supervisor/conf.d — a docker
restart re-boots services exactly the way the platform image does, with
no post-restart pkill or manual service relaunch.

Real supervisord, real builds, real HTTP content assertions. No LLM, no
RCoder control plane. Leaves the workspace volume intact for inspection;
removes only the containers it created.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default='dev-rcoder-agent-runner:latest')
    parser.add_argument('--app-cli', required=True, type=Path)
    parser.add_argument('--file-server-proxy', required=True, type=Path)
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--source-dir', default='.', type=Path,
                        help='repo checkout used for SHA / dirty-status capture')
    args = parser.parse_args()
    for binary in (args.app_cli, args.file_server_proxy):
        if not binary.is_file():
            parser.error(f'missing binary: {binary}')
    app = 'rcv' + uuid.uuid4().hex[:10]
    name = 'rcoder-app-cli-recovery-' + app
    volume = name + '-workspace'
    workspace = '/home/user/' + app
    state_root = '/home/user/.app-cli-state/' + app
    report = {'app_id': app, 'volume': volume, 'checks': [], 'containers': [],
              'scenarios': {},
              'required_scenarios': ['A', 'B', 'C', 'G', 'R4', 'J', 'I',
                                     'R3', 'E', 'D', 'H', 'K'],
              'binaries': {
                  str(p.resolve()): hashlib.sha256(p.read_bytes()).hexdigest()
                  for p in (args.app_cli, args.file_server_proxy)}}
    # RV08：报告绑定源码身份（SHA + 脏改摘要）与镜像 digest。
    repo = Path(args.source_dir).resolve()
    git = lambda *argv: subprocess.run(['git', '-C', str(repo), *argv],
                                       capture_output=True, text=True, check=False)
    report['source'] = {
        'commit': git('rev-parse', 'HEAD').stdout.strip(),
        'dirty': git('status', '--short').stdout.strip().splitlines()[:20],
        'diff_stat': git('diff', '--stat', 'HEAD').stdout.strip()[:2000],
    }
    cid = None

    def docker(*argv, check=True, timeout=240):
        return subprocess.run(['docker', *argv], capture_output=True, text=True,
                              check=check, timeout=timeout)

    def execute(command, *argv, check=True):
        return docker('exec', cid, 'sh', '-ec', command, '--', *argv, check=check)

    def try_execute(command, *argv, timeout=30):
        return docker('exec', cid, 'sh', '-ec', command, '--', *argv,
                      check=False, timeout=timeout)

    def check(label, passed, detail=None, scenario=None):
        report['checks'].append({'name': label, 'ok': bool(passed), 'detail': detail})
        if scenario:
            report['scenarios'].setdefault(scenario, []).append(label)
        print(label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise RuntimeError(label)

    def write(files):
        code = ('import json,pathlib,sys; '
                '[(pathlib.Path(p).parent.mkdir(parents=True,exist_ok=True),'
                'pathlib.Path(p).write_text(t)) for p,t in json.loads(sys.argv[1]).items()]')
        docker('exec', cid, 'python3', '-c', code, json.dumps(files))

    def get(path, port=60000):
        body = json.loads(execute('curl -fsS --max-time 10 "$1"',
                                  f'http://127.0.0.1:{port}{path}').stdout)
        if not body.get('success'):
            raise RuntimeError(f'GET {path}: {body}')
        return body

    def post(action):
        data = json.dumps({'app_id': app})
        body = json.loads(execute(
            'curl -fsS --max-time 150 -H "content-type: application/json" '
            '--data "$1" "$2"', data,
            'http://127.0.0.1:60000/api/v1/userapp/dev/' + action).stdout)
        if not body.get('success'):
            raise RuntimeError(f'{action}: {body}')
        return body['data']

    def content():
        result = try_execute('curl -fsS --max-time 5 http://127.0.0.1:9080/')
        return result.stdout if result.returncode == 0 else None

    def start(action='start'):
        task = post(action)['task_id']
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            body = get('/api/v1/userapp/tasks/' + task + '?app_id=' + app)
            data = body.get('data') or {}
            status = data.get('status')
            if status == 'completed':
                return data
            if status in ('failed', 'cancelled'):
                raise RuntimeError(f'{action} task: {data}')
            time.sleep(0.4)
        raise RuntimeError(f'{action} task timeout')

    def identity():
        return get('/v1/runtime/identity', 3010)['data']['runtime_instance_id']

    def wait_management(deadline_seconds=90):
        # 统一 owner 的身份早于初始化应答（T1b）；这里的"管理可用"指
        # 启动恢复完成（deploy/status 200），与平台请求语义一致。
        deadline = time.monotonic() + deadline_seconds
        last = None
        while time.monotonic() < deadline:
            probe = try_execute(
                'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/deploy/status')
            if probe.returncode == 0:
                try:
                    return identity()
                except (subprocess.CalledProcessError, ValueError, KeyError):
                    pass
            last = probe.stdout[-100:] or last
            time.sleep(0.4)
        raise RuntimeError('management API did not initialize: ' + str(last))

    def owner_pid():
        result = try_execute('pgrep -f "[a]pp-cli serve" | head -1')
        return result.stdout.strip() or None

    def supervisorctl(*argv, timeout=60):
        return docker('exec', cid, 'supervisorctl', *argv, check=False,
                      timeout=timeout)

    def new_container(instance=None):
        nonlocal cid
        image_id = docker('image', 'inspect', '--format', '{{.Id}}',
                          args.image).stdout.strip()
        domain = {'authority': 'app-cli-recovery-v2', 'volume': volume,
                  'instance_source_env': 'RCODER_PHYSICAL_POD_UID', 'instance': ''}
        instance = instance or str(uuid.uuid4())
        # RV08/E：fixture 程序经镜像自身的 supervisord 配置树装载（bind
        # mount 到 conf.d），容器 pid1 = supervisord（前台）——docker
        # restart 即真实入口自动重启，无需 pkill/手工拉服务。入口 wrapper
        # 先清理上一生命周期的 socket/pid 残留再 exec supervisord。
        conf_dir = Path(tempfile.mkdtemp(prefix='rcv-supervisor-'))
        (conf_dir / '40-recovery.conf').write_text(f'''[program:app-cli]
command=/usr/local/bin/app-cli serve --workspace {workspace}
directory={workspace}
autostart=true
exitcodes=0
autorestart=unexpected
startsecs=0
startretries=10
stopsignal=TERM
stopasgroup=true
killasgroup=true
stopwaitsecs=90
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
        cid = docker('create', '--name', name,
                     '--mount', f'type=volume,src={volume},dst=/home/user,volume-nocopy',
                     '--mount', f'type=bind,src={conf_dir}/40-recovery.conf,'
                     f'dst=/etc/supervisor/conf.d/40-recovery.conf',
                     '-e', f'PROJECT_ID={app}',
                     '-e', f'USERAPP_SINGLE_APP_ID={app}',
                     '-e', f'USERAPP_WORKSPACE_DIR={workspace}',
                     '-e', 'LOG_BASE_DIR=/home/user/logs',
                     '-e', f'APP_CLI_STATE_ROOT={state_root}',
                     '-e', f'RCODER_RUNTIME_IMAGE_DIGEST={image_id}',
                     '-e', 'RCODER_EXECUTION_DOMAIN=' + json.dumps(domain),
                     '-e', f'RCODER_PHYSICAL_POD_UID={instance}',
                     '-e', 'FILE_SERVER_LOG_DIR=/home/user/proxy-logs',
                     '-e', 'FILE_SERVER_APP_CLI_BIN=/usr/local/bin/app-cli',
                     # 与真实 builder 注入链同构（docker_manager B03）：
                     # 固定 serve owner 的复用路由需要部署凭据。
                     '-e', 'APP_CLI_MANAGED=1',
                     '-e', 'APP_CLI_DEPLOY_TOKEN=' + app + '-recovery-token',
                     '--entrypoint', 'sh',
                     image_id, '-ec',
                     'rm -f /var/run/supervisor.sock /var/run/supervisord.pid; '
                     f'mkdir -p /app/logs /home/user/logs {workspace}; '
                     'exec supervisord -n -c /etc/supervisor/supervisord.conf'
                     ).stdout.strip()
        report['containers'].append({'id': cid, 'domain_instance': instance})
        # docker create 允许先拷二进制再启动：程序首次拉起即有真实二进制。
        docker('cp', str(args.app_cli.resolve()), f'{cid}:/usr/local/bin/app-cli')
        docker('cp', str(args.file_server_proxy.resolve()),
               f'{cid}:/usr/local/bin/file-server-proxy')
        docker('start', cid)
        services_ready()
        return instance

    def services_ready(timeout=90):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            probe = try_execute('test -S /var/run/supervisor.sock && '
                                'curl -fsS --max-time 2 http://127.0.0.1:60000/health')
            if probe.returncode == 0:
                return
            time.sleep(0.3)
        raise RuntimeError('file-server or supervisord did not initialize')

    def runtime_status():
        return get('/v1/runtime/status', 3010)['data']

    def runtime_post(operation_id, kind, revision, profile=None):
        """直连 owner 运行 API（3010）。返回 (status_code, body)。"""
        body = json.dumps({
            'operation_id': operation_id,
            'expected_runtime_instance_id': identity(),
            'expected_revision': revision,
            'workspace_id': app,
            'kind': kind,
            'profile': profile or {'profile': 'source',
                                   'input': {'workspace_id': app}},
        })
        result = try_execute(
            'curl -sS -o /tmp/rt-out.json -w "%{http_code}" --max-time 20 '
            '-X POST -H "content-type: application/json" '
            '-H "x-deploy-token: $1" --data "$2" "$3"',
            app + '-recovery-token', body,
            'http://127.0.0.1:3010/v1/runtime/operations', timeout=40)
        code = result.stdout.strip()
        payload = try_execute('cat /tmp/rt-out.json').stdout
        try:
            parsed = json.loads(payload)
        except ValueError:
            parsed = {'raw': payload}
        return int(code) if code.isdigit() else 0, parsed

    def runtime_get(operation_id):
        result = try_execute(
            'curl -sS -o /tmp/rt-out.json -w "%{http_code}" --max-time 10 '
            '-H "x-deploy-token: $1" "$2"',
            app + '-recovery-token',
            f'http://127.0.0.1:3010/v1/runtime/operations/{operation_id}',
            timeout=30)
        code = result.stdout.strip()
        payload = try_execute('cat /tmp/rt-out.json').stdout
        try:
            parsed = json.loads(payload)
        except ValueError:
            parsed = {'raw': payload}
        return int(code) if code.isdigit() else 0, parsed

    def wait_terminal(operation_id, timeout=150):
        deadline = time.monotonic() + timeout
        state = None
        while time.monotonic() < deadline:
            code, body = runtime_get(operation_id)
            state = ((body.get('data') or {}).get('state'))
            if state in ('succeeded', 'failed', 'cancelled', 'recovery_required'):
                return state
            time.sleep(0.5)
        raise RuntimeError(f'operation {operation_id} did not reach terminal: {state}')

    def app_files(marker):
        manifest = '''schema_version = 1
[project]
service_id = "web"
name = "Recovery matrix"
type = "python"
[build]
command = ["python3", "-m", "zipfile", "-c", "artifact.zip", "main.py"]
artifact = "artifact.zip"
[run]
command = ["python3", "main.py"]
migrate = ["python3", "-c", "open('migrations.log','a').write('ran\\\\n')"]
[health]
readiness_path = "/"
[proxy]
path = "/"
strip_prefix = false
'''
        devrun = ('\n[devrun]\ncommand = ["python3", "main.py"]\n')
        # G 屏障窗口：服务捕获 SIGTERM 后有界延迟退出——物理 Stop 执行期
        # 足够宽，受理屏障（pending Stop）期间的并发不同 Start 可靠落在
        # Busy 判定窗口内。
        main_py = ('import os,signal,time\n'
                   'from http.server import BaseHTTPRequestHandler,HTTPServer\n'
                   'def _bye(sig,frame):\n'
                   '    time.sleep(8)\n'
                   '    os._exit(0)\n'
                   'signal.signal(signal.SIGTERM,_bye)\n'
                   'class H(BaseHTTPRequestHandler):\n def do_GET(self):\n'
                   '  self.send_response(200)\n  self.end_headers()\n'
                   f'  self.wfile.write(b"{marker}")\n'
                   'HTTPServer(("0.0.0.0",int(os.environ["PORT"])),H).serve_forever()\n')
        return {workspace + '/workspace.manifest.toml':
                    'schema_version=1\n[workspace]\nname="recovery"\n',
                workspace + '/web/project.manifest.toml': manifest + devrun,
                workspace + '/web/main.py': main_py,
                workspace + '/sentinel': marker}

    def migration_runs():
        result = try_execute(
            'wc -l < "$1/web/migrations.log" 2>/dev/null || echo 0', workspace)
        return int(result.stdout.strip() or 0)

    def degrade_with_bad_journal():
        """损坏业务 journal → 干净停 serve（supervisorctl，不自动复活）→
        重新拉起 → 业务初始化失败进入降级驻留，管理面保持可用。"""
        state_root_dir = execute(
            'find /home/user -maxdepth 5 -name supervisor.json -printf "%h\n" '
            '| head -1').stdout.strip()
        write({state_root_dir + '/.deploy-operation.json': '{damaged-journal'})
        supervisorctl('stop', 'app-cli')
        stopped = try_execute('pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('R3: supervisord program stopped cleanly before relaunch',
              stopped == '0', stopped, scenario='R3')
        supervisorctl('start', 'app-cli')
        deadline = time.monotonic() + 120
        probe = None
        while time.monotonic() < deadline:
            probe = try_execute(
                'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/runtime/identity')
            if probe.returncode == 0:
                return state_root_dir
            time.sleep(1)
        raise RuntimeError('degraded owner did not come back: '
                           + (probe.stdout[-150:] if probe else 'timeout'))

    try:
        docker('volume', 'create', volume)
        # ── A：并发调用方收敛到同一 owner ─────────────────────────────
        new_container()
        wait_management(120)
        write(app_files('recovery-a-1'))
        # RV08/A：先完成平台 start（release.lock 落盘），再以 agent 形态手动
        # run——第二个 CLI 入口必须显式转交给常驻 serve owner（dispatch 提交
        # 自身 Start），不出现第二编排/监听。锁竞争面由 J/C 场景覆盖。
        start('start')
        check('A: business HTTP serves content', content() == 'recovery-a-1',
              content(), scenario='A')
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli run --workspace "$1" > /tmp/manual-run.log 2>&1',
               '--', workspace)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            procs = execute(
                'ps -eo pid,args | grep "[a]pp-cli" | grep -v grep',
                check=False).stdout
            if 'app-cli run' in procs:
                break
            time.sleep(0.5)
        discovery = execute(
            'find /home/user/.app-cli-state -name supervisor.json').stdout.strip()
        check('A: exactly one supervisor discovery',
              len(discovery.splitlines()) == 1, discovery, scenario='A')
        first_identity = identity()
        listeners = execute(
            'pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('A: exactly one serve owner process', listeners == '1',
              procs, scenario='A')
        deadline = time.monotonic() + 90
        manual = ''
        while time.monotonic() < deadline:
            manual = try_execute('cat /tmp/manual-run.log').stdout
            if ('dispatching to the running owner' in manual
                    or 'dispatched start completed' in manual
                    or 'Error' in manual):
                break
            time.sleep(1)
        # 手动 run 的 Start 走"最后受理生效"接替：等待其自然终态后业务内容
        # 保持（同一 release 重编排）。
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if try_execute('pgrep -f "[a]pp-cli run" | wc -l'
                           ).stdout.strip() == '0':
                break
            time.sleep(1)
        manual = try_execute('cat /tmp/manual-run.log').stdout
        # RV08/A：裁决必须是**显式**转交/拒绝文案——"出现 owner 字样"
        # 这类宽松匹配不算数；第二监听冲突恒为失败。
        bound_conflict = ('address already in use' in manual.lower()
                          or ('bind' in manual.lower() and 'failed' in manual.lower()))
        handed_over = ('dispatching to the running owner' in manual
                       or 'dispatched start completed on the running owner' in manual)
        rejected = ('already has an owner' in manual
                    or 'operation is in progress' in manual.lower()
                    or 'conflict' in manual.lower()
                    or 'superseded' in manual.lower())
        check('A: concurrent run explicitly handed over or rejected',
              (handed_over or rejected) and not bound_conflict, manual[-500:],
              scenario='A')
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if content() == 'recovery-a-1':
                break
            time.sleep(1)
        check('A: business still serves after handover replacement',
              content() == 'recovery-a-1', content(), scenario='A')

        # ── B：owner TERM → 干净退出不复活；SIGKILL → supervisord 自愈 ──
        identity_before = first_identity
        stop_result = post('stop')
        check('B: stop confirms business stopped',
              stop_result.get('message') == 'Stopped' and content() is None,
              stop_result, scenario='B')
        check('B: management stays alive after business stop',
              identity() == identity_before, None, scenario='B')
        check('B: repeat stop idempotent',
              post('stop').get('message') == 'Stopped' and content() is None,
              None, scenario='B')
        pid = owner_pid()
        execute('kill -KILL "$1"', pid)
        # supervisord 拉起新 owner：完整初始化（含旧代次引擎清理围栏）后才
        # 能受理下一个业务请求。
        try:
            revived = wait_management(120)
        except RuntimeError:
            revived = None
        check('B: killed owner recovers via supervisord',
              revived is not None and revived != identity_before, revived,
              scenario='B')
        start('restart')
        check('B: business restarts after owner recovery',
              content() == 'recovery-a-1', content(), scenario='B')

        # ── C：同名辅助进程全部被杀（pkill app-cli）─────────────────
        write({workspace + '/web/main.py': app_files('recovery-c-2')[
            workspace + '/web/main.py']})
        execute('pkill -9 -f "[a]pp-cli" || true')
        try:
            recovered = wait_management(150)
        except RuntimeError:
            recovered = None
        check('C: management rebuilt after pkill of every app-cli',
              recovered is not None, recovered, scenario='C')
        start('restart')
        check('C: real rebuild changes HTTP content',
              content() == 'recovery-c-2', content(), scenario='C')
        check('C: workspace sentinel retained',
              execute('cat "$1/sentinel"', workspace).stdout == 'recovery-a-1',
              None, scenario='C')

        # ── G：Stop 执行屏障——不同 Start Busy 且零持久化；同 ID 重放 ──
        # RV08/G：真实受控 Stop（物理停进行中，pending 屏障持有期间）并发
        # 不同 Start：必须 Busy（附当前操作身份）且**零持久化**（不排队）。
        post('stop')
        start('restart')
        revision = runtime_status()['revision']
        stop_id = 'g-stop-' + uuid.uuid4().hex[:8]
        code, body = runtime_post(stop_id, 'stop', revision)
        check('G: runtime stop admitted', code == 202
              and (body.get('data') or {}).get('state') == 'accepted',
              body, scenario='G')
        # 屏障窗口内：不同 Start / 不同 Restart 均 Busy，零副作用。
        # （revision 用 stop 推进后的值——目标状态是停止完成后。）
        busy_start_id = 'g-start-' + uuid.uuid4().hex[:8]
        code_s, body_s = runtime_post(busy_start_id, 'start', revision + 1)
        active = body_s.get('active_operation_id')
        check('G: different start during stop execution is Busy',
              code_s == 409 and body_s.get('code') == 'ERR_OPERATION_IN_PROGRESS'
              and active == stop_id, body_s, scenario='G')
        busy_restart_id = 'g-restart-' + uuid.uuid4().hex[:8]
        code_r, body_r = runtime_post(busy_restart_id, 'restart', revision + 1)
        check('G: different restart during stop execution is Busy',
              code_r == 409 and body_r.get('code') == 'ERR_OPERATION_IN_PROGRESS',
              body_r, scenario='G')
        record = execute(
            'ls "$1/operations/" 2>/dev/null | grep -c "$2" || true',
            state_root, busy_start_id).stdout.strip()
        check('G: busy start left zero persisted record',
              record == '0', record, scenario='G')
        # 同 ID 重放：读回已受理进度（accepted/执行中），不重复受理。
        code_p, body_p = runtime_post(stop_id, 'stop', revision)
        check('G: same-id stop replays recorded state', code_p == 202
              and (body_p.get('data') or {}).get('operation_id') == stop_id,
              body_p, scenario='G')
        stop_state = wait_terminal(stop_id)
        check('G: admitted stop reaches terminal', stop_state == 'succeeded',
              stop_state, scenario='G')
        replay_code, replay = runtime_post(stop_id, 'stop', revision)
        check('G: same-id stop after terminal replays terminal state',
              replay_code == 202
              and (replay.get('data') or {}).get('state') == 'succeeded',
              replay, scenario='G')
        # Stop 完成后新 Start 正常执行。
        after_id = 'g-after-' + uuid.uuid4().hex[:8]
        code_a, body_a = runtime_post(after_id, 'start', revision + 1)
        check('G: fresh start after stop executes', code_a == 202, body_a,
              scenario='G')
        wait_terminal(after_id)
        check('G: fresh start serves content', content() == 'recovery-c-2',
              content(), scenario='G')

        # ── R4：run 首个 owner + 另一进程 HTTP Stop/Restart 消费 ──────
        post('stop')
        # RV08/R4：先干净停止 supervised serve 程序（exitcode=0 不复活），
        # 隔离出"无 owner"世界，run 才是真正首发 owner。
        supervisorctl('stop', 'app-cli')
        stopped = try_execute('pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('R4: supervised serve program stopped before run bootstrap',
              stopped == '0', stopped, scenario='R4')
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli run --workspace "$1" --log-dir /home/user/logs '
               '--admin-addr 0.0.0.0:3010 >/tmp/r4run.log 2>&1', '--', workspace)
        try:
            run_owner = wait_management(120)
        except RuntimeError:
            run_owner = None
        check('R4: real run bootstraps the first owner',
              run_owner is not None, run_owner, scenario='R4')
        run_pid = try_execute('pgrep -f "[a]pp-cli run" | head -1').stdout.strip()
        ps_r4 = try_execute(
            'ps -eo pid,args | grep "[a]pp-cli" | grep -v grep').stdout
        discovery_r4 = json.loads(execute(
            'cat "$(find /home/user/.app-cli-state -name supervisor.json | head -1)"'
        ).stdout)
        # pid/命令、native supervisor 身份与 API 身份对应：run 进程唯一且
        # discovery 属活实例（phase 非 recovery_required）。
        check('R4: run process is the sole app-cli orchestrator',
              bool(run_pid) and ps_r4.count('app-cli run') == 1
              and 'app-cli serve' not in ps_r4, ps_r4, scenario='R4')
        check('R4: discovery belongs to the live run owner',
              discovery_r4['snapshot']['phase'] in ('ready', 'stopped'),
              discovery_r4['snapshot']['phase'], scenario='R4')
        report.setdefault('r4_diag', {})
        report['r4_diag']['run_pid'] = run_pid
        report['r4_diag']['instance'] = discovery_r4['instance']
        report['r4_diag']['api_identity'] = identity()
        # 另一进程提交的 Start（file-server 复用路由）必须被 run 的
        # server_loop 消费——操作终态 + 实际 HTTP 内容。
        try:
            start('restart')
        except RuntimeError as error:
            report.setdefault('r4_fail', {})
            report['r4_fail']['error'] = str(error)
            report['r4_fail']['ps'] = try_execute(
                'ps -eo pid,args | grep "[a]pp-cli" | grep -v grep',
                timeout=30).stdout
            report['r4_fail']['r4run'] = try_execute(
                'tail -c 2000 /tmp/r4run.log 2>/dev/null', timeout=30).stdout
            raise
        check('R4: platform-submitted start consumed by run owner',
              content() == 'recovery-c-2', content(), scenario='R4')
        # HTTP Stop（同一管理面）也被消费；run 形态前台退出码语义由
        # 会话 restart_on_exit=false 保证（进程退出=业务终态）。
        stop_r4 = post('stop')
        check('R4: platform-submitted stop consumed by run owner',
              stop_r4.get('message') == 'Stopped' and content() is None,
              stop_r4, scenario='R4')
        time.sleep(2)
        run_gone = try_execute('pgrep -f "[a]pp-cli run" | wc -l').stdout.strip()
        check('R4: run owner exited after foreground stop', run_gone == '0',
              run_gone, scenario='R4')
        # Restart again: run owner 已按前台语义退出——重新拉起 supervised
        # serve 程序接管（平台常驻形态），业务恢复。
        supervisorctl('start', 'app-cli')
        wait_management(120)
        start('restart')
        check('R4: restart after run-owner exit recovers',
              content() == 'recovery-c-2', content(), scenario='R4')
        # R4 尾部：run owner 处理 Stop 时被 SIGKILL（应答回程丢失）——
        # 只有磁盘上的真实原收据算数：不伪造 Succeeded、按原 ID 收束；
        # 旧实例 ID 的重放被拒且零新记录。
        post('stop')
        supervisorctl('stop', 'app-cli')
        stopped_r4b = try_execute('pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('R4: serve stopped before the second run bootstrap',
              stopped_r4b == '0', stopped_r4b, scenario='R4')
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli run --workspace "$1" --log-dir /home/user/logs '
               '--admin-addr 0.0.0.0:3010 >/tmp/r4run2.log 2>&1', '--', workspace)
        check('R4: second run bootstraps the owner',
              wait_management(120) is not None, None, scenario='R4')
        start('restart')
        check('R4: business serving before the lost-reply stop',
              content() == 'recovery-c-2', content(), scenario='R4')
        revision_lr = runtime_status()['revision']
        lost_id = 'r4-lost-' + uuid.uuid4().hex[:8]
        lost_submit = try_execute(
            'curl -sS --max-time 60 -o /tmp/lost-reply.json '
            '-H "content-type: application/json" -H "x-deploy-token: $1" '
            '--data "$2" "$3" >/dev/null 2>&1 & '
            'sleep 1; kill -9 "$(pgrep -f "[a]pp-cli run" | head -1)"',
            app + '-recovery-token',
            json.dumps({'operation_id': lost_id,
                        'expected_runtime_instance_id': identity(),
                        'expected_revision': revision_lr,
                        'workspace_id': app,
                        'kind': 'stop',
                        'profile': {'profile': 'source',
                                    'input': {'workspace_id': app}}}),
            'http://127.0.0.1:3010/v1/runtime/operations', timeout=90)
        assert lost_submit.returncode == 0, lost_submit.stderr[-200:]
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if try_execute('pgrep -f "[a]pp-cli run" | wc -l').stdout.strip() == '0':
                break
            time.sleep(0.5)
        run2_gone = try_execute('pgrep -f "[a]pp-cli run" | wc -l').stdout.strip()
        check('R4: run owner killed mid-stop', run2_gone == '0', run2_gone,
              scenario='R4')
        # 真实原收据：该 ID 恰好一条持久化记录（受理即落盘）。
        receipt_files = execute(
            'ls "$1/operations/" 2>/dev/null | grep -c "$2" || true',
            state_root, lost_id).stdout.strip()
        check('R4: exactly one persisted record for the lost-reply stop',
              receipt_files == '1', receipt_files, scenario='R4')
        # 新 serve 接管后按真实收据收束该 ID（未确认执行不伪造 Succeeded）。
        supervisorctl('start', 'app-cli')
        wait_management(150)
        settled = None
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            code_lr, body_lr = runtime_get(lost_id)
            settled = ((body_lr.get('data') or {}).get('state'))
            if settled in ('succeeded', 'failed', 'cancelled',
                           'recovery_required'):
                break
            time.sleep(0.5)
        check('R4: lost-reply stop settles from its real receipt',
              settled in ('succeeded', 'failed', 'recovery_required'), settled,
              scenario='R4')
        # 同 ID 重放（新实例身份）：不制造新操作（记录数不增）。
        replay_lr = runtime_post(lost_id, 'stop', revision_lr)
        records_lr = execute(
            'ls "$1/operations/" 2>/dev/null | grep -c "$2" || true',
            state_root, lost_id).stdout.strip()
        check('R4: same-id replay creates no new records',
              records_lr == '1', [replay_lr[0], records_lr], scenario='R4')
        start('restart')
        check('R4: business recovers after lost-reply reconciliation',
              content() == 'recovery-c-2', content(), scenario='R4')

        # ── J：挂死（SIGSTOP）owner 的有界边界 ──────────────────────
        pid = owner_pid()
        identity_at_freeze = identity()
        execute('kill -STOP "$1"', pid)
        stopped_probe = try_execute(
            'curl -fsS --max-time 6 http://127.0.0.1:3010/v1/runtime/identity',
            timeout=20)
        check('J: frozen owner does not answer management probes',
              stopped_probe.returncode != 0, stopped_probe.stdout[-200:],
              scenario='J')
        second = try_execute(
            'serve_rc=0; timeout 30 app-cli serve --workspace "$1" '
            '>/tmp/second-owner.log 2>&1 || serve_rc=$?; '
            'echo "second-owner-rc=$serve_rc"; tail -c 600 /tmp/second-owner.log',
            workspace, timeout=75)
        combined = (second.stdout or '') + (second.stderr or '')
        check('J: no second owner while lock is held by frozen process',
              'second-owner-rc=0' not in combined and 'owner' in combined.lower(),
              combined[-600:], scenario='J')
        execute('kill -CONT "$1"', pid)
        check('J: management responds again after SIGCONT',
              identity() == identity_at_freeze, None, scenario='J')
        check('J: business unaffected by freeze window',
              content() == 'recovery-c-2', content(), scenario='J')

        # ── I：app-11 升级 fixture（owner 停机导入，新 boot 读取）─────
        # RV08/I：先结束 owner（干净停程序，不复活），再导入脱敏 fixture，
        # 再启动新二进制——磁盘状态由下一次 owner 启动真实读取，不是活
        # owner 的内存覆盖。形态：RecoveryRequired discovery + 前容器域章
        # 的 Running 旧代次（无退出回执）并存。
        post('stop')
        supervisorctl('stop', 'app-cli')
        fixture_generation = '11111111-2222-4333-8444-555555555555'
        fixture_domain = {'authority': 'app-cli-recovery-v2', 'volume': volume,
                          'instance': 'old-pod-' + uuid.uuid4().hex[:8]}
        write({state_root + '/work/' + fixture_generation + '/generation.json':
                   json.dumps({'version': 1, 'id': fixture_generation,
                               'supervisor': 'dead-supervisor-from-fixture',
                               'token': 'fixture-token', 'intent': 'run',
                               'phase': 'Running', 'worker_pid': 424242,
                               'exit_code': None, 'error': None,
                               'physical_domain': fixture_domain}),
               state_root + '/supervisor.json':
                   json.dumps({'version': 2, 'instance': 'fixture-owner',
                               'address': '127.0.0.1:1', 'token': 'fixture',
                               'snapshot': {'version': 1,
                                            'binding': {'component': 'app-cli',
                                                        'resource': workspace},
                                            'supervisor_id': 'fixture-owner',
                                            'generation': fixture_generation,
                                            'phase': 'recovery_required',
                                            'intent': 'run',
                                            'operation_id': None,
                                            'error': 'generation cleanup is '
                                                     'unconfirmed: Running',
                                            'problem': None},
                                            'requests': []})})
        supervisorctl('start', 'app-cli')
        wait_management(150)
        start('restart')
        check('I: app-11 style fixture recovers without manual edits',
              content() == 'recovery-c-2', content(), scenario='I')
        preserved = json.loads(execute(
            'cat "$1"', state_root + '/work/' + fixture_generation +
            '/generation.json').stdout)
        check('I: previous-container Running record preserved as history',
              preserved['phase'] == 'Running'
              and preserved['physical_domain']['instance'].startswith('old-pod-'),
              preserved['phase'], scenario='I')

        # ── R3：坏 journal 期间的精确 Stop（HTTP + native）+ 管理可用 ──
        # 损坏业务部署 journal → 干净重启 serve → 新 owner 的业务初始化在
        # 坏 journal 上失败进入降级驻留：窗口内 Stop 必须受理、identity 可
        # 查询、3010 不消失；修复后业务恢复。覆盖 HTTP 与 native 两个停止
        # 操作系统。
        state_root_dir = degrade_with_bad_journal()
        check('R3: management survives damaged-journal business failure',
              True, None, scenario='R3')
        stop_r3 = post('stop')
        check('R3: HTTP stop accepted during degraded business state',
              stop_r3.get('message') == 'Stopped', stop_r3, scenario='R3')
        id_probe2 = try_execute(
            'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/runtime/identity')
        check('R3: identity queryable after degraded stop',
              id_probe2.returncode == 0, id_probe2.stdout[-120:], scenario='R3')
        # native StopWork（第二辆降级列车）：控制协议直连 owner。
        state_root_dir = degrade_with_bad_journal()
        native = try_execute(
            'app-cli owner stop --workspace "$1"', workspace, timeout=60)
        check('R3: native StopWork completes on degraded owner',
              native.returncode == 0 and 'stopped' in (native.stdout or '').lower(),
              (native.stdout or '')[-300:], scenario='R3')
        id_probe3 = try_execute(
            'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/runtime/identity')
        check('R3: identity queryable after native stop',
              id_probe3.returncode == 0, id_probe3.stdout[-120:], scenario='R3')
        # RV03/F2 收口：损坏（无法解码）journal 由 owner 隔离为 .corrupt-*
        # 备份并重建——**不需要手工修复/删除**，下一显式新请求直接恢复。
        start('restart')
        check('R3: business recovers from damaged journal without manual repair',
              content() == 'recovery-c-2', content(), scenario='R3')
        corrupt_backup = execute(
            'ls "$1"/.deploy-operation.corrupt-*.json 2>/dev/null | wc -l',
            state_root_dir).stdout.strip()
        check('R3: damaged journal preserved as corrupt backup',
              corrupt_backup != '0', corrupt_backup, scenario='R3')

        # ── E：同容器重启（docker restart，容器 ID 不变）────────────
        # RV08/E：pid1 是镜像自身 supervisord——restart 即真实入口自动重启
        # 服务（app-cli serve / file-server-proxy / PG / ttyd），无额外
        # pkill、无手工拉服务。
        post('stop')
        docker('restart', cid)
        wait_management(150)
        check('E: same-container restart keeps business stopped',
              content() is None, content(), scenario='E')
        start('restart')
        check('E: start after container restart works',
              content() == 'recovery-c-2', content(), scenario='E')

        # ── D：同卷容器重建（新容器、新物理实例身份）────────────────
        post('stop')
        docker('rm', '-f', cid)
        cid = None
        new_container()
        wait_management(120)
        start('restart')
        check('D: same-volume replacement recovers and serves',
              content() == 'recovery-c-2', content(), scenario='D')
        check('D: workspace data retained across replacement',
              execute('cat "$1/sentinel"', workspace).stdout == 'recovery-a-1',
              None, scenario='D')

        # ── H：真实身份的未确认迁移屏障 + 执行计数 ────────────────────
        # RV08/H：从上一次成功编排写的真实回执中取当前 release/service 的
        # identity，改写为 completed=false——这是真实的在途屏障；依赖迁移
        # 的启动必须给出具体原因（不静默重跑、不伪造），Stop/管理可用。
        post('stop')
        receipt_dir = state_root + '/migration-receipts'
        receipts = execute('ls "$1"/*.json 2>/dev/null | head -5',
                           receipt_dir).stdout.split()
        check('H: a real migration receipt exists from prior orchestration',
              bool(receipts), receipt_dir, scenario='H')
        first_receipt = json.loads(execute('cat "$1"', receipts[0]).stdout)
        real_identity = first_receipt['identity']
        runs_before = migration_runs()
        write({receipts[0]: json.dumps(
            {'identity': real_identity, 'completed': False})})
        stop_h = post('stop')
        check('H: stop works with unconfirmed migration present',
              stop_h.get('message') == 'Stopped' and content() is None,
              stop_h, scenario='H')
        check('H: management still queryable',
              bool(identity()), None, scenario='H')
        restart_h = None
        try:
            restart_h = start('restart')
        except RuntimeError as error:
            restart_h = str(error)
        # 唯一合法结局：启动被拒并给出迁移具体原因（真实在途身份下，
        # "绕过并成功"= 伪造/静默重跑，恒为失败）。
        refused = isinstance(restart_h, str) and (
            'migration' in restart_h.lower()
            or 'reconciliation' in restart_h.lower()
            or 'unconfirmed' in restart_h.lower())
        check('H: unconfirmed migration refuses dependent start with reason',
              refused, str(restart_h)[:300], scenario='H')
        runs_after = migration_runs()
        receipt_now = json.loads(execute('cat "$1"', receipts[0]).stdout)
        check('H: migration neither rerun nor fabricated',
              receipt_now.get('completed') is False and runs_after == runs_before,
              {'receipt': receipt_now, 'runs': [runs_before, runs_after]},
              scenario='H')
        # R-H1 实链尾部：恢复保持挂起期间的原生 Stop——修复前该窗口的
        # stop waiter 读不到自身 ID 的终态快照（stop-owner 恢复链超时）。
        # 现在必须按其原操作身份精确完成，且不解除数据未知保护。
        native_h = try_execute(
            'app-cli owner stop --workspace "$1"', workspace, timeout=90)
        check('H: native stop during recovery hold completes exactly once',
              native_h.returncode == 0
              and 'stopped' in (native_h.stdout or '').lower(),
              (native_h.stdout or '')[-200:], scenario='H')
        # 迁移回执未确认：Stop 不解除未知迁移保护（回执仍 completed=false）。
        receipt_after_stop = json.loads(
            execute('cat "$1"', receipts[0]).stdout)
        check('H: stop does not release the unconfirmed migration barrier',
              receipt_after_stop.get('completed') is False,
              receipt_after_stop, scenario='H')
        # 屏障解除（如实确认）后：当前启动请求继续 → HTTP → Stop → 再启动。
        write({receipts[0]: json.dumps(
            {'identity': real_identity, 'completed': True})})
        start('restart')
        check('H: business recovers after migration barrier confirmed',
              content() == 'recovery-c-2', content(), scenario='H')
        runs_recovered = migration_runs()
        # 确认回执后的恢复编排：迁移按 journal 去重——同 release 身份已
        # 确认则不再执行（+0），新身份恰好执行一次（+1）；两者都合法，
        # >1 = 重复执行才是失败。
        check('H: recovered orchestration respects migration dedupe',
              runs_recovered in (runs_before, runs_before + 1),
              [runs_before, runs_recovered], scenario='H')
        stop_after = post('stop')
        check('H: stop after recovery completes',
              stop_after.get('message') == 'Stopped' and content() is None,
              stop_after, scenario='H')
        start('restart')
        check('H: restart after stop serves again',
              content() == 'recovery-c-2', content(), scenario='H')
        runs_final = migration_runs()
        check('H: restart after stop respects migration dedupe',
              runs_final in (runs_recovered, runs_recovered + 1),
              [runs_recovered, runs_final], scenario='H')

        # ── K：.run 激活 + 制品 zip 缓存丢失的显式恢复 ────────────────
        # 真实入口全链：dev/restart 产物态构建（真 zip）→ owner Deploy
        # (ArtifactId) 激活 .run；切回源码态；删缓存后同制品重部署
        # （运行态 next_prepared / 空闲主循环两条消费路径）；journal 损坏
        # 隔离后前台 app-cli run <proj>/.run 恢复。
        def artifact_files(marker):
            files = app_files(marker)
            files[workspace + '/web/project.manifest.toml'] = \
                files[workspace + '/web/project.manifest.toml'].replace(
                    '\n[devrun]\ncommand = ["python3", "main.py"]\n', '')
            return files

        marker_k = 'recovery-k-1'
        post('stop')
        write(artifact_files(marker_k))
        start('restart')
        check('K: artifact-mode build deploys and serves',
              content() == marker_k, content(), scenario='K')
        built = execute('ls "$1"/builds/workspace-package-*.zip 2>/dev/null',
                        workspace).stdout.split()
        check('K: registered artifact zip exists after build',
              len(built) >= 1, built, scenario='K')
        release_k = built[-1].rsplit('workspace-package-', 1)[1][:-4]
        check('K: .run activated with the built release',
              release_k in execute('cat "$1/.run/release.lock.toml"',
                                   workspace).stdout, release_k, scenario='K')
        # 切回源码态（同 owner、业务运行中）。
        write(app_files('recovery-c-2'))
        start('restart')
        check('K: source mode switch back serves source content',
              content() == 'recovery-c-2', content(), scenario='K')
        # 清缓存 → 同制品重部署：源码态 owner 切换到已激活 .run（身份
        # 核验后的复用，不需要 zip）。
        execute('rm -f "$1"/builds/workspace-package-*.zip', workspace)
        revision_k = runtime_status()['revision']
        artifact_profile = {
            'profile': 'artifact',
            'input': {'artifact': {'source': 'artifact_id',
                                   'value': {'artifact_id': release_k}}}}
        k_switch = 'k-switch-' + uuid.uuid4().hex[:8]
        code_k, body_k = runtime_post(k_switch, 'deploy', revision_k,
                                      artifact_profile)
        check('K: cache-lost redeploy admitted while source serves',
              code_k == 202, body_k, scenario='K')
        check('K: cache-lost redeploy switches to the activated artifact',
              wait_terminal(k_switch) == 'succeeded'
              and content() == marker_k, content(), scenario='K')
        # 空闲路径：Stop 后同制品再部署（无 zip、业务已停）。
        post('stop')
        revision_k2 = runtime_status()['revision']
        k_idle = 'k-idle-' + uuid.uuid4().hex[:8]
        code_k2, body_k2 = runtime_post(k_idle, 'deploy', revision_k2,
                                        artifact_profile)
        check('K: idle cache-lost redeploy starts the business',
              code_k2 == 202 and wait_terminal(k_idle) == 'succeeded'
              and content() == marker_k, body_k2, scenario='K')
        # journal 损坏隔离 → 前台 run <proj>/.run：唯一真实恢复入口。
        post('stop')
        state_root_k = execute(
            'find /home/user -maxdepth 5 -name supervisor.json -printf "%h\n" '
            '| head -1').stdout.strip()
        write({state_root_k + '/.deploy-operation.json': '{damaged-k'})
        supervisorctl('stop', 'app-cli')
        stopped_k = try_execute('pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('K: serve stopped before the foreground recovery run',
              stopped_k == '0', stopped_k, scenario='K')
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli run --workspace "$1" --log-dir /home/user/logs '
               '--admin-addr 0.0.0.0:3010 >/tmp/krun.log 2>&1',
               '--', workspace + '/.run')
        try:
            run_k = wait_management(150)
        except RuntimeError:
            run_k = None
        check('K: foreground run bootstraps on the damaged journal',
              run_k is not None, run_k, scenario='K')
        k_content = None
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            k_content = content()
            if k_content == marker_k:
                break
            time.sleep(1)
        check('K: foreground run serves the activated artifact without the zip',
              k_content == marker_k, k_content, scenario='K')
        # marker 释放在编排 readiness 确认（complete_running）之后——晚于
        # 业务首个 HTTP 应答，按预算轮询。
        marker_k_files = '1'
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            marker_k_files = execute(
                'ls "$1"/.deploy-recovery-required.json 2>/dev/null | wc -l',
                state_root_k).stdout.strip()
            if marker_k_files == '0':
                break
            time.sleep(2)
        check('K: replacement receipt released the recovery marker',
              marker_k_files == '0', marker_k_files, scenario='K')
        stop_k = post('stop')
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if try_execute('pgrep -f "[a]pp-cli run" | wc -l').stdout.strip() == '0':
                break
            time.sleep(0.5)
        krun_gone = try_execute('pgrep -f "[a]pp-cli run" | wc -l').stdout.strip()
        check('K: foreground run exits after its stop',
              stop_k.get('message') == 'Stopped' and krun_gone == '0',
              [stop_k, krun_gone], scenario='K')
        # 回到平台常驻形态：新 Start/Restart 可执行（源码态恢复）。
        supervisorctl('start', 'app-cli')
        wait_management(120)
        start('restart')
        check('K: fresh restart after recovery returns to source mode',
              content() == 'recovery-c-2', content(), scenario='K')

        # RV08：必做场景清单完整性——任何未执行/无断言的场景都不算通过。
        executed = set(report['scenarios'])
        missing = [s for s in report['required_scenarios'] if s not in executed]
        check('matrix: every required scenario executed with assertions',
              not missing, missing, scenario='matrix')
        report['success'] = True
    except (Exception, KeyboardInterrupt) as error:
        report.update(success=False, error=str(error))
        if cid:
            ps_snapshot = try_execute(
                'ps -eo pid,ppid,args | head -40', timeout=30).stdout
            report['processes'] = ps_snapshot
            report['logs'] = try_execute(
                'python3 -c \'import sys\n'
                'from pathlib import Path\n'
                'files=[Path("/tmp/proxy.log"),Path("/tmp/manual-run.log"),'
                'Path("/tmp/second-owner.log"),Path("/tmp/r4run.log"),'
                'Path("/tmp/r4run2.log"),Path("/tmp/krun.log"),'
                'Path("/app/logs/supervisord.log")]'
                '+list(Path("/home/user/logs").rglob("*.log"))\n'
                'for p in files:\n'
                ' if p.is_file():\n'
                '  print(str(p)); print(p.read_text(errors="replace")[-6000:])\n\'',
                timeout=60).stdout
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
