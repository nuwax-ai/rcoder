#!/bin/bash
set -e

echo "=== Undeploying RCoder from K8s cluster ==="

# Step 1: 清理 RCoder 应用层
kubectl delete -f manifests/rcoder-deployment.yaml --ignore-not-found
kubectl delete -f manifests/rcoder-service.yaml --ignore-not-found
kubectl delete -f manifests/rcoder-pdb.yaml --ignore-not-found
kubectl delete -f manifests/rcoder-networkpolicy.yaml --ignore-not-found
kubectl delete -f manifests/rcoder-pvc.yaml --ignore-not-found
kubectl delete -f manifests/rcoder-configmap.yaml --ignore-not-found

# 清理 RCoder 运行时创建的 Pod 和 PVC
if kubectl get namespace rcoder >/dev/null 2>&1; then
  kubectl delete pods,pvc -n rcoder -l managed-by=rcoder-runtime --ignore-not-found
fi

# Step 2: 清理 RBAC
kubectl delete -f manifests/serviceaccount.yaml --ignore-not-found

# Step 3: 清理 Namespace
kubectl delete -f manifests/namespace.yaml --ignore-not-found

# Step 4: 清理 NFS 存储层（最后删除，确保无 Pod 引用）
kubectl delete -f manifests/nfs-server.yaml --ignore-not-found

echo "=== RCoder undeployed ==="
