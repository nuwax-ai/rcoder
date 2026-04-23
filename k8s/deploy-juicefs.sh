#!/bin/bash
# ============================================================
# JuiceFS + MinIO + PostgreSQL 部署脚本
# 使用 Kustomize 部署
# ============================================================

set -e

NAMESPACE="${NAMESPACE:-nuwax-rcoder}"
MANIFESTS_DIR="./manifests"
STORAGE_DIR="${MANIFESTS_DIR}/base/storage"

echo "=========================================="
echo "  RCoder - JuiceFS 存储部署"
echo "=========================================="

# 检查 kubectl
if ! command -v kubectl &> /dev/null; then
    echo "Error: kubectl not found"
    exit 1
fi

# 检查 kubectl kustomize 插件
if ! kubectl kustomize --help &> /dev/null; then
    echo "Error: kubectl kustomize not found (需要 kubectl 1.14+)"
    exit 1
fi

# 部署 JuiceFS CSI Driver
echo ""
echo "[1/6] 检查 JuiceFS CSI Driver..."
if ! kubectl get daemonset juicefs-csi-driver-node -n kube-system &> /dev/null; then
    echo "JuiceFS CSI Driver 未部署，是否部署? (y/n)"
    read -r response
    if [[ "$response" =~ ^[Yy]$ ]]; then
        helm repo add juicefs https://juicefs.github.io/charts || true
        helm repo update
        helm install juicefs-csi-driver juicefs/juicefs-csi-driver \
          --namespace kube-system \
          --set webhook.enabled=false
        echo "等待 JuiceFS CSI Driver 就绪..."
        kubectl rollout status daemonset/juicefs-csi-driver-node -n kube-system --timeout=120s
    fi
fi

# 使用 Kustomize 部署
echo ""
echo "[2/6] 使用 Kustomize 部署..."
kubectl apply -k ${MANIFESTS_DIR}/base

# 等待存储层就绪
echo ""
echo "[3/6] 等待 PostgreSQL 就绪..."
kubectl wait --for=condition=ready pod -l app=postgresql -n ${NAMESPACE} --timeout=180s 2>/dev/null || echo "⚠️  PostgreSQL 等待超时"

echo ""
echo "[4/6] 等待 MinIO 就绪..."
kubectl wait --for=condition=ready pod -l app=minio -n ${NAMESPACE} --timeout=180s 2>/dev/null || echo "⚠️  MinIO 等待超时"

echo ""
echo "[5/6] 等待 MinIO Bucket 初始化..."
kubectl wait --for=condition=complete job/minio-init -n ${NAMESPACE} --timeout=120s 2>/dev/null || echo "⚠️  MinIO 初始化等待超时"

# 验证部署
echo ""
echo "[6/6] 验证部署状态..."
echo ""
echo "StorageClass:"
kubectl get sc | grep juicefs || echo "  (未找到)"

echo ""
echo "PVC:"
kubectl get pvc -n ${NAMESPACE}

echo ""
echo "Pods:"
kubectl get pods -n ${NAMESPACE}

echo ""
echo "=========================================="
echo "  部署完成！"
echo "=========================================="
