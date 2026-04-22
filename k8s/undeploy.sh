#!/bin/bash
# ============================================================
# K8s 资源清理脚本
# 使用 Kustomize
# ============================================================

set -e

NAMESPACE="nuwax-rcoder"
MANIFESTS_DIR="./manifests"

echo "=== 使用 Kustomize 清理 RCoder 资源 ==="

# 使用 kustomize 删除
kubectl delete -k ${MANIFESTS_DIR}/base --ignore-not-found

echo "=== RCoder 资源已清理 ==="
