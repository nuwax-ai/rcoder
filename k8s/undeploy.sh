#!/bin/bash
# ============================================================
# K8s 资源清理脚本 (Kustomize)
# 用法:
#   ENV=dev  ./undeploy.sh   # 清理开发环境 (默认)
#   ENV=prod ./undeploy.sh   # 清理生产环境
# ============================================================

set -e

ENV="${ENV:-dev}"

case "$ENV" in
    dev|prod) ;;
    *)
        echo "Error: ENV 必须是 dev 或 prod，当前值: $ENV"
        exit 1
        ;;
esac

# 路径解析：支持从任意 cwd 调用
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NAMESPACE="${NAMESPACE:-nuwax-rcoder-${ENV}}"
OVERLAY_DIR="${SCRIPT_DIR}/manifests/overlays/${ENV}"

echo "=== 使用 Kustomize 清理 RCoder 资源 (${ENV} 环境, namespace: ${NAMESPACE}) ==="

kubectl delete -k "${OVERLAY_DIR}" --ignore-not-found

echo "=== RCoder 资源已清理 ==="
