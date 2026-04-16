#!/bin/bash
set -e

echo "=== Starting Kind cluster for RCoder ==="

# 检查 kind 是否安装
if ! command -v kind &> /dev/null; then
    echo "Error: kind is not installed"
    echo "Install: brew install kind"
    exit 1
fi

# 检查 kubectl 是否安装
if ! command -v kubectl &> /dev/null; then
    echo "Error: kubectl is not installed"
    echo "Install: brew install kubectl"
    exit 1
fi

# 创建集群（如果不存在）
if kind get clusters 2>/dev/null | grep -q "rcoder-dev"; then
    echo "Kind cluster 'rcoder-dev' already exists"
else
    echo "Creating Kind cluster 'rcoder-dev'..."
    kind create cluster --config kind-config.yaml
fi

# 等待集群就绪
echo "Waiting for cluster to be ready..."
kubectl wait --for=condition=Ready nodes --all --timeout=60s

echo "=== Kind cluster is ready ==="
kubectl get nodes
kubectl get pods -n kube-system