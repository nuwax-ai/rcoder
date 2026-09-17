#!/usr/bin/env python3
"""Independent remote build/deploy/E2E entrypoint; old make targets are untouched."""
import argparse
from contextlib import contextmanager
import datetime
import json
import os
from pathlib import Path
import re
import secrets
import select
import shlex
import signal
import subprocess
import sys
import time
import urllib.parse
import urllib.request
import uuid
from common import Config, LABEL, ROOT, VERSION, atomic_json, digest, run
from manifests import render
import snapshot
import remote_process
import registry
import test_snapshot
import build_cache
import diagnostics


@contextmanager
def lock(c):
    # Kernel releases flock even after a disconnected client; never steal stale locks.
    c.lock_token = uuid.uuid4().hex
    code = """import fcntl,sys,os,json
p='/tmp/rcoder-remote-k8s-'+sys.argv[1]+'.lock'
f=os.fdopen(os.open(p,os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600),'w')
fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)
operation=os.fdopen(os.open('/tmp/rcoder-remote-k8s-'+sys.argv[1]+'.operation',os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600),'w')
fcntl.flock(operation,fcntl.LOCK_EX|fcntl.LOCK_NB)
active=os.fdopen(os.open('/tmp/rcoder-remote-k8s-'+sys.argv[1]+'.active',os.O_CREAT|os.O_WRONLY|os.O_TRUNC|os.O_NOFOLLOW,0o600),'w')
json.dump({'pid':os.getpid(),'token':sys.argv[2]},active);active.close()
operation.close()
print('LOCKED',flush=True)
sys.stdin.read()
"""
    args = ['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', '-o', 'ServerAliveInterval=10',
            '-o', 'ServerAliveCountMax=2', c.host, shlex.join(['python3', '-c', code, c.id, c.lock_token])]
    p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    try:
        if not select.select([p.stdout], [], [], 15)[0] or p.stdout.readline().strip() != 'LOCKED':
            raise RuntimeError('Environment is locked by another command, or SSH lock unavailable')
        c.lock_process = p
        yield
    finally:
        p.stdin.close()
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            p.terminate()
            p.wait(timeout=5)


def alive(c):
    if hasattr(c, 'lock_process') and c.lock_process.poll() is not None:
        raise RuntimeError('Environment lock transport was lost; refusing further actions')


def owned(c, kind, name, optional=False):
    result = c.kube('get', kind, name, '--ignore-not-found', '-o', 'json')
    if not result.strip():
        if optional:
            return None
        raise RuntimeError(f'Missing {kind}/{name}')
    row = json.loads(result)
    if kind.lower() == 'serviceaccount' and name == 'default' and not row['metadata'].get('labels', {}).get(LABEL):
        # Kubernetes creates this account automatically inside our owned namespace.
        owned(c, 'namespace', c.ns)
        if not row.get('imagePullSecrets'):
            return row
    if row['metadata'].get('labels', {}).get(LABEL) != c.id:
        raise RuntimeError(f'Refusing foreign {kind}/{name}')
    return row


def initialize_remote(c):
    code = """import pathlib,sys,json
root=pathlib.Path(sys.argv[1]); owner=sys.argv[2]; session=sys.argv[3]
if root.is_symlink(): raise RuntimeError('Remote root is a symlink')
marker=root/'owner.json'
if root.exists() and not marker.exists(): raise RuntimeError('Refusing unowned remote directory')
root.mkdir(parents=True,exist_ok=True)
if marker.exists() and json.loads(marker.read_text())!=owner: raise RuntimeError('Remote owner mismatch')
session_marker=root/'sync-session'
if session_marker.exists() and session_marker.read_text()!=session: raise RuntimeError('Another local checkout owns this sync directory')
session_marker.write_text(session)
marker.write_text(json.dumps(owner));marker.chmod(0o600)
for n in ['live','snapshots','receipts']: (root/n).mkdir(exist_ok=True)
"""
    c.ssh(['python3', '-c', code, c.remote, c.id, c.session])


def sync_start(c):
    version = c.mut('version').strip()
    if version != VERSION:
        raise RuntimeError(f'Mutagen {VERSION} required, got {version}')
    initialize_remote(c)
    cfg = digest([c.ignores(), str(ROOT), c.remote, VERSION])
    existing = json.loads(c.mut('sync', 'list', '--label-selector', 'rcoder-session=' + c.session, '--template', '{{json .}}'))
    stamp = c.state / 'sync.json'
    if existing:
        if len(existing) != 1 or not stamp.exists() or json.loads(stamp.read_text()).get('config') != cfg:
            raise RuntimeError('Sync configuration changed; run sync-stop then sync-start')
        c.mut('sync', 'resume', c.session)
    else:
        args = ['sync', 'create', str(ROOT), c.host + ':' + c.remote + '/live', '--name', c.session,
                '--label', 'rcoder-session=' + c.session, '--mode', 'one-way-replica', '--no-global-configuration', '--ignore-vcs']
        for pattern in c.ignores():
            args += ['--ignore', pattern]
        print(c.mut(*args).strip())
        atomic_json(stamp, {'config': cfg, 'session': c.session})
    print('Sync session:', c.session)


def doctor(c):
    if c.mut('version').strip() != VERSION:
        raise RuntimeError(f'Mutagen {VERSION} required')
    print('Mutagen version OK', flush=True)
    print(c.ssh(['docker', 'buildx', 'version']).strip(), flush=True)
    c.ssh(['python3', '--version'])
    if c.ssh(['uname', '-m']).strip() != 'x86_64':
        raise RuntimeError('Remote build host must be native x86_64')
    c.ssh(['df', '-h', '/'])
    nodes = c.obj('get', 'nodes')['items']
    if not any(n['metadata']['labels'].get('kubernetes.io/arch') == 'amd64' and
               any(x['type'] == 'Ready' and x['status'] == 'True' for x in n['status']['conditions']) for n in nodes):
        raise RuntimeError('No Ready amd64 Kubernetes node')
    for sc in dict.fromkeys([c.get('STORAGE_CLASS', 'cephfs'), c.get('PG_STORAGE_CLASS', 'ceph-rbd'), c.get('USERAPP_STORAGE_CLASS', 'ceph-rbd')]):
        c.kube('get', 'storageclass', sc)
    for crd in ['gateways.gateway.networking.k8s.io', 'httproutes.gateway.networking.k8s.io', 'ciliumgatewayclassconfigs.cilium.io']:
        c.kube('get', 'crd', crd)
    owned(c, 'namespace', c.ns, optional=True)
    registry = c.get('REGISTRY', required=True)
    if not re.fullmatch(r'[a-zA-Z0-9._:/-]+', registry) or '://' in registry:
        raise ValueError('REGISTRY must be host[:port]/dedicated-project, without scheme')
    # Read-only reachability check. Authentication/push permissions are verified by build --push.
    scheme = 'http' if c.get('REGISTRY_HTTP', 'false') == 'true' else 'https'
    status = c.ssh(['curl', '--silent', '--show-error', '--output', '/dev/null', '--write-out', '%{http_code}',
                    '--max-time', '10', scheme + '://' + registry.split('/')[0] + '/v2/']).strip()
    if status not in ['200', '401']:
        raise RuntimeError('Registry /v2/ returned HTTP ' + status)
    for key in ['RCODER_BASE', 'COMPUTER_BASE', 'RUNTIME_BASE']:
        c.get(key, required=True)
    print('SSH / cluster / CRDs / storage / registry reachable. Push and node pull are checked during build/deploy.', flush=True)


def build(c, expected_manifest=None):
    doctor(c)
    sync_start(c)
    build_started = time.monotonic()
    build_id = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ-') + uuid.uuid4().hex[:8]
    freeze_started = time.monotonic()
    frozen = snapshot.create(c, build_id, expected=expected_manifest)
    receipt = {k: v for k, v in frozen.items() if k != 'manifest'}
    receipt.update(build_id=build_id, environment=c.id, namespace=c.ns, context=c.context, status='building', images={}, bases={},
                   timings={'snapshot_freeze_ms': int((time.monotonic() - freeze_started) * 1000)})
    atomic_json(c.state / 'builds' / (build_id + '.json'), receipt)
    atomic_json(c.state / 'builds' / (build_id + '-source.json'), frozen)
    builder = 'rcoder-' + c.id
    listing = c.ssh(['docker', 'buildx', 'ls', '--format', '{{.Name}}']).splitlines()
    if builder not in listing:
        config = '[worker.oci]\n  max-parallelism = ' + c.get('JOBS', '4') + '\n'
        mirrors = [m.strip() for m in c.get('DOCKER_MIRRORS', '').split(',') if m.strip()]
        if mirrors:
            config += '[registry."docker.io"]\n  mirrors = ' + json.dumps(mirrors) + '\n'
        if c.get('REGISTRY_HTTP', 'false') == 'true':
            host = c.get('REGISTRY').split('/')[0]
            config += '[registry.' + json.dumps(host) + ']\n  http = true\n'
        c.ssh(['python3', '-c', 'import pathlib,sys;pathlib.Path(sys.argv[1]).write_text(sys.stdin.read())', c.remote + '/buildkit.toml'], config)
        c.ssh(['docker', 'buildx', 'create', '--name', builder, '--driver', 'docker-container',
               '--driver-opt', 'memory=' + c.get('MEMORY', '16g'), '--driver-opt', 'cpu-quota=' + str(int(c.get('CPUS', '4')) * 100000),
               '--driver-opt', 'cpu-period=100000', '--buildkitd-config', c.remote + '/buildkit.toml'])
    bases_started = time.monotonic()
    base_args = []
    for key in ['RCODER_BASE', 'COMPUTER_BASE', 'RUNTIME_BASE']:
        ref = c.get(key, required=True)
        info = c.ssh(['docker', 'buildx', 'imagetools', 'inspect', ref], timeout=180)
        match = re.search(r'^Digest:\s*(sha256:[a-f0-9]{64})', info, re.M)
        if not match:
            raise RuntimeError('Could not resolve base digest for ' + key)
        pinned = ref.split('@')[0] + '@' + match.group(1)
        receipt['bases'][key] = pinned
        base_args += ['--build-arg', key + '=' + pinned]
    receipt['timings']['base_resolution_ms'] = int((time.monotonic() - bases_started) * 1000)
    # T06/Q03：缓存 key 锁定构建工具链实际 digest——同 tag 被 registry 更新后
    # 必须 miss。解析失败 → 本轮**完全禁用**缓存读写（不是给失败 key 加字样：
    # 稳定的失败 key 仍会命中上一轮同失败路径下写入的产物），构建继续用 tag
    # 并如实记录不可复现性。
    rust_ref = c.get('RUST_IMAGE', 'rust:1.95-trixie')
    rust_build_arg = rust_ref
    cache_key_this_round = None
    try:
        rust_info = c.ssh(['docker', 'buildx', 'imagetools', 'inspect', rust_ref], timeout=180)
        rust_match = re.search(r'^Digest:\s*(sha256:[a-f0-9]{64})', rust_info, re.M)
        if not rust_match:
            raise ValueError('no digest in imagetools output')
        receipt['rust_image'] = rust_ref.split('@')[0] + '@' + rust_match.group(1)
        # 构建也使用解析后的 digest（解析与构建之间 tag 变化不再造成
        # receipt/cache 标识与实际工具链不一致）
        rust_build_arg = receipt['rust_image']
        cache_key_this_round = build_cache.cache_key(
            receipt['source_sha256'], receipt['bases'], c, rust_image=receipt['rust_image'])
    except (RuntimeError, ValueError) as error:
        receipt['rust_image'] = 'unresolved:' + rust_ref + ':' + type(error).__name__
        receipt['cache'] = {'disabled': 'rust-image-digest-unresolved'}
        print('Warning: RUST_IMAGE digest unresolved (' + type(error).__name__ +
              '); build cache read AND write disabled for this round', flush=True)
    receipt['cache_key'] = cache_key_this_round
    cached = cache_key_this_round and build_cache.load(c, cache_key_this_round)
    if cached:
        reusable = []
        for target in build_cache.TARGETS:
            present, error_class = build_cache.artifact_matches(c, cached['images'][target])
            if present:
                reusable.append(target)
            elif error_class == 'registry-error':
                raise RuntimeError('Registry verification failed while reusing cached build ('
                                   + target + '); not treating as hit or success')
        if len(reusable) == len(build_cache.TARGETS):
            receipt['images'] = dict(cached['images'])
            receipt['status'] = 'built'
            receipt['cache'] = {'reused': True, 'reused_from': cached['build_id'],
                                'source_sha256': cached['source_sha256']}
            receipt['timings']['total_ms'] = int((time.monotonic() - build_started) * 1000)
            receipt['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            atomic_json(c.state / 'builds' / (build_id + '.json'), receipt)
            atomic_json(c.state / 'build.json', receipt)
            print('Build reused from', cached['build_id'], '(cache key ' + cache_key_this_round[:12] + ')', flush=True)
            return receipt
        print('Cache entry incomplete on registry (missing: ' +
              ', '.join(t for t in build_cache.TARGETS if t not in reusable) + '); rebuilding', flush=True)
    try:
        for target in ['rcoder', 'computer', 'runtime']:
            alive(c)
            target_started = time.monotonic()
            tag = c.get('REGISTRY').rstrip('/') + '/' + c.get('IMAGE_PREFIX', c.ns + '-') + target + ':' + build_id
            if c.get('MOUNT_BASE_LAYERS', 'false') == 'true':
                receipt.setdefault('base_layer_mounts', {})[target] = registry.mount(c, receipt['bases'][target.upper() + '_BASE'], tag.rsplit(':', 1)[0])
            metadata = c.remote + '/receipts/' + build_id + '-' + target + '.json'
            args = ['docker', 'buildx', 'build', '--builder', builder, '--platform', 'linux/amd64',
                    '--file', frozen['path'] + '/docker/remote-k8s/Dockerfile', '--target', target,
                    '--build-arg', 'CARGO_JOBS=' + c.get('JOBS', '4'), '--build-arg', 'RUST_IMAGE=' + c.get('RUST_IMAGE', 'rust:1.95-trixie'),
                    '--build-arg', 'APT_MIRROR=' + c.get('APT_MIRROR', 'http://deb.debian.org'),
                    '--build-arg', 'CARGO_MIRROR=' + c.get('CARGO_MIRROR', ''),
                    *base_args, '--tag', tag, '--metadata-file', metadata, '--push', '--provenance=false', frozen['path']]
            print('Building ' + target + ' from ' + receipt['source_sha256'][:12], flush=True)
            # Remote build has its own timeout; the environment lock is held throughout.
            budget = int(c.get('BUILD_TIMEOUT', '3600'))
            remote_process.execute(c, args, budget, c.state / 'builds' / (build_id + '-' + target + '.log'))
            meta = json.loads(c.ssh(['cat', metadata]))
            image_digest = meta.get('containerimage.digest', '')
            if not re.fullmatch('sha256:[a-f0-9]{64}', image_digest):
                raise RuntimeError('Build did not provide a valid image digest')
            receipt['images'][target] = tag.rsplit(':', 1)[0] + '@' + image_digest
            receipt['timings']['build_' + target + '_ms'] = int((time.monotonic() - target_started) * 1000)
            atomic_json(c.state / 'builds' / (build_id + '.json'), receipt)
    except BaseException as error:
        receipt.update(status='failed', error_type=type(error).__name__)
        atomic_json(c.state / 'builds' / (build_id + '.json'), receipt)
        raise
    receipt['status'] = 'built'
    if 'cache' not in receipt:
        receipt['cache'] = {'reused': False}
    receipt['timings']['total_ms'] = int((time.monotonic() - build_started) * 1000)
    receipt['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    if cache_key_this_round:
        build_cache.store(c, cache_key_this_round, receipt)
    atomic_json(c.state / 'builds' / (build_id + '.json'), receipt)
    atomic_json(c.state / 'build.json', receipt)
    print('Build receipt:', c.state / 'build.json', flush=True)
    return receipt


def read_receipt(c, name):
    row = json.loads((c.state / name).read_text())
    if row.get('environment') != c.id:
        raise RuntimeError('Receipt belongs to a different environment')
    return row


def apply(c, row):
    alive(c)
    owned(c, row['kind'], row['metadata']['name'], optional=True)
    c.kube('apply', '--server-side', '--field-manager=rcoder-remote-k8s', '-f', '-', data=json.dumps(row))


def wait_for(action, predicate, budget=180):
    end = time.monotonic() + budget
    while True:
        value = action()
        if predicate(value):
            return value
        if time.monotonic() >= end:
            raise TimeoutError('Kubernetes readiness deadline exceeded')
        time.sleep(2)


def gateway_url(c):
    svc = wait_for(lambda: c.kube('get', 'service', 'cilium-gateway-rcoder', '--ignore-not-found', '-o', 'json'), bool)
    svc = json.loads(svc)
    gateway = owned(c, 'gateway', 'rcoder')
    if not any(o['uid'] == gateway['metadata']['uid'] for o in svc['metadata'].get('ownerReferences', [])):
        raise RuntimeError('Gateway service ownership mismatch')
    port = next(p['nodePort'] for p in svc['spec']['ports'] if p['port'] == 80)
    host = urllib.parse.urlsplit(c.url).hostname
    if not host:
        raise ValueError('REMOTE_K8S_URL must specify reachable node URL')
    return 'http://' + host + ':' + str(port)


def outside_inventory(c):
    rows = c.obj('get', 'deployments,statefulsets,services,pvc,httproutes', '-A')['items']
    return {r['kind'] + '/' + r['metadata']['namespace'] + '/' + r['metadata']['name']:
            {'uid': r['metadata']['uid'], 'spec_sha256': digest(r.get('spec', {}))}
            for r in rows if r['metadata']['namespace'] != c.ns}


def outside_unchanged(c, before):
    after = outside_inventory(c)
    changed = [key for key, value in before.items() if after.get(key) != value]
    if changed:
        raise RuntimeError('Existing resources changed during validation (inspect concurrent activity): ' + ', '.join(changed))


def deploy(c):
    baseline = outside_inventory(c)
    receipt = read_receipt(c, 'build.json')
    if receipt.get('status') != 'built' or set(receipt['images']) != {'rcoder', 'computer', 'runtime'}:
        raise RuntimeError('Incomplete build receipt')
    owned(c, 'namespace', c.ns, optional=True)
    secret = owned(c, 'secret', 'postgres', optional=True) if c.kube('get', 'namespace', c.ns, '--ignore-not-found').strip() else None
    if secret:
        import base64
        password = base64.b64decode(secret['data']['password']).decode()
    else:
        password = secrets.token_hex(24)
    auth = None
    if c.get('REGISTRY_AUTH', 'none') == 'docker':
        code = "import pathlib,json,sys;d=json.loads((pathlib.Path.home()/'.docker/config.json').read_text());a=d.get('auths',{}).get(sys.argv[1]);assert a and a.get('auth'), 'No inline Docker auth for registry';print(json.dumps({'auths':{sys.argv[1]:a}}))"
        auth = json.loads(c.ssh(['python3', '-c', code, c.get('REGISTRY').split('/')[0]]))
    resources = render(c, receipt['images'], password, auth)
    # Preflight every name before performing any apply; never adopt existing foreign resources.
    for row in resources:
        if row['kind'] == 'Namespace' or 'namespace' not in row['metadata'] or c.kube('get', 'namespace', c.ns, '--ignore-not-found').strip():
            owned(c, row['kind'], row['metadata']['name'], optional=True)
    for row in resources:
        apply(c, row)
    c.kube('rollout', 'status', 'statefulset/postgres', '--timeout=180s', timeout=210)
    c.kube('rollout', 'status', 'deployment/rcoder', '--timeout=300s', timeout=330)
    dep = owned(c, 'deployment', 'rcoder')
    receipt.update(outside_baseline=baseline, deployment_uid=dep['metadata']['uid'], generation=dep['metadata']['generation'],
                   url=c.url, gateway_url=gateway_url(c), config_sha256=digest([r for r in resources if r['kind'] != 'Secret']))
    atomic_json(c.state / 'deployment.json', receipt)
    smoke(c, receipt)
    receipt['pods'] = pod_identities(c, receipt)
    outside_unchanged(c, baseline)
    atomic_json(c.state / 'deployment.json', receipt)
    print('Deployed:', receipt['url'], 'Gateway:', receipt['gateway_url'], flush=True)



def pod_identities(c, receipt):
    pods = c.obj('get', 'pods', '-l', 'app=rcoder,' + LABEL + '=' + c.id)['items']
    rows = []
    for pod in pods:
        if pod['metadata'].get('deletionTimestamp'):
            continue
        statuses = pod.get('status', {}).get('containerStatuses', [])
        container = next((s for s in statuses if s['name'] == 'rcoder'), {})
        if not container.get('ready') or not container.get('imageID'):
            raise RuntimeError('RCoder pod has no Ready runtime image identity')
        if pod['spec']['containers'][0]['image'] != receipt['images']['rcoder']:
            raise RuntimeError('RCoder pod image differs from deployment receipt')
        rows.append({'name': pod['metadata']['name'], 'uid': pod['metadata']['uid'],
                     'imageID': container['imageID'], 'containerID': container.get('containerID')})
    if len(rows) != 2:
        raise RuntimeError('Expected exactly two active RCoder pods')
    return rows

def identity(c, receipt):
    dep = owned(c, 'deployment', 'rcoder')
    if dep['metadata']['uid'] != receipt['deployment_uid'] or dep['metadata']['generation'] != receipt['generation']:
        raise RuntimeError('Deployment changed since recorded deployment')
    if dep['spec']['template']['spec']['containers'][0]['image'] != receipt['images']['rcoder']:
        raise RuntimeError('Deployment image mismatch')
    if dep.get('status', {}).get('readyReplicas', 0) != 2 or dep['spec']['replicas'] != 2:
        raise RuntimeError('Expected two Ready RCoder replicas')
    return dep


def http_health(url):
    with urllib.request.urlopen(url.rstrip('/') + '/health', timeout=10) as response:
        body = json.load(response)
        if response.status != 200 or body.get('code') != '0000':
            raise RuntimeError('Health endpoint did not return RCoder success')


def smoke(c, receipt):
    identity(c, receipt)
    for claim in ['workspace', 'computer-workspace', 'cephfs-root', 'postgres-data']:
        if owned(c, 'pvc', claim)['status']['phase'] != 'Bound':
            raise RuntimeError('Unbound PVC: ' + claim)
    http_health(receipt['url'])


CHAT_CASE_PATTERN = re.compile(r'^(pass|fail): k8s_lb::([a-zA-Z0-9_]+)$', re.M)


def registered_chat_cases(root=ROOT):
    """已注册 chat 场景目录。retest 传冻结快照根：活动目录的 catalog 变化
    不得影响对历史冻结输入的校验（T05）。"""
    catalog = json.loads((Path(root) / 'tests-e2e/tools/suite_cases.json').read_text())
    return list(catalog.get('k8s_lb', []))


def run_chat_suite(c, receipt, context, case=''):
    """chat 套件经严格启动器执行；case 精确筛选时记录部分覆盖标记。

    启动器非零退出（存在失败场景）时仍解析已完成场景的逐项结果并放入
    context['cases']——失败报告必须携带失败用例集合（可重跑），无结果可解析
    的基础设施失败保持空集，不伪造用例。解析完成后原样上抛失败。
    """
    env = dict(os.environ)
    env.update({k: v for k, v in c.values.items() if k.startswith('LLM_')})
    snapshot_root = context['snapshot_path']
    env.update(PYTHONDONTWRITEBYTECODE='1',
               CARGO_TARGET_DIR=str(c.state / 'e2e-target'),
               RCODER_URL=receipt['url'], TEST_K8S_SSH=c.host, TEST_K8S_NS=c.ns,
               TEST_K8S_CONTEXT=c.context, TEST_K8S_ENVIRONMENT_ID=c.id,
               LB_ENTRY_HOSTS=c.get('ENTRY_HOSTS', urllib.parse.urlsplit(receipt['url']).hostname or ''),
               LB_NODEPORT=str(c.nodeport),
               E2E_SOURCE_ROOT=str(snapshot_root),
               E2E_INPUT_MANIFEST=str(context['snapshot_manifest_path']),
               E2E_ORIGIN_HEAD=context['origin_head'],
               E2E_RUN_ROOT=str(context['report_dir'] / 'e2e-reports'))
    for key in ['LLM_API_KEY', 'LLM_MODEL', 'LLM_BASE_URL']:
        if not env.get(key):
            raise ValueError('Missing ' + key + ' (real LLM is required; refusing to mock or skip)')
    launcher = snapshot_root / 'tests-e2e/tools/run.py'
    args = ['python3', str(launcher), '--group', 'k8s', '--suite', 'k8s_lb', '--filter', case, '--ignored', '--remote-k8s']
    failure = None
    try:
        output = run(args, timeout=2400, env=env, log=context['report_dir'] / 'chat.log', guard=lambda: alive(c))
    except RuntimeError as error:
        failure = error  # 场景失败：先解析已完成结果，再上抛（T04）
        output = (context['report_dir'] / 'chat.log').read_text()
    (context['report_dir'] / 'chat.log').write_text(output)
    # T04：优先消费严格启动器落盘的结构化 summary（含基础设施失败/中断的
    # aborted 记录）；stdout 正则仅兜底（无报告可解析时绝不伪造用例）
    cases = _structured_chat_cases(context)
    if cases is None:
        cases = [{'name': name, 'verdict': verdict}
                 for verdict, name in CHAT_CASE_PATTERN.findall(output)]
    context['cases'] = cases
    if failure is not None:
        raise failure
    return cases


def _structured_chat_cases(context):
    """解析本轮 e2e-reports 下严格启动器的结构化 summary（T04）。

    report_dir 每次测试独立新建，目录内即本轮启动器输出；结构化结果包含
    场景失败与 aborted（无完成进程结果）明细，比 stdout 正则可信。
    """
    root = context['report_dir'] / 'e2e-reports'
    if not root.is_dir():
        return None
    rows = []
    for run_dir in sorted(root.iterdir()):
        summary = run_dir / 'summary.json'
        if not summary.exists():
            continue
        try:
            data = json.loads(summary.read_text())
        except ValueError:
            continue
        for result in data.get('results', []):
            if result.get('suite') == 'k8s_lb' and result.get('test'):
                rows.append({'name': result['test'],
                             'verdict': result.get('verdict'),
                             'errors': result.get('errors', []),
                             'run_id': data.get('run_id')})
    return rows or None


def run_userapp_suite(c, receipt, context):
    env = dict(os.environ)
    env.update({k: v for k, v in c.values.items() if k.startswith('LLM_')})
    snapshot_root = context['snapshot_path']
    env.update(PYTHONDONTWRITEBYTECODE='1',
               CARGO_TARGET_DIR=str(c.state / 'e2e-target'),
               RCODER_URL=receipt['url'], TEST_K8S_SSH=c.host, TEST_K8S_NS=c.ns,
               TEST_K8S_CONTEXT=c.context, TEST_K8S_ENVIRONMENT_ID=c.id,
               E2E_SOURCE_ROOT=str(snapshot_root),
               E2E_INPUT_MANIFEST=str(context['snapshot_manifest_path']),
               E2E_ORIGIN_HEAD=context['origin_head'],
               E2E_RUN_ROOT=str(context['report_dir'] / 'e2e-reports'))
    launcher = snapshot_root / 'tests-e2e/tools/k8s_userapp.py'
    args = ['python3', str(launcher), '--ssh', c.host,
            '--namespace', c.ns, '--deployment', 'rcoder', '--context', c.context,
            '--environment-id', c.id, '--url', receipt['url'], '--proxy-url', receipt['gateway_url'],
            '--internal-url', 'http://rcoder.' + c.ns + '.svc:8086']
    output = run(args, timeout=2400, env=env, log=context['report_dir'] / 'userapp.log', guard=lambda: alive(c))
    (context['report_dir'] / 'userapp.log').write_text(output)


def prepare_test_snapshot(c, manifest_override=None):
    """冻结本轮测试输入（manifest_override：verify 轮的同轮清单）。"""
    if manifest_override is not None:
        frozen = test_snapshot.freeze_from_manifest(c, manifest_override)
    else:
        frozen = test_snapshot.freeze(c)
    origin_head = ''
    try:
        origin_head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True, timeout=60).strip()
    except (subprocess.CalledProcessError, OSError):
        origin_head = 'unknown'
    frozen['origin_head'] = origin_head
    return frozen


def execute_suites(c, receipt, context, suite, case=''):
    """已冻结快照上执行选定套件；**所有收束路径**（成功/失败/取消）都执行
    末尾快照校验（Q05）——失败报告必须能区分业务失败与输入篡改；校验错误
    不掩盖原始错误，两者一并上抛。

    运行后校验失败 → 异常 → 调用方 verdict=fail：被篡改输入上的结果不得记 pass。
    """
    test_snapshot.verify(context['snapshot_record'])
    started = time.monotonic()
    try:
        if suite in ['gateway', 'all']:
            for kind, name in [('gateway', 'rcoder'), ('httproute', 'rcoder')]:
                row = owned(c, kind, name)
                conditions = row.get('status', {}).get('conditions', []) if kind == 'gateway' else [x for p in row.get('status', {}).get('parents', []) for x in p.get('conditions', [])]
                required = ['Accepted', 'ResolvedRefs'] if kind == 'httproute' else ['Accepted']
                if any(not any(x['type'] == condition and x['status'] == 'True' and
                               x.get('observedGeneration') == row['metadata']['generation'] for x in conditions) for condition in required):
                    raise RuntimeError(kind + ' does not have current successful conditions')
            http_health(receipt['gateway_url'])
        if suite in ['userapp', 'all']:
            run_userapp_suite(c, receipt, context)
        if suite in ['chat', 'all']:
            context['cases'] = run_chat_suite(c, receipt, context, case=case)
    except BaseException as suite_error:
        # Q05：失败/取消也校验快照——输入被篡改的失败与业务失败必须可区分；
        # 校验错误链在原始错误之后，不互相掩盖
        try:
            test_snapshot.verify(context['snapshot_record'])
        except BaseException as verify_error:
            raise RuntimeError(f'suite error: {suite_error!r}; '
                               f'post-run frozen snapshot verification ALSO failed: {verify_error}') from suite_error
        raise
    elapsed = int((time.monotonic() - started) * 1000)
    test_snapshot.verify(context['snapshot_record'])
    return elapsed


class TestRunError(RuntimeError):
    """tests() 整轮失败（V01）：携带本次 run 的显式 test_id 与完整结果，
    调用方（retest）不得以 mtime 扫描猜测报告，也不得用 case 结果覆盖整轮失败。"""

    def __init__(self, test_id, result):
        super().__init__(str(result.get('error') or 'test run failed'))
        self.test_id = test_id
        self.result = result


def tests(c, suite, case='', frozen_snapshot=None, receipt_override=None):
    receipt = receipt_override or read_receipt(c, 'deployment.json')
    test_id = uuid.uuid4().hex
    report = c.state / 'tests' / test_id
    report.mkdir(parents=True)
    result = {**receipt, 'suite': suite, 'verdict': 'aborted', 'test_id': test_id}
    if case:
        result['case_filter'] = case
        result['partial_coverage'] = True
    atomic_json(report / 'summary.json', result)
    context = None
    try:
        alive(c)
        freeze_started = time.monotonic()
        snapshot_record = frozen_snapshot or prepare_test_snapshot(c)
        context = {'snapshot_record': snapshot_record,
                   'snapshot_path': Path(snapshot_record['path']) / 'source',
                   'snapshot_manifest_path': Path(snapshot_record['path']) / 'inputs.json',
                   'origin_head': snapshot_record.get('origin_head', 'unknown'),
                   'report_dir': report}
        result['snapshot_id'] = snapshot_record['snapshot_id']
        result['origin_head'] = context['origin_head']
        result['test_source_sha256'] = snapshot_record['source_sha256']
        result['test_source'] = {'snapshot_id': snapshot_record['snapshot_id'],
                                 'source_sha256': snapshot_record['source_sha256']}
        result['server_source_sha256'] = receipt.get('source_sha256')
        result['sources_match'] = result['test_source_sha256'] == receipt.get('source_sha256')
        result['timings'] = {'freeze_ms': int((time.monotonic() - freeze_started) * 1000)}
        if not result['sources_match']:
            # test 模式验证已部署版本：两套身份都展示，不假装同轮（verify 同轮
            # 由 verify() 冻结一次保证）
            print('Note: test source differs from deployed source (test-mode accepts; '
                  'use verify for same-round acceptance)', flush=True)
        smoke(c, receipt)
        result['timings']['suites_ms'] = execute_suites(c, receipt, context, suite, case=case)
        alive(c)
        identity(c, receipt)
        result['pods_after'] = pod_identities(c, receipt)
        outside_unchanged(c, receipt['outside_baseline'])
        # 活动目录变化仅记提示（快照身份不受影响）
        try:
            live_now = digest(snapshot.manifest())
            result['live_source_changed_during_test'] = live_now != snapshot_record['source_sha256']
        except Exception:  # noqa: BLE001 - 提示性观察，不影响判定
            result['live_source_changed_during_test'] = 'unknown'
        result['verdict'] = 'pass'
    except BaseException as exc:
        result.update(verdict='fail', error=type(exc).__name__ + ': ' + str(exc))
        try:
            logs(c, report)
        except Exception as diagnostic_error:
            result['diagnostic_error'] = type(diagnostic_error).__name__
        raise TestRunError(test_id, result) from exc
    finally:
        # 失败也保留已解析的场景结果（T04）：失败用例集合是 retest 与归因的输入
        if context is not None and context.get('cases') is not None:
            result['cases'] = context['cases']
        atomic_json(report / 'summary.json', result)
        print('Test report:', report, flush=True)
    return test_id, result


def run_retest_case(c, record, receipt, name):
    """单用例重跑（V01）：以 tests() 整轮结果为准——整轮失败不得被 case
    pass 覆盖；run 身份显式返回/异常携带，禁止 mtime 扫描。"""
    outcome = {'name': name, 'verdict': 'fail'}
    try:
        case_test_id, case_result = tests(c, 'chat', case=name,
                                          frozen_snapshot=record,
                                          receipt_override=receipt)
    except KeyboardInterrupt:
        raise
    except TestRunError as run_error:
        case_test_id = run_error.test_id
        case_result = run_error.result
        outcome['error'] = str(case_result.get('error') or run_error)[:300]
    except BaseException as exc:
        outcome['error'] = type(exc).__name__ + ': ' + str(exc)[:300]
        return outcome, None, {}
    run_passed = case_result.get('verdict') == 'pass'
    case_passed = any(row.get('name') == name and row.get('verdict') == 'pass'
                      for row in (case_result.get('cases') or []))
    if run_passed and case_passed:
        outcome['verdict'] = 'pass'
    elif not outcome.get('error'):
        # case 未过或整轮未过但无异常：如实记录失败原因维度
        outcome['error'] = ('case failed' if not case_passed else '') + \
                           ('; run verdict=' + str(case_result.get('verdict')) if not run_passed else '')
    outcome['test_id'] = case_test_id
    return outcome, case_test_id, case_result


def retest_failed(c, parent_id):
    """失败重跑：复用父报告的冻结测试输入；部署身份变化即拒绝。

    仅处理有可靠场景级结果的失败（chat cases）；构建/环境/aborted 失败没有
    可信用例集合，明确要求修复后重新运行对应入口。
    """
    if not re.fullmatch(r'[0-9a-f]{32}', parent_id or ''):
        raise ValueError('RUN must be a 32-hex test id')
    parent_dir = c.state / 'tests' / parent_id
    parent_summary = parent_dir / 'summary.json'
    if not parent_summary.exists():
        raise RuntimeError('Parent test report not found: ' + str(parent_dir))
    parent = json.loads(parent_summary.read_text())
    if parent.get('environment') != c.id:
        raise RuntimeError('Parent report belongs to a different environment')
    receipt = {k: v for k, v in parent.items()
               if k in {'images', 'bases', 'build_id', 'environment', 'namespace', 'context',
                        'source_sha256', 'deployment_uid', 'generation', 'url', 'gateway_url',
                        'outside_baseline', 'config_sha256'}}
    identity(c, receipt)  # 部署被替换/换代 → 拒绝旧结果重跑
    failed = [row['name'] for row in parent.get('cases', []) if row['verdict'] == 'fail']
    if parent.get('verdict') != 'fail' or not failed:
        raise RuntimeError('Parent has no reliable failed case results '
                           '(infrastructure or aborted failures require rerunning the original entrypoint)')
    snapshot_dir = c.state / 'test-snapshots' / parent.get('snapshot_id', '')
    record_path = snapshot_dir / 'snapshot.json'
    if not parent.get('snapshot_id') or not record_path.exists() or not (snapshot_dir / 'inputs.json').exists():
        raise RuntimeError('Parent frozen test snapshot is missing or untrustworthy; rerun the original entrypoint')
    record = json.loads(record_path.read_text())
    if record.get('status') != 'sealed' or record.get('environment') != c.id:
        raise RuntimeError('Parent frozen test snapshot is not sealed for this environment')
    record['path'] = str(snapshot_dir)
    test_snapshot.verify(record)  # 快照被篡改 → 拒绝
    # Q04：父报告源码身份与冻结记录绑定核验——重跑输入必须与父报告同源
    if record.get('source_sha256') != parent.get('test_source_sha256'):
        raise RuntimeError('Parent report source identity does not match its frozen snapshot')
    # 场景注册校验用冻结快照内的 catalog（T05）：活动目录后续增删场景
    # 不改变父报告冻结输入的合法性边界
    frozen_root = Path(record['path']) / 'source'
    registered = registered_chat_cases(frozen_root)
    unknown = [name for name in failed if name not in registered]
    if unknown:
        raise RuntimeError('Parent failed cases are not registered scenarios: ' + ', '.join(unknown))
    print('Retesting failed cases from parent', parent_id, ':', ', '.join(failed), flush=True)
    # Q04/V01：重跑复用普通 tests() 的完整收束路径（同轮冻结、部署身份/外部
    # 资源复核、所有收束路径的快照校验、结构化失败报告）——不绕过任何保护。
    # V01：本次 run 的 test_id 与完整结果由 tests() 显式返回/异常携带——
    # 禁止 mtime 扫描猜测；**整轮 verdict=fail 不得被 case pass 覆盖**；
    # receipt 绑定父报告部署身份（tests() 内 smoke/identity 对照该身份，
    # 运行期间部署被替换 → 整轮失败）。
    outcomes = []
    for name in failed:
        outcome, case_test_id, case_result = run_retest_case(c, record, receipt, name)
        outcomes.append(outcome)
        if case_test_id:
            summary = {'parent_test_id': parent_id, 'retest': True, 'case': name,
                       'verdict': outcome['verdict'], 'snapshot_id': record['snapshot_id'],
                       'test_source_sha256': record['source_sha256'],
                       'server_source_sha256': receipt.get('source_sha256'),
                       'run_verdict': case_result.get('verdict'),
                       'case_rows': [row for row in (case_result.get('cases') or [])
                                     if row.get('name') == name],
                       'error': outcome.get('error', ''),
                       'report': str(c.state / 'tests' / case_test_id)}
            atomic_json((c.state / 'tests' / case_test_id) / 'retest-summary.json', summary)
    still_failed = [row['name'] for row in outcomes if row['verdict'] != 'pass']
    summary_path = c.state / 'tests' / (parent_id + '-retest-' + uuid.uuid4().hex[:8])
    atomic_json(summary_path / 'summary.json', {'parent_test_id': parent_id, 'retest': True,
                                                'cases': outcomes, 'still_failed': still_failed,
                                                'partial_coverage': True,
                                                'note': 'retest covers only previously failed cases; '
                                                        'full-suite pass requires a fresh complete run'})
    print('Retest report:', summary_path, flush=True)
    if still_failed:
        raise RuntimeError('Retested cases still failing: ' + ', '.join(still_failed))


def logs(c, destination=None):
    destination = destination or c.state / 'logs' / uuid.uuid4().hex
    destination.mkdir(parents=True, exist_ok=True)
    owned(c, 'namespace', c.ns)
    redactions = [v for k, v in c.values.items() if re.search(r'API_KEY|PASSWORD|TOKEN|SECRET', k) and len(v) > 5]
    try:
        import base64
        redactions.append(base64.b64decode(owned(c, 'secret', 'postgres')['data']['password']).decode())
    except (RuntimeError, KeyError):
        pass
    def scrub(text):
        for value in redactions:
            text = text.replace(value, '<redacted>')
        return text
    # Q06：前置查询逐项独立保护——namespace/pods/events 任一失败不得阻止
    # 其余诊断项收集（错误记录进 collection-errors）
    try:
        owned(c, 'namespace', c.ns)
    except RuntimeError as error:
        collection_errors.append({'item': 'namespace ownership', 'error_class': 'collect-failed',
                                  'detail': str(error)[-300:]})
    try:
        pods = c.obj('get', 'pods')['items']
        rows = [{'name': p['metadata']['name'], 'uid': p['metadata']['uid'], 'status': p.get('status')} for p in pods]
        atomic_json(destination / 'pods.json', rows)
    except Exception as error:  # noqa: BLE001 - 单项失败不阻止其余收集
        collection_errors.append({'item': 'pods inventory', 'error_class': 'collect-failed',
                                  'detail': str(error)[-300:]})
        pods = []
    try:
        (destination / 'events.txt').write_text(scrub(c.kube('get', 'events', '--sort-by=.lastTimestamp')))
    except Exception as error:  # noqa: BLE001 - 同上
        collection_errors.append({'item': 'events', 'error_class': 'collect-failed',
                                  'detail': str(error)[-300:]})
    collection_errors = []
    for pod in pods:
        name = pod['metadata']['name']
        for container in pod['spec']['containers']:
            # 逐项采集：单项失败记录 error_class 并继续，绝不吞掉也不覆盖原始错误
            try:
                data = c.kube('logs', name, '-c', container['name'], '--tail=200')
                (destination / (name + '-' + container['name'] + '.log')).write_text(scrub(data))
            except RuntimeError as error:
                collection_errors.append({'item': name + '/' + container['name'] + ' logs',
                                          'error_class': 'collect-failed', 'detail': str(error)[-300:]})
            if pod['metadata'].get('labels', {}).get('app') == 'rcoder' and container['name'] == 'rcoder':
                try:
                    data = c.kube('exec', name, '-c', 'rcoder', '--', 'sh', '-c', 'tail -n 200 /app/logs/rcoder.* 2>/dev/null')
                    (destination / (name + '-files.log')).write_text(scrub(data))
                except RuntimeError as error:
                    collection_errors.append({'item': name + '/rcoder file-logs',
                                              'error_class': 'collect-failed', 'detail': str(error)[-300:]})
    if collection_errors:
        atomic_json(destination / 'collection-errors.json', collection_errors)
    print('Diagnostics:', destination, flush=True)


def down(c):
    owned(c, 'namespace', c.ns)
    before = {p['metadata']['name']: p['metadata']['uid'] for p in c.obj('get', 'pvc')['items']}
    baseline = outside_inventory(c)
    # Stop the controller before scaling dynamically created workloads.
    owned(c, 'deployment', 'rcoder')
    c.kube('scale', 'deployment/rcoder', '--replicas=0')
    wait_for(lambda: c.obj('get', 'pods', '-l', 'app=rcoder')['items'], lambda rows: not rows)
    for kind in ['deployment', 'statefulset']:
        for row in c.obj('get', kind)['items']:
            alive(c)
            c.kube('scale', kind + '/' + row['metadata']['name'], '--replicas=0')
    wait_for(lambda: c.obj('get', 'pods')['items'], lambda pods: not any(p.get('status', {}).get('phase') not in ['Succeeded', 'Failed'] for p in pods))
    after = {p['metadata']['name']: p['metadata']['uid'] for p in c.obj('get', 'pvc')['items']}
    if before != after:
        raise RuntimeError('PVC identities changed while stopping workloads')
    outside_unchanged(c, baseline)
    atomic_json(c.state / 'down.json', {'environment': c.id, 'pvc_before': before, 'pvc_after': after, 'verdict': 'pass'})
    print('Workloads scaled to zero. Namespace, routes, Secrets and all PVCs retained.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['doctor', 'sync-start', 'sync-status', 'sync-stop', 'build', 'deploy',
                                           'test', 'verify', 'logs', 'down', 'status', 'check', 'retest-failed'])
    # 参数经环境变量安全传递（make 侧不插值进 shell；见 make/remote-k8s.mk）。
    parser.add_argument('--suite', choices=['smoke', 'userapp', 'chat', 'gateway', 'all'],
                        default=os.environ.get('REMOTE_K8S_SUITE', 'smoke'))
    parser.add_argument('--case', default=os.environ.get('REMOTE_K8S_CASE', ''))
    parser.add_argument('--run', default=os.environ.get('REMOTE_K8S_RUN', ''))
    args = parser.parse_args()
    if args.case:
        # CASE 仅支持 chat 套件（UserApp 是有依赖的完整生命周期链，未建独立
        # fixture 前不允许任意截断）；精确匹配已注册场景，空集/未知提前拒绝
        if args.suite != 'chat':
            parser.error('--case/--filter only supports SUITE=chat (userapp keeps the full lifecycle chain)')
        if not re.fullmatch(r'[a-zA-Z0-9_]+', args.case):
            parser.error('CASE must be a single registered scenario name')
        registered = registered_chat_cases()
        if args.case not in registered:
            parser.error('unknown chat case: ' + args.case + ' (registered: ' + ', '.join(registered) + ')')
    if args.run and args.action != 'retest-failed':
        parser.error('--run only applies to retest-failed')
    c = Config()
    c.state.mkdir(parents=True, exist_ok=True)
    c.state.chmod(0o700)
    if args.action == 'doctor':
        doctor(c)
        return
    if args.action == 'sync-status':
        print(c.mut('sync', 'list', '--label-selector', 'rcoder-session=' + c.session))
        return
    if args.action == 'status':
        diagnostics.status(c)
        return
    if args.action == 'check':
        diagnostics.check(c)
        return
    with lock(c):
        try:
            if args.action == 'sync-start': sync_start(c)
            elif args.action == 'sync-stop': c.mut('sync', 'terminate', '--label-selector', 'rcoder-session=' + c.session)
            elif args.action == 'build': build(c)
            elif args.action == 'deploy': deploy(c)
            elif args.action == 'test': tests(c, args.suite, case=args.case)
            elif args.action == 'logs': logs(c)
            elif args.action == 'down': down(c)
            elif args.action == 'retest-failed': retest_failed(c, args.run)
            elif args.action == 'verify':
                # 同轮输入：冻结一次，构建与测试消费同一清单（R1）
                frozen = snapshot.manifest()
                prepared = prepare_test_snapshot(c, manifest_override=frozen)
                build(c, expected_manifest=frozen)
                deploy(c)
                tests(c, args.suite, case=args.case, frozen_snapshot=prepared)
        except BaseException:
            if args.action in ['deploy', 'verify']:
                try:
                    logs(c)
                except Exception as diagnostic_error:
                    print('Diagnostics unavailable: ' + type(diagnostic_error).__name__, file=sys.stderr)
            raise



if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    try:
        main()
    except (Exception, KeyboardInterrupt) as error:
        print('remote-k8s failed: ' + str(error), file=sys.stderr)
        sys.exit(1)
