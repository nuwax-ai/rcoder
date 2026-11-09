#!/bin/bash
# 在 192.168.1.19（k3s 单/多节点）的 nuwax-k8s-test 命名空间验证内存看门狗。
#
# 前提：
#   - 镜像已按 20-local-fast 流程推送到 nuwax-k8s-test 仓库（版本 0.1.416 或传参覆盖）
#   - /tmp/oom-bin/ 下有 mem_watch_scenarios + mem_hog（19 本机构建）
#   - 集群可拉取 aliyun 仓库（imagePullSecrets 已在 namespace 配置）
#
# 两个验证 Pod：
#   wd-int-oom —— 真实 agent_runner（新镜像全量运行时），RCODER_MEM_WATCH=dry，
#                 验证 mem_watch.started / 热切换 / 信号探针
#   wd-beh-oom —— 场景化 e2e（512Mi 限额 + limit_override），验证 off/on+窗口/dry
#
# 用法：IMAGE=<tag> bash deploy-oom-test-19.sh
set -euo pipefail

NS=nuwax-k8s-test
REG=nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com
IMAGE=${IMAGE:-$REG/nuwax-k8s-test/rcoder-computer-agent-runner:0.1.416}

echo "使用镜像: $IMAGE"

echo "== 1/3 wd-int-oom：集成验证（真实 agent_runner, mode=dry）=="
kubectl -n "$NS" apply -f - >/dev/null <<YML
apiVersion: v1
kind: Pod
metadata: {name: wd-int-oom, namespace: $NS, labels: {app: wd-oom-verify}}
spec:
  restartPolicy: Never
  containers:
  - name: agent-runner
    image: $IMAGE
    imagePullPolicy: IfNotPresent
    command: ["bash","-c","exec /usr/local/bin/agent_runner --port 18086"]
    env:
    - {name: RUST_LOG, value: info}
    - {name: RCODER_MEM_WATCH, value: dry}
    - {name: RCODER_MEM_WATCH_LIMIT_BYTES, value: "8589934592"}
    resources:
      requests: {memory: 1Gi}
      limits:   {memory: 8Gi}
YML

echo "== 2/3 wd-beh-oom：行为验证（场景化 e2e, 512Mi）=="
kubectl -n "$NS" apply -f - >/dev/null <<YML
apiVersion: v1
kind: Pod
metadata: {name: wd-beh-oom, namespace: $NS, labels: {app: wd-oom-verify}}
spec:
  restartPolicy: Never
  containers:
  - name: scenarios
    image: $IMAGE
    imagePullPolicy: IfNotPresent
    command: ["sh","-c","ls /mnt/bin/mem_watch_scenarios >/dev/null 2>&1 && cp /mnt/bin/mem_watch_scenarios /mnt/bin/mem_hog /tmp/ && chmod +x /tmp/mem_watch_scenarios /tmp/mem_hog && exec /tmp/mem_watch_scenarios || exec /usr/local/bin/mem_watch_scenarios"]
    env:
    - {name: RUST_LOG, value: debug}
    - {name: RCODER_MEM_WATCH_LIMIT_BYTES, value: "536870912"}
    volumeMounts: [{name: bin, mountPath: /mnt/bin, readOnly: true}]
    resources:
      requests: {memory: 256Mi}
      limits:   {memory: 512Mi}
  volumes:
  - name: bin
    hostPath: {path: /tmp/oom-bin, type: Directory}
YML

echo "== 3/3 状态 =="
sleep 10
kubectl -n "$NS" get pod -l app=wd-oom-verify
echo
echo "验证命令："
echo "  kubectl -n $NS logs wd-int-oom -c agent-runner | grep -E 'mem_watch|ERROR' | head"
echo "  kubectl -n $NS exec wd-int-oom -c agent-runner -- sh -c 'echo mode=on > /tmp/mem-watch.conf; sleep 2'"
echo "  kubectl -n $NS logs wd-int-oom -c agent-runner | grep mode_changed | tail -1"
echo "  kubectl -n $NS logs wd-beh-oom -c scenarios | grep -E 'PASS|ALL SCENARIOS|Error'"
