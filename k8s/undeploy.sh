#!/bin/bash
set -e

echo "=== Undeploying RCoder from K8s cluster ==="

# 删除 Deployment 和 Service
kubectl delete -f manifests/rcoder-deployment.yaml --ignore-not-found
kubectl delete -f manifests/rcoder-service.yaml --ignore-not-found

# 清理由 RCoder Kubernetes 运行时创建的 Agent 等业务 Pod（与 Makefile dev-down-k8s 一致）
if kubectl get namespace rcoder >/dev/null 2>&1; then
  kubectl delete pods -n rcoder -l managed-by=rcoder-runtime --ignore-not-found
fi

# 删除 RBAC 配置（须先于 namespace：避免仅删 ns 时遗留 ClusterRole/ClusterRoleBinding）
kubectl delete -f manifests/serviceaccount.yaml --ignore-not-found

# 删除 namespace
kubectl delete -f manifests/namespace.yaml --ignore-not-found

echo "=== RCoder undeployed ==="