#!/bin/bash
# ============================================================
# K8s 资源清理脚本
# 使用 Kustomize
# ============================================================

set -e

NAMESPACE="${NAMESPACE:-nuwax-rcoder-dev}"
MANIFESTS_DIR="./manifests"
ENV="${ENV:-dev}"  # dev 或 prod

echo "=== 使用 Kustomize 清理 RCoder 资源 (${ENV} 环境, namespace: ${NAMESPACE}) ==="

# 使用 kustomize 删除
if [ "$ENV" = "base" ]; then
    kubectl delete -k ${MANIFESTS_DIR}/base --ignore-not-found
else
    kubectl delete -k ${MANIFESTS_DIR}/overlays/${ENV} --ignore-not-found
fi

echo "=== RCoder 资源已清理 ==="
