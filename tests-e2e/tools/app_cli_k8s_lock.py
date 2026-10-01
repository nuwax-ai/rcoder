#!/usr/bin/env python3
"""app-cli recovery v2 K8s real-cluster experiments (plan §11.2/§11.3).

Runs against a personal test cluster (default context k3s-131):

- T0-S RBD lock chain on real ceph-rbd storage: two same-node pods mounting
  one RWO PVC contend through app-cli's ACTUAL owner acquisition path
  (File::try_lock); SIGKILL releases; roles swap; a normal cross-node
  detach/attach handover follows. No lock-file deletion anywhere.
- K8s E same-Pod container restart: a builder-shaped pod (Downward API
  platform binding) restarts its container in place — asserts Pod UID
  unchanged, container identity changed, pid1 epoch changed, PVC sentinel
  preserved, and the new owner reconciling the old generation.

Only resources it creates are removed (namespace-scoped); PVC data of the
retained volume is left for inspection unless --cleanup-volume is passed.
"""
import argparse
import json
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--context', default='k3s-131')
    parser.add_argument('--image', required=True)
    parser.add_argument('--storage-class', default='ceph-rbd')
    parser.add_argument('--report', required=True, type=str)
    parser.add_argument('--cleanup-volume', action='store_true')
    parser.add_argument('--node-ssh', action='append', default=[],
                        help='node=host[:user[:password]] mapping for runtime '
                             'container restarts, e.g. soddy=192.168.32.131:soddy:pw')
    args = parser.parse_args()

    run_id = 'rcv' + uuid.uuid4().hex[:8]
    namespace = f'rcoder-e2e-soddy-lock-{run_id}'
    report = {'run_id': run_id, 'namespace': namespace, 'image': args.image,
              'storage_class': args.storage_class, 'checks': [], 'scenarios': {}}

    def kubectl(*argv, check=True, timeout=180, input_text=None):
        cmd = ['kubectl', '--context', args.context, '-n', namespace, *argv]
        return subprocess.run(cmd, capture_output=True, text=True, check=check,
                              timeout=timeout, input=input_text)

    def kubectl_raw(*argv, check=True, timeout=180):
        return subprocess.run(['kubectl', '--context', args.context, *argv],
                              capture_output=True, text=True, check=check, timeout=timeout)

    def check(label, passed, detail=None, scenario=None):
        report['checks'].append({'name': label, 'ok': bool(passed), 'detail': detail})
        if scenario:
            report['scenarios'].setdefault(scenario, []).append(label)
        print(label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise RuntimeError(label)

    def apply(name, manifest):
        kubectl('apply', '-f', '-', input_text=json.dumps(manifest))

    def wait_ready(pod, budget=180):
        deadline = time.monotonic() + budget
        while time.monotonic() < deadline:
            out = kubectl('get', 'pod', pod, '-o', 'json', check=False).stdout
            try:
                info = json.loads(out)
                statuses = [c.get('ready') for c in
                            info['status'].get('containerStatuses', [])]
                if statuses and all(statuses):
                    return info
            except (ValueError, KeyError):
                pass
            time.sleep(2)
        raise RuntimeError(f'{pod} not ready')

    def exec_in(pod, command, check=True, timeout=120):
        return kubectl('exec', pod, '--', 'sh', '-ec', command,
                       check=check, timeout=timeout)

    def pod_spec(name, node, command, pvc, binding=False):
        volumes = [{'name': 'shared',
                    'persistentVolumeClaim': {'claimName': pvc}}]
        if binding:
            volumes.append({
                'name': 'rcoder-platform-binding',
                'downwardAPI': {'items': [
                    {'path': 'RCODER_PHYSICAL_POD_UID',
                     'fieldRef': {'fieldPath': 'metadata.uid'}},
                    {'path': 'execution-domain',
                     'fieldRef': {'fieldPath':
                                  "metadata.annotations['rcoder.io/execution-domain']"}},
                ]}})
        mounts = [{'name': 'shared', 'mountPath': '/shared'}]
        if binding:
            mounts.append({'name': 'rcoder-platform-binding',
                           'mountPath': '/etc/rcoder/platform', 'readOnly': True})
        return {
            'apiVersion': 'v1', 'kind': 'Pod',
            'metadata': {'name': name,
                         'annotations': {'rcoder.io/execution-domain':
                                         json.dumps({'authority': 'k8s-lock-test',
                                                     'volume': f'pvc:{pvc}',
                                                     'instance': '',
                                                     'instance_source_env':
                                                         'RCODER_PHYSICAL_POD_UID'})}},
            'spec': {'nodeName': node, 'restartPolicy': 'Always',
                     'volumes': volumes,
                     'containers': [{
                         'name': 'main', 'image': args.image,
                         'imagePullPolicy': 'IfNotPresent',
                         'command': command,
                         'volumeMounts': mounts}]},
        }

    def serve_owner(pod, node, pvc, binding=False):
        # serve 作为后台子进程、存活壳为 pid1：SIGKILL 持锁进程时容器不退出、
        # 不自动复活 serve（successor 有获锁窗口）；pod 删除仍走正常终止。
        return pod_spec(pod, node,
                        ['/bin/sh', '-ec',
                         'mkdir -p /shared/ws /shared/logs; '
                         'app-cli serve --workspace /shared/ws '
                         '--log-dir /shared/logs --admin-addr 0.0.0.0:3010 '
                         '>/shared/logs/serve.out 2>&1 & '
                         'echo $! > /shared/logs/serve.pid; '
                         'wait $(cat /shared/logs/serve.pid) || true; '
                         'sleep 3600'],
                        pvc, binding=binding)

    def try_second_owner(pod, node, pvc, budget=180):
        """A competing serve against a live owner: refused (lock held, no
        second API) or transferred (dispatch to the running owner). Both prove
        single-owner semantics; a second discovery must never appear."""
        spec = pod_spec(pod, node,
                        ['/bin/sh', '-ec',
                         'mkdir -p /shared/logs; '
                         'rc=0; timeout 90 app-cli serve --workspace /shared/ws '
                         '--log-dir /shared/logs --admin-addr 0.0.0.0:3999 '
                         '>/shared/logs/second.log 2>&1 || rc=$?; '
                         'echo "second-rc=$rc"; '
                         'sleep 3600'],
                        pvc)
        apply(pod, spec)
        wait_ready(pod)
        deadline = time.monotonic() + budget
        detail = ''
        while time.monotonic() < deadline:
            detail = exec_in(pod, 'tail -c 900 /shared/logs/second.log',
                             check=False).stdout or ''
            if 'second-rc=' not in detail and 'unified' not in detail:
                # serve still starting; keep waiting for its terminal line
                pass
            logs = kubectl('logs', pod, check=False, timeout=30).stdout or ''
            if 'second-rc=' in logs:
                detail = logs[-900:] + '\n---file---\n' + detail
                break
            time.sleep(3)
        kubectl('delete', 'pod', pod, '--wait=true')
        return detail

    def read_discovery(pod):
        out = exec_in(pod,
                      'find /shared -name supervisor.json -exec cat {} \\; 2>/dev/null',
                      check=False).stdout
        try:
            return json.loads(out.strip().splitlines()[-1])['instance']
        except (ValueError, IndexError, KeyError):
            return None

    def discovery_instance():
        return read_discovery('lock-a')

    node_ssh = {}
    for mapping in args.node_ssh:
        node, _, rest = mapping.partition('=')
        host, _, cred = rest.partition(':')
        user, _, password = cred.partition(':')
        node_ssh[node] = (host, user or 'soddy', password)

    def ssh_node(node, command, timeout=120):
        host, user, password = node_ssh.get(
            node, (None, None, None))
        if host is None:
            raise RuntimeError(f'no --node-ssh mapping for node {node}')
        remote = f"echo {password} | sudo -S sh -ec '{command}'"
        return subprocess.run(['ssh', f'{user}@{host}', remote],
                              capture_output=True, text=True, timeout=timeout)

    def restart_container(node, pod):
        # pid1 在自身 PID namespace 内不可被 SIGKILL（内核保护）；从节点
        # runtime 侧 stop 容器 → kubelet 原地重启（restartCount+1，Pod 不变）。
        # 容器名与 pod 名不同：按 pod.name 标签定位（crictl 单标签一次；
        # namespace 由 run_id 唯一性保证不串）。
        listing = ssh_node(
            node,
            f'k3s crictl ps --label io.kubernetes.pod.name={pod} -o json'
            ' 2>/dev/null')
        containers = json.loads(listing.stdout or '[]').get('containers', [])
        target = next((c for c in containers
                       if c.get('state') == 'CONTAINER_RUNNING'
                       and (c.get('labels') or {}).get(
                           'io.kubernetes.pod.namespace') == namespace), None)
        if target is None:
            raise RuntimeError(f'container for {pod} not found on {node}')
        cid = target['id']
        ssh_node(node, f'k3s crictl stop --timeout 5 {cid}')
        return cid

    def owner_pid1_epoch(pod):
        out = exec_in(pod,
                      'b=$(cat /proc/sys/kernel/random/boot_id); '
                      's=$(awk "{print \\$22}" /proc/1/stat); '
                      'echo "$b:$s"').stdout.strip()
        return out

    nodes = [n['metadata']['name'] for n in json.loads(
        kubectl_raw('get', 'nodes', '-o', 'json').stdout)['items']]
    if len(nodes) < 2:
        raise RuntimeError(f'need two nodes for cross-node handover, got {nodes}')
    print('nodes:', nodes)

    pvc_name = f'app-cli-lock-{run_id}'
    try:
        kubectl_raw('create', 'namespace', namespace)
        apply(pvc_name, {
            'apiVersion': 'v1', 'kind': 'PersistentVolumeClaim',
            'metadata': {'name': pvc_name},
            'spec': {'accessModes': ['ReadWriteOnce'], 'resources': {
                'requests': {'storage': '1Gi'}},
                'storageClassName': args.storage_class}})
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            phase = json.loads(kubectl('get', 'pvc', pvc_name, '-o',
                                       'json').stdout)['status'].get('phase')
            if phase == 'Bound':
                break
            time.sleep(2)
        check('setup: RBD PVC bound', phase == 'Bound', phase, scenario='setup')

        # ── T0-S：同节点双 Pod 经真实 owner 获取链争锁 ───────────────
        apply('lock-a', serve_owner('lock-a', nodes[0], pvc_name))
        wait_ready('lock-a')
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline and discovery_instance() is None:
            time.sleep(2)
        first = discovery_instance()
        check('T0-S: first owner acquires on RBD volume', bool(first), first,
              scenario='T0-S')
        exec_in('lock-a', 'echo first > /shared/sentinel')

        contender = try_second_owner('lock-b-contender', nodes[0], pvc_name)
        refused = ('owner lock is held' in contender
                   or 'refusing to start a competing orchestrator' in contender)
        transferred = ('dispatch' in contender.lower()
                       or 'second-rc=0' in contender)
        check('T0-S: same-node competitor converges to the single owner',
              refused or transferred, contender[-400:], scenario='T0-S')
        discoveries = exec_in('lock-a',
                              'find /shared -name supervisor.json | wc -l').stdout.strip()
        check('T0-S: still exactly one discovery after contention',
              discoveries == '1', discoveries, scenario='T0-S')

        # SIGKILL 持锁进程（Pod 保留）：锁必须由 OS 释放，竞争者可获锁。
        exec_in('lock-a', 'kill -9 "$(cat /shared/logs/serve.pid)"')
        apply('lock-b', serve_owner('lock-b', nodes[0], pvc_name))
        wait_ready('lock-b')
        deadline = time.monotonic() + 120
        second = None
        while time.monotonic() < deadline:
            second = read_discovery('lock-b')
            if second and second != first:
                break
            time.sleep(3)
        check('T0-S: SIGKILL releases lock; successor acquires',
              bool(second) and second != first, f'{first} -> {second}',
              scenario='T0-S')
        check('T0-S: sentinel data preserved across owner change',
              exec_in('lock-b', 'cat /shared/sentinel').stdout.strip() == 'first',
              None, scenario='T0-S')

        # 角色互换：B 持锁，A（已杀）重启后必须等待而不是双持。
        contender2 = try_second_owner('lock-a-contender', nodes[0], pvc_name)
        refused2 = ('owner lock is held' in contender2
                    or 'refusing to start a competing orchestrator' in contender2)
        transferred2 = ('dispatch' in contender2.lower()
                        or 'second-rc=0' in contender2)
        discoveries2 = exec_in('lock-b',
                               'find /shared -name supervisor.json | wc -l').stdout.strip()
        check('T0-S: role swap keeps single owner semantics',
              (refused2 or transferred2) and discoveries2 == '1',
              contender2[-300:] + f' discoveries={discoveries2}',
              scenario='T0-S')

        # ── T0-S：跨节点正常卸载/挂载交接 ────────────────────────────
        kubectl('delete', 'pod', 'lock-b', '--wait=true')
        kubectl('delete', 'pod', 'lock-a', '--wait=true')
        # RWO：等待 volume detach 后另一节点才可 attach（正常路径，不强制双挂）。
        time.sleep(10)
        deadline = time.monotonic() + 300
        third = None
        apply('lock-c', serve_owner('lock-c', nodes[1], pvc_name))
        while time.monotonic() < deadline:
            info = json.loads(kubectl('get', 'pod', 'lock-c', '-o',
                                      'json', check=False).stdout)
            scheduled = info['spec'].get('nodeName') == nodes[1]
            if scheduled:
                third = read_discovery('lock-c')
                if third:
                    break
            time.sleep(3)
        check('T0-S: cross-node detach/attach handover',
              bool(third) and third not in (first, second),
              f'{second} -> {third}', scenario='T0-S')
        check('T0-S: data intact after cross-node handover',
              exec_in('lock-c', 'cat /shared/sentinel').stdout.strip() == 'first',
              None, scenario='T0-S')

        # ── K8s E：同 Pod 容器重启（builder 形态 Downward API 绑定）────
        kubectl('delete', 'pod', 'lock-c', '--wait=true')
        time.sleep(10)
        apply('restart-pod', serve_owner('restart-pod', nodes[0], pvc_name,
                                         binding=True))
        wait_ready('restart-pod')
        deadline = time.monotonic() + 90
        before_instance = None
        while time.monotonic() < deadline:
            before_instance = read_discovery('restart-pod')
            if before_instance:
                break
            time.sleep(3)
        before = json.loads(kubectl('get', 'pod', 'restart-pod', '-o',
                                    'json').stdout)
        before_uid = before['metadata']['uid']
        before_container = before['status']['containerStatuses'][0][
            'containerID']
        before_restarts = before['status']['containerStatuses'][0]['restartCount']
        before_epoch = owner_pid1_epoch('restart-pod')
        check('K8s-E: owner running with platform binding',
              bool(before_instance), before_instance, scenario='K8s-E')
        binding_read = exec_in(
            'restart-pod',
            'cat /etc/rcoder/platform/RCODER_PHYSICAL_POD_UID; echo; '
            'head -c 120 /etc/rcoder/platform/execution-domain').stdout
        check('K8s-E: downward API binding files present',
              before_uid in binding_read and 'execution-domain' not in binding_read
              and 'k8s-lock-test' in binding_read, binding_read, scenario='K8s-E')

        # 同 Pod 容器原地重启：节点 runtime 侧 stop（pid1 内部不可杀）。
        restart_container(nodes[0], 'restart-pod')
        deadline = time.monotonic() + 180
        after = None
        while time.monotonic() < deadline:
            info = json.loads(kubectl('get', 'pod', 'restart-pod', '-o',
                                      'json', check=False).stdout)
            cs = info['status'].get('containerStatuses', [{}])[0]
            if cs.get('containerID') != before_container and cs.get('ready'):
                after = info
                break
            time.sleep(2)
        check('K8s-E: container restarted in place', after is not None, None,
              scenario='K8s-E')
        after_cs = after['status']['containerStatuses'][0]
        check('K8s-E: Pod UID unchanged',
              after['metadata']['uid'] == before_uid,
              f'{before_uid} vs {after["metadata"]["uid"]}', scenario='K8s-E')
        check('K8s-E: container identity changed',
              after_cs['containerID'] != before_container
              and (after_cs['restartCount'] > before_restarts
                   or after_cs.get('startedAt') != before['status']
                   ['containerStatuses'][0].get('startedAt')),
              f'{before_container} -> {after_cs["containerID"]}',
              scenario='K8s-E')
        after_epoch = owner_pid1_epoch('restart-pod')
        check('K8s-E: pid1 epoch changed',
              after_epoch != before_epoch,
              f'{before_epoch} -> {after_epoch}', scenario='K8s-E')
        deadline = time.monotonic() + 120
        after_instance = None
        while time.monotonic() < deadline:
            after_instance = read_discovery('restart-pod')
            if after_instance and after_instance != before_instance:
                break
            time.sleep(3)
        check('K8s-E: new owner reconciles and serves after restart',
              bool(after_instance) and after_instance != before_instance,
              f'{before_instance} -> {after_instance}', scenario='K8s-E')
        check('K8s-E: PVC sentinel preserved',
              exec_in('restart-pod', 'cat /shared/sentinel').stdout.strip()
              == 'first', None, scenario='K8s-E')

        report['success'] = True
    except (Exception, KeyboardInterrupt) as error:
        report.update(success=False, error=str(error))
        report['diagnostics'] = kubectl('get', 'pods', '-o', 'wide',
                                        check=False).stdout
        for pod in ('lock-a', 'lock-b', 'lock-c', 'restart-pod'):
            logs = kubectl('exec', pod, '--', 'sh', '-ec',
                           'tail -c 2000 /shared/logs/serve.out 2>/dev/null; '
                           'echo ---; cat /shared/logs/serve.pid 2>/dev/null',
                           check=False, timeout=60)
            if logs.stdout.strip():
                report[f'logs:{pod}'] = logs.stdout[-2000:]
    finally:
        kubectl_raw('delete', 'namespace', namespace,
                    '--ignore-not-found=true', '--wait=true')
        report['cleanup_ok'] = True
        with open(args.report, 'w', encoding='utf-8') as handle:
            json.dump(report, handle, ensure_ascii=False, indent=2)
    return 0 if report.get('success') else 1


if __name__ == '__main__':
    raise SystemExit(main())
