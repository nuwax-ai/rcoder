#!/bin/bash
# 在 192.168.31.45（单节点 k3s）上验证内存看门狗 —— 免镜像构建方案。
#
# 原理：45 的 docker 内容存储有损坏（save 只出 37KB 空壳 tar，见 AGENTS.md 已知
# 问题 2 的同类形态），因此完全绕过 docker：
#   1. 用 k3s ctr 给**已在 containerd 里**的生产镜像（0.1.407）打新 tag（零拷贝）
#   2. 新二进制经 hostPath 挂进容器，initContainer 覆盖旧 agent_runner
#   3. 两个 Pod：
#      wd-integration —— 真实 agent_runner（完整沙箱镜像），dry + 热切换验证接线
#      wd-behavior    —— 场景化 e2e，512Mi 限额，验证 off / on+窗口 / dry
set -euo pipefail

NS=oom-test
SRC=/tmp/rcoder-src/target/release
BINDIR=/tmp/oom-bin
TAG_REF=nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-k8s-test/rcoder-computer-agent-runner:0.1.407
NEW_TAG=oom-watchdog-test:dev-oom

echo "== 1/6 准备二进制目录（hostPath 源）=="
mkdir -p "$BINDIR"
cp "$SRC/agent_runner" "$BINDIR/"
cp "$SRC/examples/mem_watch_scenarios" "$SRC/examples/mem_hog" "$BINDIR/"
chmod +x "$BINDIR"/*
ls -la "$BINDIR"

echo "== 2/6 在 k3s containerd 里给生产镜像打 tag（零拷贝）=="
sudo k3s ctr images tag "$TAG_REF" "$NEW_TAG" 2>/dev/null || true
sudo k3s ctr images list | grep "$NEW_TAG" | head -1

echo "== 3/6 命名空间 =="
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

echo "== 4/6 wd-integration：真实 agent_runner + 热切换 =="
cat <<YML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata:
  name: wd-integration
  namespace: $NS
spec:
  restartPolicy: Never
  initContainers:
  - name: inject-binary
    image: $NEW_TAG
    imagePullPolicy: Never
    command: ["sh", "-c", "cp /mnt/bin/agent_runner /usr/local/bin/agent_runner && echo injected"]
    volumeMounts: [{ name: bin, mountPath: /mnt/bin }]
    securityContext: { runAsUser: 0 }
  containers:
  - name: agent-runner
    image: $NEW_TAG
    imagePullPolicy: Never
    command: ["bash", "-c", "exec /usr/local/bin/agent_runner --port 18086"]
    env:
    - { name: RUST_LOG, value: "info" }
    volumeMounts: [{ name: bin, mountPath: /mnt/bin, readOnly: true }]
    resources:
      requests: { memory: 512Mi }
      limits:   { memory: 2Gi }
  volumes:
  - name: bin
    hostPath: { path: $BINDIR, type: Directory }
YML

echo "== 5/6 wd-behavior：场景化 e2e =="
cat <<YML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata:
  name: wd-behavior
  namespace: $NS
spec:
  restartPolicy: Never
  initContainers:
  - name: inject-binary
    image: $NEW_TAG
    imagePullPolicy: Never
    command: ["sh", "-c", "cp /mnt/bin/mem_watch_scenarios /usr/local/bin/ && cp /mnt/bin/mem_hog /usr/local/bin/ && echo injected"]
    volumeMounts: [{ name: bin, mountPath: /mnt/bin }]
    securityContext: { runAsUser: 0 }
  containers:
  - name: scenarios
    image: $NEW_TAG
    imagePullPolicy: Never
    command: ["/usr/local/bin/mem_watch_scenarios"]
    volumeMounts: [{ name: bin, mountPath: /mnt/bin, readOnly: true }]
    resources:
      requests: { memory: 256Mi }
      limits:   { memory: 512Mi }
  volumes:
  - name: bin
    hostPath: { path: $BINDIR, type: Directory }
YML

echo "== 6/6 完成 =="
sleep 5
kubectl -n "$NS" get pods
echo
echo "验证命令："
echo "  kubectl -n $NS logs wd-integration -c agent-runner | grep mem_watch   # started + mode_changed"
echo "  kubectl -n $NS logs wd-behavior  -c scenarios     | tail -25          # 三个场景 PASS"
