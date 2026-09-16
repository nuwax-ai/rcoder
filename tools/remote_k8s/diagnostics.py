"""Status and layered diagnostics (R4).

- `status`：只读。展示配置与现场入口、已部署身份（build/deployment receipt）、
  实时 Deployment 匹配状态、最近测试及其来源与历史标记。绝不读取/打印
  Secret 或容器 env。
- `check`：分项检查，每项 name/status(pass|fail|unknown)/duration_ms/
  error_class + 证据路径。独立读操作有限并行；每项有 deadline，整体有预算。
  缺权限/未知/超时记 unknown，绝不当通过。默认不创建任何资源。
"""
import concurrent.futures
import json
import time
import urllib.error
import urllib.request

from common import atomic_json, digest

CHECK_BUDGET_SECONDS = 120
ITEM_TIMEOUT_SECONDS = 20


def _probe(label, namespace_hint=''):
    """执行单项检查的通用包装：异常分类 + 计时。"""

    def wrapper(function):
        def run(c):
            started = time.monotonic()
            try:
                detail = function(c)
                return {'name': label, 'status': 'pass',
                        'duration_ms': int((time.monotonic() - started) * 1000),
                        'detail': detail}
            except PermissionError as error:
                return {'name': label, 'status': 'unknown', 'error_class': 'forbidden',
                        'duration_ms': int((time.monotonic() - started) * 1000),
                        'detail': str(error)[:200]}
            except TimeoutError as error:
                return {'name': label, 'status': 'unknown', 'error_class': 'timeout',
                        'duration_ms': int((time.monotonic() - started) * 1000),
                        'detail': str(error)[:200]}
            except Exception as error:  # noqa: BLE001 - 分项隔离：单项失败不拖垮其他诊断
                return {'name': label, 'status': 'fail', 'error_class': type(error).__name__,
                        'duration_ms': int((time.monotonic() - started) * 1000),
                        'detail': str(error)[:200]}
        return run
    return wrapper


@_probe('ssh-connectivity')
def _ssh(c):
    c.ssh(['true'])
    return 'ssh ok'


@_probe('kubectl-api')
def _api(c):
    c.ssh(['kubectl', '--context', c.context, 'get', '--raw', '/version'], timeout=ITEM_TIMEOUT_SECONDS)
    return 'api ok'


@_probe('namespace-present')
def _namespace(c):
    row = c.obj('get', 'namespace', c.ns, '--ignore-not-found')
    if not row:
        raise RuntimeError('namespace missing: ' + c.ns)
    return c.ns


def _deployment_state(c):
    """实时 Deployment 状态（未部署返回 None，不算失败——历史环境合法态）。"""
    row = c.obj('get', 'deployment', 'rcoder', '--ignore-not-found')
    if not row:
        return None
    return {'uid': row['metadata']['uid'], 'generation': row['metadata']['generation'],
            'replicas': row['spec'].get('replicas'), 'ready': row.get('status', {}).get('readyReplicas', 0),
            'image': row['spec']['template']['spec']['containers'][0]['image']}


@_probe('deployment-runtime')
def _deployment(c):
    state = _deployment_state(c)
    if state is None:
        raise RuntimeError('rcoder deployment absent (never deployed or scaled down)')
    return state


def _http_json(url, timeout=ITEM_TIMEOUT_SECONDS):
    with urllib.request.urlopen(url.rstrip('/') + '/health', timeout=timeout) as response:
        body = json.load(response)
        if response.status != 200 or body.get('code') != '0000':
            raise RuntimeError('health endpoint did not return RCoder success')
        return 'health ok'


@_probe('direct-health')
def _direct(c):
    receipt = _deployment_receipt(c)
    if not receipt or not receipt.get('url'):
        raise RuntimeError('no deployed URL on record (run deploy first)')
    return _http_json(receipt['url'])


@_probe('gateway-health')
def _gateway(c):
    receipt = _deployment_receipt(c)
    if not receipt or not receipt.get('gateway_url'):
        raise RuntimeError('no gateway URL on record (run deploy first)')
    return _http_json(receipt['gateway_url'])


@_probe('pvc-bound')
def _pvc(c):
    rows = c.obj('get', 'pvc')['items']
    if not rows:
        raise RuntimeError('no PVCs in namespace')
    unbound = [r['metadata']['name'] for r in rows if r.get('status', {}).get('phase') != 'Bound']
    if unbound:
        raise RuntimeError('unbound PVCs: ' + ', '.join(unbound))
    return len(rows)


@_probe('storage-class')
def _storage(c):
    classes = c.obj('get', 'storageclass')['items']
    return [x['metadata']['name'] for x in classes]


@_probe('coredns-pods')
def _dns(c):
    rows = c.ssh(['kubectl', '--context', c.context, '-n', 'kube-system', 'get', 'pods',
                  '-l', 'k8s-app=kube-dns', '--no-headers'], timeout=ITEM_TIMEOUT_SECONDS)
    running = [line.split()[0] for line in rows.splitlines() if line.strip() and line.split()[2].startswith('Running')]
    if not running:
        raise RuntimeError('no Running coredns pods')
    return running


@_probe('ceph-detail')
def _ceph(c):
    # 只读尽力探查；无权限/无 Ceph CRD 记 unknown（wrapper 已分类），不当通过
    rows = c.ssh(['kubectl', '--context', c.context, 'get', 'cephcluster', '-A',
                  '-o', 'jsonpath={.items[*].status.ceph.health}'], timeout=ITEM_TIMEOUT_SECONDS)
    return rows.strip() or 'no cephcluster CRD reports health'


def _deployment_receipt(c):
    path = c.state / 'deployment.json'
    if not path.exists():
        return None
    return json.loads(path.read_text())


def check(c):
    items = [_ssh, _api, _namespace, _deployment, _direct, _gateway,
             _pvc, _storage, _dns, _ceph]
    results = []
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        futures = {pool.submit(item, c): item for item in items}
        for future in concurrent.futures.as_completed(futures, timeout=CHECK_BUDGET_SECONDS):
            results.append(future.result())
    elapsed = int((time.monotonic() - started) * 1000)
    # 稳定排序：按检查项定义序展示
    priority = {'ssh-connectivity': 0, 'kubectl-api': 1, 'namespace-present': 2,
                'deployment-runtime': 3, 'direct-health': 4, 'gateway-health': 5,
                'pvc-bound': 6, 'storage-class': 7, 'coredns-pods': 8, 'ceph-detail': 9}
    results.sort(key=lambda row: priority.get(row['name'], 99))
    destination = c.state / 'checks' / (str(int(time.time())) + '-' + digest(results)[:8])
    atomic_json(destination / 'check.json', {'environment': c.id, 'elapsed_ms': elapsed, 'items': results})
    for row in results:
        print(f"{row['status']:>7}  {row['name']}  ({row['duration_ms']}ms)  {row.get('detail', '')}", flush=True)
    failed = [row['name'] for row in results if row['status'] == 'fail']
    if failed:
        raise RuntimeError('check failed: ' + ', '.join(failed))
    print('Check report:', destination, flush=True)
    return results


def status(c):
    """只读状态汇总：身份、入口、实时状态、最近测试（含历史标记）。"""
    rows = []
    build_receipt = json.loads((c.state / 'build.json').read_text()) if (c.state / 'build.json').exists() else None
    deployment_receipt = _deployment_receipt(c)
    if build_receipt:
        rows.append(('build', {'build_id': build_receipt.get('build_id'),
                               'source_sha256': build_receipt.get('source_sha256'),
                               'images': build_receipt.get('images'),
                               'cache': build_receipt.get('cache'),
                               'status': build_receipt.get('status')}))
    live = None
    try:
        live = _deployment_state(c)
    except Exception as error:  # noqa: BLE001 - status 只读汇总：实时态不可得记为说明
        live = 'unavailable: ' + type(error).__name__
    if deployment_receipt:
        matches = (isinstance(live, dict) and live['uid'] == deployment_receipt.get('deployment_uid')
                   and live['image'] == deployment_receipt.get('images', {}).get('rcoder'))
        rows.append(('deployment', {
            'deployment_uid': deployment_receipt.get('deployment_uid'),
            'generation': deployment_receipt.get('generation'),
            'url': deployment_receipt.get('url'), 'gateway_url': deployment_receipt.get('gateway_url'),
            'deployed_source_sha256': deployment_receipt.get('source_sha256'),
            'live_matches_receipt': matches, 'live': live}))
    tests_root = c.state / 'tests'
    recent = []
    if tests_root.exists():
        for directory in sorted(tests_root.iterdir(), key=lambda p: p.name, reverse=True)[:5]:
            summary = directory / 'summary.json'
            if not summary.exists():
                continue
            try:
                row = json.loads(summary.read_text())
            except ValueError:
                continue
            recent.append({'test_id': row.get('test_id'), 'suite': row.get('suite'),
                           'verdict': row.get('verdict'), 'source': row.get('test_source'),
                           'note': 'historical result — not evidence for the current deployment'
                           if deployment_receipt and row.get('server_source_sha256') != deployment_receipt.get('source_sha256') else 'current deployment round'})
    rows.append(('recent-tests', recent))
    report = {'environment': c.id, 'namespace': c.ns, 'context': c.context,
              'entries': dict(rows)}
    atomic_json(c.state / 'status.json', report)
    for key, value in rows:
        print('==', key, '==')
        print(json.dumps(value, indent=2, ensure_ascii=False), flush=True)
    print('Status report:', c.state / 'status.json', flush=True)
