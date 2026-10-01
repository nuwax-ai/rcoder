#!/usr/bin/env python3
"""app-cli runtime recovery v2 fault matrix (plan §11.2 scenarios).

One container chain exercises the unified in-process owner against real
faults: concurrent callers (A), owner TERM/SIGKILL (B), pkill of every
app-cli process (C), repeated/interleaved stop semantics (G), a frozen
(SIGSTOP) owner with no second-owner takeover (J), an app-11 style stale
Running + RecoveryRequired fixture upgraded in place (I), same-container
restart (E) and same-volume container replacement (D).

Real supervisord, real builds, real HTTP content assertions. No LLM, no
RCoder control plane. Leaves the workspace volume intact for inspection;
removes only the containers it created.
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
    app = 'rcv' + uuid.uuid4().hex[:10]
    name = 'rcoder-app-cli-recovery-' + app
    volume = name + '-workspace'
    workspace = '/home/user/' + app
    state_root = '/home/user/.app-cli-state/' + app
    report = {'app_id': app, 'volume': volume, 'checks': [], 'containers': [],
              'scenarios': {}, 'binaries': {
                  str(p.resolve()): hashlib.sha256(p.read_bytes()).hexdigest()
                  for p in (args.app_cli, args.file_server_proxy)}}
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

    def new_container(instance=None):
        nonlocal cid
        image_id = docker('image', 'inspect', '--format', '{{.Id}}',
                          args.image).stdout.strip()
        domain = {'authority': 'app-cli-recovery-v2', 'volume': volume,
                  'instance_source_env': 'RCODER_PHYSICAL_POD_UID', 'instance': ''}
        instance = instance or str(uuid.uuid4())
        cid = docker('run', '-d', '--name', name,
                     '--mount', f'type=volume,src={volume},dst=/home/user,volume-nocopy',
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
                     '--entrypoint', 'sleep', image_id, 'infinity').stdout.strip()
        report['containers'].append({'id': cid, 'domain_instance': instance})
        docker('cp', str(args.app_cli.resolve()), f'{cid}:/usr/local/bin/app-cli')
        docker('cp', str(args.file_server_proxy.resolve()),
               f'{cid}:/usr/local/bin/file-server-proxy')
        # 固定 serve 程序对齐真实 builder（killed owners 由 supervisord 重启；
        # 干净退出 0 不复活）。supervisord/proxy 为 exec 派生，restart 后须重拉。
        launch_services()
        return instance

    def launch_services():
        write({'/tmp/recovery-supervisor.conf': f'''[unix_http_server]
file=/var/run/supervisor.sock
[supervisord]
nodaemon=true
logfile=/tmp/recovery-supervisor.log
pidfile=/tmp/recovery-supervisor.pid
[rpcinterface:supervisor]
supervisor.rpcinterface_factory=supervisor.rpcinterface:make_main_rpcinterface
[program:app-cli]
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
[include]
files=/etc/supervisor/conf.d/50-app-services.conf
'''})
        # docker restart 复用容器层：上一生命周期的 socket/pid 残留会阻塞
        # supervisord 绑定（unix socket 文件存在即 bind 失败）——先清理。
        execute('rm -f /var/run/supervisor.sock /tmp/recovery-supervisor.pid')
        docker('exec', '-d', cid, 'supervisord', '-n',
               '-c', '/tmp/recovery-supervisor.conf')
        execute('mkdir -p "$1" /home/user/logs', workspace)
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec file-server-proxy --embed --policy all_rust --port 60000 '
               '> /tmp/proxy.log 2>&1')
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            probe = try_execute('test -S /var/run/supervisor.sock && '
                                'curl -fsS --max-time 2 http://127.0.0.1:60000/health')
            if probe.returncode == 0:
                return
            time.sleep(0.3)
        raise RuntimeError('file-server or supervisord did not initialize')


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
[health]
readiness_path = "/"
[proxy]
path = "/"
strip_prefix = false
'''
        devrun = '\n[devrun]\ncommand = ["python3", "main.py"]\n'
        main_py = ('import os\n'
                   'from http.server import BaseHTTPRequestHandler,HTTPServer\n'
                   'class H(BaseHTTPRequestHandler):\n def do_GET(self):\n'
                   '  self.send_response(200)\n  self.end_headers()\n'
                   f'  self.wfile.write(b"{marker}")\n'
                   'HTTPServer(("0.0.0.0",int(os.environ["PORT"])),H).serve_forever()\n')
        return {workspace + '/workspace.manifest.toml':
                    'schema_version=1\n[workspace]\nname="recovery"\n',
                workspace + '/web/project.manifest.toml': manifest + devrun,
                workspace + '/web/main.py': main_py,
                workspace + '/sentinel': marker}

    try:
        docker('volume', 'create', volume)
        # ── A：并发调用方收敛到同一 owner ─────────────────────────────
        new_container()
        # 固定 serve 程序随容器启动；与真实平台一致先等管理面就绪
        #（builder pod 探针等价）再发起业务请求。
        wait_management(90)
        write(app_files('recovery-a-1'))
        # 与 supervisord 常驻 serve 并发：agent 形态的手动 run 与平台 start
        # 同时发起，只能有一个 owner/一个管理监听器。
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli run --workspace "$1" > /tmp/manual-run.log 2>&1',
               '--', workspace)
        start('start')
        discovery = execute(
            'find /home/user/.app-cli-state -name supervisor.json').stdout.strip()
        check('A: exactly one supervisor discovery',
              len(discovery.splitlines()) == 1, discovery, scenario='A')
        first_identity = identity()
        procs = execute(
            'ps -eo pid,args | grep "[a]pp-cli" | grep -v grep').stdout
        listeners = execute(
            'pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('A: exactly one serve owner process', listeners == '1',
              procs, scenario='A')
        check('A: business HTTP serves content', content() == 'recovery-a-1',
              content(), scenario='A')
        manual = try_execute('cat /tmp/manual-run.log').stdout
        # plan §8：并发请求按身份/revision 裁决——转交、明确拒绝（Busy/
        # Conflict/前置契约失败）都证明没有第二编排；禁止的是静默第二
        # 绑定。不变量直接断言：无第二监听冲突、单 discovery（已查）。
        bound_conflict = ('address already in use' in manual.lower()
                          or ('bind' in manual.lower() and 'failed' in manual.lower()))
        ownership_enforced = ('dispatching to the running owner' in manual
                              or 'already has an owner' in manual
                              or 'owner' in manual.lower())
        check('A: concurrent run did not bind a second orchestrator',
              ownership_enforced and not bound_conflict, manual[-500:],
              scenario='A')

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

        # ── G：恢复后的停止/启动不排队、不误启 ───────────────────────
        post('stop')
        check('G: stopped business stays stopped', content() is None,
              None, scenario='G')
        start('restart')
        check('G: start after stop re-launches business',
              content() == 'recovery-c-2', content(), scenario='G')

        # ── R4：run 首个 owner + 另一进程 HTTP Stop/Restart 消费 ──────
        post('stop')
        execute(
            'p="$(pgrep -f "[a]pp-cli serve" | head -1)"; '
            '[ -n "$p" ] && kill -9 "$p" || true', check=False)
        time.sleep(2)
        # 真实 run 成为第一个 owner（经 supervisord 外的手动 exec）。
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli run --workspace "$1" --log-dir /home/user/logs '
               '--admin-addr 0.0.0.0:3010 >/tmp/r4run.log 2>&1', '--', workspace)
        try:
            run_owner = wait_management(120)
        except RuntimeError:
            run_owner = None
        check('R4: real run bootstraps the first owner',
              run_owner is not None, run_owner, scenario='R4')
        report.setdefault('r4_diag', {})
        report['r4_diag']['ps'] = try_execute(
            'ps -eo pid,args | grep "[a]pp-cli" | grep -v grep',
            timeout=30).stdout
        report['r4_diag']['r4run'] = try_execute(
            'cat /tmp/r4run.log 2>/dev/null | tail -c 1500', timeout=30).stdout
        report['r4_diag']['snapshot'] = try_execute(
            'find /home/user -name supervisor.json -exec cat {} \\;',
            timeout=30).stdout[:800]
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
            report['r4_fail']['outlog'] = try_execute(
                'tail -c 2000 /home/user/logs/app-cli.out.log 2>/dev/null',
                timeout=30).stdout
            report['r4_fail']['task'] = try_execute(
                'curl -fsS --max-time 5 "http://127.0.0.1:60000/api/v1/userapp/tasks'
                '?app_id=' + app + '" 2>/dev/null || true', timeout=30).stdout[:800]
            report['r4_fail']['ops'] = try_execute(
                'find /home/user -path "*operations*" -name "*.json" | head -5 | '
                'xargs -r -n1 sh -c "echo --- $0; head -c 400 $0"', timeout=60).stdout[:2000]
            raise
        check('R4: platform-submitted start consumed by run owner',
              content() == 'recovery-c-2', content(), scenario='R4')
        # HTTP Stop（同一管理面）也被消费；run 形态前台退出码语义由
        # 会话 restart_on_exit=false 保证（进程退出=业务终态）。
        stop_r4 = post('stop')
        check('R4: platform-submitted stop consumed by run owner',
              stop_r4.get('message') == 'Stopped' and content() is None,
              stop_r4, scenario='R4')
        # Restart again: run owner may have exited after stop (foreground
        # exit semantics) — a fresh run bootstrap must take over cleanly.
        start('restart')
        check('R4: restart after run-owner exit recovers',
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

        # ── I：app-11 升级 fixture（脱敏重建，非现场快照）────────────
        # 形态：RecoveryRequired discovery + 前容器域章的 Running 旧代次
        #（无退出回执）并存。新版启动必须零手改自动恢复且保留旧记录。
        post('stop')
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

        # ── R3：坏 journal 期间的精确 Stop + 管理可用 ──────────────────
        # 损坏业务部署 journal → 杀 serve 由 supervisord 重启（新 owner 的
        # 业务初始化在坏 journal 上失败进入降级驻留）：窗口内 Stop 必须
        # 受理、identity 可查询、3010 不消失；修复后业务恢复。
        state_root_dir = execute(
            'find /home/user -maxdepth 5 -name supervisor.json -printf "%h\n" '
            '| head -1').stdout.strip()
        check('R3: located state root', bool(state_root_dir), state_root_dir,
              scenario='R3')
        write({state_root_dir + '/.deploy-operation.json': '{damaged-journal'})
        execute(
            'p="$(pgrep -f "[a]pp-cli serve" | head -1)"; '
            '[ -n "$p" ] && kill -9 "$p" || true', check=False)
        # supervisord 重启 serve；新会话业务初始化失败但 owner 驻留。
        deadline = time.monotonic() + 120
        degraded = False
        while time.monotonic() < deadline:
            id_probe = try_execute(
                'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/runtime/identity')
            if id_probe.returncode == 0:
                degraded = True
                break
            time.sleep(1)
        check('R3: management survives damaged-journal business failure',
              degraded, id_probe.stdout[-150:] if degraded else 'timeout',
              scenario='R3')
        stop_r3 = post('stop')
        check('R3: stop accepted during degraded business state',
              stop_r3.get('message') == 'Stopped', stop_r3, scenario='R3')
        id_probe2 = try_execute(
            'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/runtime/identity')
        check('R3: identity queryable after degraded stop',
              id_probe2.returncode == 0, id_probe2.stdout[-120:], scenario='R3')
        # 修复 journal（合法空记录覆盖，不删除原文件的诊断需要已满足），
        # 业务恢复。
        write({state_root_dir + '/.deploy-operation.json':
                   '{\"deploy_replays\": {}}'})
        start('restart')
        check('R3: business recovers after journal repaired',
              content() == 'recovery-c-2', content(), scenario='R3')

        # ── E：同容器重启（docker restart，容器 ID 不变）────────────
        # 测试装置保真：exec 派生的 supervisord/proxy 在 pid1 快速退出时可能
        # 跨 restart 存活（真实平台 supervisord 是容器入口、随容器干净重启）。
        # 先清残留守护再重拉，保证单一管理进程的世界。
        docker('restart', cid)
        try_execute('pkill -9 -f "[s]upervisord" || true; '
                    'pkill -9 -f "[f]ile-server-proxy" || true; '
                    'pkill -9 -f "[a]pp-cli serve" || true', timeout=30)
        launch_services()
        wait_management(120)
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
        wait_management(90)
        start('restart')
        check('D: same-volume replacement recovers and serves',
              content() == 'recovery-c-2', content(), scenario='D')
        check('D: workspace data retained across replacement',
              execute('cat "$1/sentinel"', workspace).stdout == 'recovery-a-1',
              None, scenario='D')

        # ── H：迁移结果未知 + 进程范围已空 ────────────────────────────
        # 预置未确认迁移回执（identity 匹配当前 release，completed=false）：
        # Stop/管理必须可用；依赖迁移的启动给出具体原因；不自动重跑、不伪造。
        post('stop')
        migrate_ws = workspace
        receipt_dir = state_root + '/migration-receipts'
        write({receipt_dir + '/unconfirmed.json':
                   json.dumps({'identity': 'H' * 64, 'completed': False})})
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
        # 两种合法结局：启动被拒并给出迁移具体原因；或启动成功但**没有重跑
        # 迁移**（回执仍 completed=false——伪造成功/静默重跑都算失败）。
        refused = isinstance(restart_h, str) and (
            'migration' in restart_h.lower()
            or 'reconciliation' in restart_h.lower())
        started_ok = not isinstance(restart_h, str)
        receipt_now = json.loads(execute(
            'cat "$1"', receipt_dir + '/unconfirmed.json').stdout)
        check('H: unconfirmed migration blocks or explains dependent start',
              refused or started_ok, str(restart_h)[:200], scenario='H')
        check('H: migration outcome not fabricated nor silently rerun',
              receipt_now.get('completed') is False,
              receipt_now, scenario='H')

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
                'Path("/tmp/second-owner.log"),Path("/tmp/recovery-supervisor.log"),'
                'Path("/tmp/r4run.log")]'
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
