#!/usr/bin/env python3
"""app-cli 的个人 K8s 存储锁与容器重启实验。

cephfs-lock 只跑两个 Ready 节点共同挂载同一 RWX PVC 的真实 owner 竞争；
all 另跑同节点 RBD 竞争、正常跨节点挂载交接与同 Pod 容器重启。
必须显式指定节点、不可变镜像与构建回执。仅删除捕获 UID 的测试 Pod，
使用 UID/resourceVersion 前置条件；namespace 和所有 PVC 永久保留。
"""

import argparse
import json
import os
import re
import shlex
import subprocess
import time
import uuid
from pathlib import Path
from urllib.parse import quote

from prod_readiness_contract import source_identity


SOURCE_STEP = 'source matches frozen build receipt and stays unchanged'


CEPHFS_STEPS = (
    'T0-S-fs: cephfs RWX PVC bound',
    'T0-S-fs: owner acquires on cephfs',
    'T0-S-fs: independent Ready nodes share the exact PVC',
    'T0-S-fs: cross-client contender converges',
    'T0-S-fs: holder identity stays live and unchanged',
    'T0-S-fs: contender has no independent management listener',
    'T0-S-fs: cross-client SIGKILL handover',
    'T0-S-fs: sentinel survives cross-client handover',
)
RBD_STEPS = (
    'setup: RBD PVC bound',
    'T0-S: first owner acquires on RBD volume',
    'T0-S: same-node competitor converges to the single owner',
    'T0-S: still exactly one discovery after contention',
    'T0-S: holder identity stays live and unchanged',
    'T0-S: contender has no independent management listener',
    'T0-S: SIGKILL releases lock; successor acquires',
    'T0-S: sentinel data preserved across owner change',
    'T0-S: role swap keeps single owner semantics',
    'T0-S: role swap holder stays live and contender has no API',
    'T0-S: cross-node detach/attach handover',
    'T0-S: data intact after cross-node handover',
)
RESTART_STEPS = (
    'K8s-E: owner running with platform binding',
    'K8s-E: downward API binding files present',
    'K8s-E: container restarted in place',
    'K8s-E: Pod UID unchanged',
    'K8s-E: container identity changed',
    'K8s-E: pid1 epoch changed',
    'K8s-E: new owner reconciles and serves after restart',
    'K8s-E: PVC sentinel preserved',
)
REQUIRED_STEPS = {
    'cephfs-lock': (SOURCE_STEP,) + CEPHFS_STEPS,
    'all': (SOURCE_STEP,) + RBD_STEPS + CEPHFS_STEPS + RESTART_STEPS,
}


def validate_required_steps(report, scenario):
    """缺失/重复必测步骤和任何失败都不能被总 success 掩盖。"""
    checks = report.get('checks', [])
    failures = [c['name'] for c in checks if not c.get('ok')]
    names = [c['name'] for c in checks]
    missing = [name for name in REQUIRED_STEPS[scenario]
               if names.count(name) != 1]
    if missing or failures:
        raise RuntimeError(f'incomplete acceptance: missing/duplicate={missing}; '
                           f'failed={failures}')


def select_nodes(document, selected):
    if len(selected) != 2 or len(set(selected)) != 2:
        raise ValueError('exactly two distinct --node arguments are required')
    nodes = {item['metadata']['name']: item for item in document['items']}
    for name in selected:
        info = nodes.get(name)
        if info is None:
            raise ValueError(f'node not found: {name}')
        if info.get('spec', {}).get('unschedulable'):
            raise ValueError(f'node is unschedulable: {name}')
        ready = any(c.get('type') == 'Ready' and c.get('status') == 'True'
                    for c in info.get('status', {}).get('conditions', []))
        if not ready:
            raise ValueError(f'node is not Ready: {name}')
    return list(selected)


def require_build_source(value, current):
    """使用严格启动器的冻结清单口径，不接受仅具备合法 hash 形状的旧回执。"""
    if (not re.fullmatch(r'[0-9a-f]{40}', current.get('origin_head', ''))
            or not re.fullmatch(r'[0-9a-f]{64}', current.get('worktree_sha256', ''))):
        raise ValueError('current frozen source identity is unavailable')
    if (value.get('source_commit') != current['origin_head']
            or value.get('source_digest') != current['worktree_sha256']):
        raise ValueError('build receipt does not match current frozen source')


def load_build_receipt(path, image, current):
    value = json.loads(Path(path).read_text(encoding='utf-8'))
    if value.get('schema_version') != 1 or value.get('image') != image:
        raise ValueError('build receipt schema/image does not match this run')
    if not re.fullmatch(r'.+@sha256:[0-9a-f]{64}', image):
        raise ValueError('--image must be an immutable @sha256 digest reference')
    for key, width in (('app_cli_sha256', 64), ('source_commit', 40),
                       ('source_digest', 64)):
        if not re.fullmatch(r'[0-9a-f]{' + str(width) + r'}', value.get(key, '')):
            raise ValueError(f'build receipt has no valid {key}')
    require_build_source(value, current)
    return value


class ClusterExperiment:
    def __init__(self, args, runner=subprocess.run, sleep=time.sleep,
                 clock=time.monotonic):
        self.args = args
        self.runner = runner
        self.sleep = sleep
        self.clock = clock
        self.run_id = 'rcv' + uuid.uuid4().hex[:8]
        self.namespace = f'rcoder-e2e-soddy-lock-{self.run_id}'
        self.pods = {}
        self.volumes = {}
        self.unconfirmed_creations = []
        self.namespace_uid = None
        self.receipt = None
        self.source_dir = Path(args.source_dir).resolve()
        self.source_before = None
        self.report = {
            'run_id': self.run_id, 'namespace': self.namespace,
            'context': args.context, 'scenario': args.scenario,
            'image': args.image, 'storage_class': args.storage_class,
            'checks': [], 'scenarios': {}, 'pods': {}, 'retained_volumes': [],
            'deleted_pods': [],
            'evidence_level': 'real K8s only when all required checks pass',
        }

    def kubectl(self, *argv, check=True, timeout=180, input_text=None,
                cluster=False):
        cmd = ['kubectl', '--context', self.args.context]
        if not cluster:
            cmd += ['-n', self.namespace]
        return self.runner(cmd + list(argv), capture_output=True, text=True,
                           check=check, timeout=timeout, input=input_text)

    def check(self, label, passed, detail=None, scenario=None):
        self.report['checks'].append(
            {'name': label, 'ok': bool(passed), 'detail': detail})
        if scenario:
            self.report['scenarios'].setdefault(scenario, []).append(label)
        print(label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise RuntimeError(label)

    def create(self, manifest):
        manifest['metadata'].setdefault('labels', {})[
            'rcoder.io/lock-test-run'] = self.run_id
        attempt = {'kind': manifest['kind'], 'name': manifest['metadata']['name']}
        self.unconfirmed_creations.append(attempt)
        result = self.kubectl('create', '-f', '-', '-o', 'json',
                              input_text=json.dumps(manifest))
        info = json.loads(result.stdout)
        meta = info['metadata']
        if (info.get('kind') != attempt['kind'] or meta.get('name') != attempt['name']
                or meta.get('namespace') != self.namespace or not meta.get('uid')
                or not meta.get('resourceVersion')
                or meta.get('labels', {}).get('rcoder.io/lock-test-run')
                != self.run_id):
            raise RuntimeError('created resource identity is unavailable')
        if info['kind'] == 'Pod':
            self.pods[meta['name']] = {'uid': meta['uid'],
                                      'resource_version': meta['resourceVersion']}
            self.report['pods'][meta['name']] = {
                'uid': meta['uid'], 'creation_resource_version': meta['resourceVersion']}
        elif info['kind'] == 'PersistentVolumeClaim':
            self.volumes[meta['name']] = {'uid': meta['uid']}
        self.unconfirmed_creations.remove(attempt)
        return info

    def get(self, kind, name, absent_ok=False):
        argv = ['get', kind, name, '-o', 'json']
        if absent_ok:
            argv.append('--ignore-not-found=true')
        out = self.kubectl(*argv)
        if out.stdout.strip():
            return json.loads(out.stdout)
        if absent_ok:
            return None
        raise RuntimeError(f'{kind} {name} observation returned no object')

    def delete_pod(self, name, budget=180):
        captured = self.pods.get(name)
        if captured is None:
            raise RuntimeError(f'cannot delete uncaptured Pod: {name}')
        current = self.get('pod', name, absent_ok=True)
        if current is None:
            self.report['deleted_pods'].append({'name': name, 'uid': captured['uid'],
                                               'already_absent': True})
            self.pods.pop(name)
            return
        meta = current['metadata']
        if (meta['uid'] != captured['uid']
                or meta.get('labels', {}).get('rcoder.io/lock-test-run')
                != self.run_id):
            raise RuntimeError(f'Pod identity changed; retained without deletion: {name}')
        # kubectl 的普通 delete 不带版本保护；raw DELETE 支持 stdin 请求体。
        # 仅使用当前已核验 UID 的最新 RV，不用 --force、selector 或 namespace 删除。
        options = {'apiVersion': 'v1', 'kind': 'DeleteOptions',
                   'preconditions': {'uid': captured['uid'],
                                     'resourceVersion': meta['resourceVersion']}}
        path = f'/api/v1/namespaces/{quote(self.namespace)}/pods/{quote(name)}'
        self.kubectl('delete', '--raw', path, '-f', '-',
                     input_text=json.dumps(options))
        deadline = self.clock() + budget
        while self.clock() < deadline:
            observed = self.get('pod', name, absent_ok=True)
            if observed is None:
                self.report['deleted_pods'].append({'name': name, 'uid': captured['uid'],
                    'delete_resource_version': meta['resourceVersion'], 'confirmed_absent': True})
                self.pods.pop(name)
                return
            if observed['metadata']['uid'] != captured['uid']:
                raise RuntimeError(f'Pod replaced during deletion; retained: {name}')
            self.sleep(1)
        raise RuntimeError(f'captured Pod deletion unconfirmed: {name}')

    def cleanup(self):
        errors = [f'{item["kind"]} {item["name"]}: creation response unavailable; '
                  'no captured UID, retained without deletion'
                  for item in self.unconfirmed_creations]
        self.report['unconfirmed_creations'] = list(self.unconfirmed_creations)
        for name in list(self.pods):
            try:
                self.delete_pod(name)
            except Exception as error:
                errors.append(f'Pod {name}: {self.error_text(error)}')
        retained = []
        for name, captured in self.volumes.items():
            try:
                info = self.get('pvc', name)
                if info['metadata']['uid'] != captured['uid']:
                    raise RuntimeError('PVC UID changed; preservation not confirmed')
                if info['metadata'].get('deletionTimestamp'):
                    raise RuntimeError('PVC is being deleted; preservation not confirmed')
                retained.append(dict(captured, name=name,
                    phase=info.get('status', {}).get('phase'),
                    volume_name=info.get('spec', {}).get('volumeName')))
            except Exception as error:
                errors.append(f'PVC {name}: {self.error_text(error)}')
                retained.append({'name': name, 'uid': captured['uid'],
                                 'error': self.error_text(error)})
        self.report['retained_volumes'] = retained
        if self.namespace_uid:
            try:
                out = self.kubectl('get', 'namespace', self.namespace, '-o',
                                   'json', cluster=True)
                info = json.loads(out.stdout)
                if info['metadata']['uid'] != self.namespace_uid:
                    raise RuntimeError('namespace UID changed')
                if (info['metadata'].get('deletionTimestamp')
                        or info.get('status', {}).get('phase') != 'Active'):
                    raise RuntimeError('namespace is not Active; preservation not confirmed')
                self.report['retained_namespace'] = {
                    'name': self.namespace, 'uid': self.namespace_uid,
                    'phase': info.get('status', {}).get('phase')}
            except Exception as error:
                errors.append(f'namespace: {self.error_text(error)}')
        self.report['cleanup_errors'] = errors
        self.report['cleanup_ok'] = not errors
        return not errors

    @staticmethod
    def error_text(error):
        if isinstance(error, subprocess.CalledProcessError):
            return f'kubectl exit {error.returncode}: {(error.stderr or "")[-1000:]}'
        return str(error)

    def exec_in(self, pod, command, check=True, timeout=120):
        return self.kubectl('exec', pod, '--', 'sh', '-ec', command,
                            check=check, timeout=timeout)

    def wait_ready(self, name, expected_node, pvc, budget=300):
        deadline = self.clock() + budget
        while self.clock() < deadline:
            info = self.get('pod', name)
            if info['metadata']['uid'] != self.pods[name]['uid']:
                raise RuntimeError(f'Pod identity changed during startup: {name}')
            statuses = info.get('status', {}).get('containerStatuses', [])
            if statuses and all(c.get('ready') for c in statuses):
                if info['spec'].get('nodeName') != expected_node:
                    raise RuntimeError(f'Pod scheduled on unexpected node: {name}')
                names = [v.get('persistentVolumeClaim', {}).get('claimName')
                         for v in info['spec']['volumes']]
                if pvc not in names:
                    raise RuntimeError(f'Pod does not mount expected PVC: {name}')
                volume = self.get('pvc', pvc)
                if volume['metadata']['uid'] != self.volumes[pvc]['uid']:
                    raise RuntimeError(f'PVC identity changed: {pvc}')
                image_id = statuses[0].get('imageID', '')
                digest = self.args.image.split('@')[1]
                if not image_id.endswith(digest):
                    raise RuntimeError(f'Pod imageID differs from receipt: {image_id}')
                binary = self.exec_in(name, 'sha256sum "$(command -v app-cli)"').stdout
                if binary.split()[0] != self.receipt['app_cli_sha256']:
                    raise RuntimeError(f'Pod app-cli binary differs from receipt: {name}')
                self.report['pods'][name] = {
                    'uid': info['metadata']['uid'], 'node_name': expected_node,
                    'container_id': statuses[0].get('containerID'),
                    'image_id': image_id, 'pvc': pvc,
                    'pvc_uid': volume['metadata']['uid'],
                    'app_cli_sha256': binary.split()[0]}
                return info
            self.sleep(2)
        raise RuntimeError(f'{name} not ready')

    def pod_spec(self, name, node, command, pvc, binding=False):
        volumes = [{'name': 'shared',
                    'persistentVolumeClaim': {'claimName': pvc}}]
        mounts = [{'name': 'shared', 'mountPath': '/shared'}]
        if binding:
            volumes.append({'name': 'rcoder-platform-binding', 'downwardAPI': {
                'items': [{'path': 'RCODER_PHYSICAL_POD_UID', 'fieldRef': {
                    'fieldPath': 'metadata.uid'}},
                    {'path': 'execution-domain', 'fieldRef': {'fieldPath':
                     "metadata.annotations['rcoder.io/execution-domain']"}}]}})
            mounts.append({'name': 'rcoder-platform-binding',
                           'mountPath': '/etc/rcoder/platform', 'readOnly': True})
        return {
            'apiVersion': 'v1', 'kind': 'Pod',
            'metadata': {'name': name, 'annotations': {
                'rcoder.io/execution-domain': json.dumps({
                    'authority': 'k8s-lock-test', 'volume': f'pvc:{pvc}',
                    'instance': '', 'instance_source_env': 'RCODER_PHYSICAL_POD_UID'})}},
            'spec': {'nodeName': node, 'restartPolicy': 'Always',
                     'securityContext': {'fsGroup': 1000},
                     'volumes': volumes, 'containers': [{
                         'name': 'main', 'image': self.args.image,
                         'imagePullPolicy': 'IfNotPresent', 'command': command,
                         'env': [{'name': 'APP_CLI_STATE_ROOT', 'value': '/shared/state'}],
                         'volumeMounts': mounts}]},
        }

    def serve_owner(self, name, node, pvc, binding=False):
        # PID、输出文件按 Pod 分离；不能从共享文件误杀另一个 Pod 的同号 PID。
        return self.pod_spec(name, node, ['/bin/sh', '-ec',
            'mkdir -p /shared/ws /shared/logs; '
            'app-cli serve --workspace /shared/ws --log-dir /shared/logs '
            '--admin-addr 0.0.0.0:3010 > /shared/logs/' + name + '.out 2>&1 & '
            'echo $! > /tmp/serve.pid; wait $(cat /tmp/serve.pid) || true; sleep 3600'],
            pvc, binding)

    def observe_owner(self, pod):
        script = '''import json,os,urllib.request
pid=int(open('/tmp/serve.pid').read())
stat=open('/proc/%d/stat'%pid).read().rsplit(')',1)[1].split()
if stat[0]=='Z': raise RuntimeError('captured owner is a zombie')
cmd=open('/proc/%d/cmdline'%pid,'rb').read().replace(b'\\0',b' ').decode()
if 'app-cli serve ' not in cmd: raise RuntimeError('captured PID is not serve')
socket_inodes=[]
for fd in os.listdir('/proc/%d/fd'%pid):
 try:
  target=os.readlink('/proc/%d/fd/%s'%(pid,fd))
 except FileNotFoundError: continue
 if target.startswith('socket:['): socket_inodes.append(target[8:-1])
listener_inodes=[]
for path in ('/proc/net/tcp','/proc/net/tcp6'):
 for line in open(path).read().splitlines()[1:]:
  fields=line.split()
  if fields[3]=='0A' and int(fields[1].split(':')[1],16)==3010:
   listener_inodes.append(fields[9])
if len(listener_inodes)!=1 or listener_inodes[0] not in socket_inodes:
 raise RuntimeError('captured owner does not own the management listener')
with urllib.request.urlopen('http://127.0.0.1:3010/v1/runtime/identity',timeout=3) as r:
 identity=json.load(r)['data']
discovery=json.load(open('/shared/state/supervisor.json'))
print(json.dumps({'pid':pid,'starttime':stat[19],'cmdline':cmd,
 'instance':discovery['instance'],'runtime_instance':identity['runtime_instance_id'],
 'management_socket':listener_inodes[0]}))'''
        result = self.exec_in(pod, 'python3 -c ' + shlex.quote(script))
        return json.loads(result.stdout)

    def wait_owner(self, pod, previous=None, budget=120):
        deadline = self.clock() + budget
        error = None
        while self.clock() < deadline:
            try:
                observed = self.observe_owner(pod)
                if previous is None or observed['instance'] != previous['instance']:
                    self.report['pods'][pod]['owner'] = observed
                    return observed
            except Exception as caught:
                error = self.error_text(caught)
            self.sleep(2)
        raise RuntimeError(f'owner API not ready in {pod}: {error}')

    def kill_exact_owner(self, pod, captured):
        script = '''import json,os,select,signal
expected=json.loads(%r)
pid=expected['pid']; fd=os.pidfd_open(pid)
stat=open('/proc/%%d/stat'%%pid).read().rsplit(')',1)[1].split()
cmd=open('/proc/%%d/cmdline'%%pid,'rb').read().replace(b'\\0',b' ').decode()
if stat[19]!=expected['starttime'] or cmd!=expected['cmdline']:
 raise RuntimeError('captured owner PID identity changed; refusing signal')
signal.pidfd_send_signal(fd,signal.SIGKILL)
poller=select.poll(); poller.register(fd,select.POLLIN)
if not poller.poll(10000): raise RuntimeError('captured owner physical exit unconfirmed')
os.close(fd)''' % json.dumps(captured)
        self.exec_in(pod, 'python3 -c ' + shlex.quote(script))

    def no_management_listener(self, pod):
        script = '''import json
ports=[]
for path in ('/proc/net/tcp','/proc/net/tcp6'):
 for line in open(path).read().splitlines()[1:]:
  fields=line.split()
  if fields[3]=='0A' and int(fields[1].split(':')[1],16) in (3010,3999):
   ports.append(int(fields[1].split(':')[1],16))
print(json.dumps(ports))'''
        return json.loads(self.exec_in(pod, 'python3 -c ' + shlex.quote(script)).stdout)

    def compete(self, name, node, pvc):
        command = ['/bin/sh', '-ec',
            'mkdir -p /shared/logs; rc=0; timeout 90 app-cli serve '
            '--workspace /shared/ws --log-dir /shared/logs '
            '--admin-addr 0.0.0.0:3999 > /shared/logs/' + name + '.out 2>&1 || rc=$?; '
            'echo "second-rc=$rc"; sleep 3600']
        self.create(self.pod_spec(name, node, command, pvc))
        self.wait_ready(name, node, pvc)
        deadline = self.clock() + 120
        while self.clock() < deadline:
            listeners = self.no_management_listener(name)
            if listeners:
                raise RuntimeError(f'competing owner opened an independent management listener: '
                                   f'{name} ports={listeners}')
            logs = self.kubectl('logs', name).stdout
            if 'second-rc=' in logs:
                detail = self.exec_in(name, 'tail -c 900 /shared/logs/' + name + '.out').stdout
                return logs[-900:] + '\n' + detail, self.no_management_listener(name)
            self.sleep(2)
        raise RuntimeError(f'competing serve did not finish: {name}')

    @staticmethod
    def converged(detail):
        return ('owner lock is held' in detail
                or 'refusing to start a competing orchestrator' in detail
                or 'dispatch' in detail.lower() or 'second-rc=0' in detail)

    def create_volume(self, name, storage, mode, label, scenario):
        self.create({'apiVersion': 'v1', 'kind': 'PersistentVolumeClaim',
            'metadata': {'name': name}, 'spec': {'accessModes': [mode],
                'resources': {'requests': {'storage': '1Gi'}},
                'storageClassName': storage}})
        deadline = self.clock() + 180
        while self.clock() < deadline:
            info = self.get('pvc', name)
            if info['metadata']['uid'] != self.volumes[name]['uid']:
                raise RuntimeError('PVC identity changed before binding')
            if info.get('status', {}).get('phase') == 'Bound':
                volume_name = info['spec'].get('volumeName')
                if not volume_name:
                    raise RuntimeError('Bound PVC has no PV identity')
                pv = json.loads(self.kubectl('get', 'pv', volume_name, '-o',
                                            'json', cluster=True).stdout)
                claim = pv['spec'].get('claimRef', {})
                if (claim.get('uid') != self.volumes[name]['uid']
                        or claim.get('namespace') != self.namespace
                        or claim.get('name') != name):
                    raise RuntimeError('PV does not bind the exact captured PVC')
                driver = pv['spec'].get('csi', {}).get('driver', '')
                if mode == 'ReadWriteMany' and 'cephfs' not in driver.lower():
                    raise RuntimeError('CephFS scenario requires an actual CephFS CSI PV')
                if mode == 'ReadWriteOnce' and 'rbd' not in driver.lower():
                    raise RuntimeError('RBD scenario requires an actual RBD CSI PV')
                self.volumes[name].update(pv_name=volume_name,
                    pv_uid=pv['metadata']['uid'], csi_driver=driver)
                self.check(label, True, {'uid': info['metadata']['uid'],
                           'phase': 'Bound', 'pv_uid': pv['metadata']['uid'],
                           'csi_driver': driver}, scenario)
                return
            self.sleep(2)
        self.check(label, False, info.get('status'), scenario)

    def run_cephfs(self, nodes):
        pvc = f'app-cli-lockfs-{self.run_id}'
        self.create_volume(pvc, self.args.cephfs_class, 'ReadWriteMany',
                           CEPHFS_STEPS[0], 'T0-S-fs')
        self.create(self.serve_owner('fs-a', nodes[0], pvc))
        self.wait_ready('fs-a', nodes[0], pvc)
        first = self.wait_owner('fs-a')
        self.check(CEPHFS_STEPS[1], True, first, 'T0-S-fs')
        self.exec_in('fs-a', 'echo first > /shared/sentinel')
        detail, listeners = self.compete('fs-b', nodes[1], pvc)
        a, b = self.report['pods']['fs-a'], self.report['pods']['fs-b']
        self.check(CEPHFS_STEPS[2], a['node_name'] != b['node_name']
                   and a['pvc_uid'] == b['pvc_uid'] == self.volumes[pvc]['uid'],
                   {'holder': a, 'contender': b}, 'T0-S-fs')
        self.check(CEPHFS_STEPS[3], self.converged(detail), detail, 'T0-S-fs')
        self.check(CEPHFS_STEPS[4], self.observe_owner('fs-a') == first,
                   first, 'T0-S-fs')
        self.check(CEPHFS_STEPS[5], not listeners, listeners, 'T0-S-fs')
        self.delete_pod('fs-b')
        self.kill_exact_owner('fs-a', first)
        self.create(self.serve_owner('fs-c', nodes[1], pvc))
        self.wait_ready('fs-c', nodes[1], pvc)
        second = self.wait_owner('fs-c', previous=first)
        self.check(CEPHFS_STEPS[6], second['instance'] != first['instance']
                   and second['runtime_instance'] != first['runtime_instance'],
                   {'old': first, 'new': second}, 'T0-S-fs')
        self.check(CEPHFS_STEPS[7], self.exec_in('fs-c', 'cat /shared/sentinel').stdout.strip()
                   == 'first', None, 'T0-S-fs')
        self.delete_pod('fs-a')
        self.delete_pod('fs-c')

    def run_rbd(self, nodes):
        pvc = f'app-cli-lock-{self.run_id}'
        self.create_volume(pvc, self.args.storage_class, 'ReadWriteOnce',
                           RBD_STEPS[0], 'T0-S')
        self.create(self.serve_owner('lock-a', nodes[0], pvc))
        self.wait_ready('lock-a', nodes[0], pvc)
        first = self.wait_owner('lock-a')
        self.check(RBD_STEPS[1], True, first, 'T0-S')
        self.exec_in('lock-a', 'echo first > /shared/sentinel')
        detail, listeners = self.compete('lock-b-contender', nodes[0], pvc)
        self.check(RBD_STEPS[2], self.converged(detail), detail, 'T0-S')
        count = self.exec_in('lock-a', 'find /shared -name supervisor.json | wc -l').stdout.strip()
        self.check(RBD_STEPS[3], count == '1', count, 'T0-S')
        self.check(RBD_STEPS[4], self.observe_owner('lock-a') == first, first, 'T0-S')
        self.check(RBD_STEPS[5], not listeners, listeners, 'T0-S')
        self.delete_pod('lock-b-contender')
        self.kill_exact_owner('lock-a', first)
        self.create(self.serve_owner('lock-b', nodes[0], pvc))
        self.wait_ready('lock-b', nodes[0], pvc)
        second = self.wait_owner('lock-b', previous=first)
        self.check(RBD_STEPS[6], second['instance'] != first['instance'],
                   {'old': first, 'new': second}, 'T0-S')
        self.check(RBD_STEPS[7], self.exec_in('lock-b', 'cat /shared/sentinel').stdout.strip()
                   == 'first', None, 'T0-S')
        detail2, listeners2 = self.compete('lock-a-contender', nodes[0], pvc)
        count2 = self.exec_in('lock-b', 'find /shared -name supervisor.json | wc -l').stdout.strip()
        self.check(RBD_STEPS[8], self.converged(detail2) and count2 == '1', detail2, 'T0-S')
        self.check(RBD_STEPS[9], self.observe_owner('lock-b') == second and not listeners2,
                   listeners2, 'T0-S')
        self.delete_pod('lock-a-contender')
        self.delete_pod('lock-b')
        self.delete_pod('lock-a')
        self.create(self.serve_owner('lock-c', nodes[1], pvc))
        self.wait_ready('lock-c', nodes[1], pvc)
        third = self.wait_owner('lock-c', previous=second)
        self.check(RBD_STEPS[10], third['instance'] not in (first['instance'], second['instance']),
                   third, 'T0-S')
        self.check(RBD_STEPS[11], self.exec_in('lock-c', 'cat /shared/sentinel').stdout.strip()
                   == 'first', None, 'T0-S')
        self.delete_pod('lock-c')
        return pvc

    def ssh_node(self, node, command):
        mapping = next((m.partition('=')[2] for m in self.args.node_ssh
                        if m.partition('=')[0] == node), None)
        if mapping is None:
            raise RuntimeError(f'no --node-ssh mapping for {node}')
        host, _, rest = mapping.partition(':')
        user, _, password = rest.partition(':')
        if not re.fullmatch(r'[A-Za-z0-9._-]+', host):
            raise ValueError('invalid SSH host')
        user = user or 'soddy'
        if not re.fullmatch(r'[A-Za-z0-9._-]+', user):
            raise ValueError('invalid SSH user')
        remote = 'sudo ' + ('-S' if password else '-n') + ' -- sh -ec ' + shlex.quote(command)
        # 密码只从 stdin 进入 sudo，既不进命令行，也不写报告。
        return self.runner(['ssh', '-o', 'BatchMode=yes', f'{user}@{host}', remote],
                           capture_output=True, text=True, check=True, timeout=120,
                           input=password + '\n' if password else None)

    def restart_container(self, node, pod):
        out = self.ssh_node(node, 'k3s crictl ps --label io.kubernetes.pod.name='
                             + shlex.quote(pod) + ' -o json')
        containers = json.loads(out.stdout)['containers']
        captured = self.pods[pod]['uid']
        targets = [c for c in containers if c.get('state') == 'CONTAINER_RUNNING'
                   and c.get('labels', {}).get('io.kubernetes.pod.namespace') == self.namespace
                   and c.get('labels', {}).get('io.kubernetes.pod.uid') == captured]
        if len(targets) != 1:
            raise RuntimeError('exact captured runtime container is unavailable')
        self.ssh_node(node, 'k3s crictl stop --timeout 5 ' + shlex.quote(targets[0]['id']))

    def pid1_epoch(self, pod):
        script = "from pathlib import Path; print(Path('/proc/sys/kernel/random/boot_id').read_text().strip()+':'+Path('/proc/1/stat').read_text().rsplit(')',1)[1].split()[19])"
        return self.exec_in(pod, 'python3 -c ' + shlex.quote(script)).stdout.strip()

    def run_restart(self, nodes, pvc):
        name = 'restart-pod'
        self.create(self.serve_owner(name, nodes[0], pvc, binding=True))
        before = self.wait_ready(name, nodes[0], pvc)
        owner = self.wait_owner(name)
        epoch = self.pid1_epoch(name)
        self.check(RESTART_STEPS[0], True, owner, 'K8s-E')
        binding = self.exec_in(name, 'cat /etc/rcoder/platform/RCODER_PHYSICAL_POD_UID; '
                               'echo; cat /etc/rcoder/platform/execution-domain').stdout
        self.check(RESTART_STEPS[1], before['metadata']['uid'] in binding
                   and 'k8s-lock-test' in binding, binding, 'K8s-E')
        self.restart_container(nodes[0], name)
        deadline = self.clock() + 180
        old_cs = before['status']['containerStatuses'][0]
        after = None
        while self.clock() < deadline:
            info = self.get('pod', name)
            statuses = info.get('status', {}).get('containerStatuses', [])
            if statuses and statuses[0].get('ready') and statuses[0].get('containerID') != old_cs['containerID']:
                after = info
                break
            self.sleep(2)
        self.check(RESTART_STEPS[2], after is not None, None, 'K8s-E')
        cs = after['status']['containerStatuses'][0]
        self.check(RESTART_STEPS[3], after['metadata']['uid'] == before['metadata']['uid'],
                   {'before': before['metadata']['uid'], 'after': after['metadata']['uid']}, 'K8s-E')
        self.check(RESTART_STEPS[4], cs['containerID'] != old_cs['containerID']
                   and cs['restartCount'] > old_cs['restartCount'], cs['containerID'], 'K8s-E')
        new_epoch = self.pid1_epoch(name)
        self.check(RESTART_STEPS[5], new_epoch != epoch, {'old': epoch, 'new': new_epoch}, 'K8s-E')
        self.wait_ready(name, nodes[0], pvc)
        new_owner = self.wait_owner(name, previous=owner)
        self.check(RESTART_STEPS[6], new_owner['instance'] != owner['instance'], new_owner, 'K8s-E')
        self.check(RESTART_STEPS[7], self.exec_in(name, 'cat /shared/sentinel').stdout.strip()
                   == 'first', None, 'K8s-E')

    def diagnose(self):
        try:
            self.report['diagnostics'] = self.kubectl('get', 'pods', '-o', 'wide').stdout[-8000:]
        except Exception as error:
            self.report['diagnostic_error'] = self.error_text(error)

    def run(self):
        passed = False
        try:
            if self.args.cleanup_volume:
                raise ValueError('--cleanup-volume is forbidden: namespace and PVC must be retained')
            if not self.args.cephfs_class:
                raise ValueError('CephFS is required; --cephfs-class must not be empty')
            self.source_before = source_identity(self.source_dir)
            self.report['source_before'] = self.source_before
            self.receipt = load_build_receipt(self.args.build_receipt, self.args.image,
                                              self.source_before)
            self.report['build_receipt'] = self.receipt
            nodes = select_nodes(json.loads(self.kubectl('get', 'nodes', '-o', 'json',
                                                        cluster=True).stdout), self.args.node)
            self.report['nodes'] = nodes
            if self.args.scenario == 'all' and not any(m.startswith(nodes[0] + '=')
                                                       for m in self.args.node_ssh):
                raise ValueError('all requires --node-ssh for its first node (K8s-E)')
            namespace = {'apiVersion': 'v1', 'kind': 'Namespace', 'metadata': {
                'name': self.namespace, 'labels': {'rcoder.io/lock-test-run': self.run_id}}}
            out = self.kubectl('create', '-f', '-', '-o', 'json', cluster=True,
                               input_text=json.dumps(namespace))
            info = json.loads(out.stdout)
            self.namespace_uid = info['metadata']['uid']
            if self.args.scenario == 'all':
                pvc = self.run_rbd(nodes)
                self.run_cephfs(nodes)
                self.run_restart(nodes, pvc)
            else:
                self.run_cephfs(nodes)
            passed = True
        except (Exception, KeyboardInterrupt) as error:
            self.report['error'] = self.error_text(error)
            if self.namespace_uid:
                self.diagnose()
        finally:
            try:
                clean = self.cleanup()
            except (Exception, KeyboardInterrupt) as error:
                clean = False
                self.report['cleanup_ok'] = False
                self.report['cleanup_errors'] = [self.error_text(error)]
            # 收尾清理同样属于本轮冻结观察窗；无论源码是否漂移都按捕获身份清理。
            if self.source_before is not None:
                try:
                    after = source_identity(self.source_dir)
                    self.report['source_after'] = after
                    if self.receipt is not None:
                        matched = (after == self.source_before
                                   and self.receipt.get('source_commit') == after.get('origin_head')
                                   and self.receipt.get('source_digest') == after.get('worktree_sha256'))
                        self.check(SOURCE_STEP, matched,
                                   {'before': self.source_before, 'after': after})
                        require_build_source(self.receipt, after)
                except (Exception, KeyboardInterrupt) as error:
                    passed = False
                    self.report['source_error'] = self.error_text(error)
                    self.report.setdefault('error', self.error_text(error))
            if passed:
                try:
                    validate_required_steps(self.report, self.args.scenario)
                except Exception as error:
                    passed = False
                    self.report['error'] = self.error_text(error)
            self.report['success'] = passed and clean
            path = Path(self.args.report)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(self.report, ensure_ascii=False, indent=2) + '\n',
                            encoding='utf-8')
        return 0 if self.report['success'] else 1


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--context', required=True)
    parser.add_argument('--image', required=True)
    parser.add_argument('--build-receipt', required=True,
                        help='JSON schema_version=1, image digest, app_cli_sha256, '
                             'source_commit and source_digest; actual Pod binary must match')
    parser.add_argument('--source-dir', default=os.environ.get(
        'E2E_SOURCE_ROOT', str(Path(__file__).resolve().parents[2])),
        help='frozen source root; uses E2E_INPUT_MANIFEST/E2E_ORIGIN_HEAD when present')
    parser.add_argument('--scenario', choices=tuple(REQUIRED_STEPS), default='all')
    parser.add_argument('--node', action='append', required=True,
                        help='exactly two distinct Ready, schedulable node names')
    parser.add_argument('--storage-class', default='ceph-rbd')
    parser.add_argument('--cephfs-class', default='cephfs')
    parser.add_argument('--report', required=True)
    parser.add_argument('--cleanup-volume', action='store_true',
                        help='rejected: namespace/PVC deletion is forbidden')
    parser.add_argument('--node-ssh', action='append', default=[],
                        help='node=host[:user[:password]], only all/K8s-E uses it; '
                             'prefer passwordless sudo and never put real credentials in documents')
    return parser.parse_args(argv)


def main(argv=None):
    return ClusterExperiment(parse_args(argv)).run()


if __name__ == '__main__':
    raise SystemExit(main())
