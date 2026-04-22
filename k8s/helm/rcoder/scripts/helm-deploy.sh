#!/bin/bash
# ============================================================
# RCoder Helm 部署脚本
# 负责 Helm Chart 的部署/升级/卸载操作
# 由 deploy.sh 调用，或独立使用
# ============================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(dirname "$SCRIPT_DIR")"

NAMESPACE="${NAMESPACE:-nuwax-rcoder}"
VALUES_FILE="${VALUES_FILE:-${CHART_DIR}/values.yaml}"
ACTION="${1:-install}"  # install | upgrade | uninstall | template

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

usage() {
    echo "用法: $0 [命令]"
    echo ""
    echo "命令:"
    echo "  install   安装/升级 RCoder (默认)"
    echo "  upgrade   升级 RCoder"
    echo "  uninstall 卸载 RCoder"
    echo "  template  渲染模板但不部署"
    echo "  status    查看 release 状态"
    echo ""
    echo "环境变量:"
    echo "  NAMESPACE     命名空间 (默认: nuwax-rcoder)"
    echo "  VALUES_FILE   values 文件路径"
    echo ""
    echo "示例:"
    echo "  NAMESPACE=nuwax-rcoder VALUES_FILE=${CHART_DIR}/values-prod.yaml $0 install"
    echo "  $0 uninstall"
}

case "$ACTION" in
    install)
        echo -e "${GREEN}部署 RCoder 到 namespace: $NAMESPACE${NC}"
        helm upgrade --install rcoder "$CHART_DIR" \
            --namespace "$NAMESPACE" \
            --create-namespace \
            --values "$VALUES_FILE" \
            --wait --timeout 5m
        echo -e "${GREEN}✅ 部署完成${NC}"
        ;;
    upgrade)
        echo -e "${GREEN}升级 RCoder in namespace: $NAMESPACE${NC}"
        helm upgrade rcoder "$CHART_DIR" \
            --namespace "$NAMESPACE" \
            --values "$VALUES_FILE" \
            --wait --timeout 5m
        echo -e "${GREEN}✅ 升级完成${NC}"
        ;;
    uninstall)
        echo -e "${YELLOW}卸载 RCoder from namespace: $NAMESPACE${NC}"
        helm uninstall rcoder --namespace "$NAMESPACE"
        echo -e "${GREEN}✅ 卸载完成${NC}"
        ;;
    template)
        echo -e "${GREEN}渲染 Helm 模板 (不部署)${NC}"
        helm template rcoder "$CHART_DIR" --values "$VALUES_FILE"
        ;;
    status)
        helm status rcoder --namespace "$NAMESPACE"
        ;;
    help|--help|-h)
        usage
        exit 0
        ;;
    *)
        echo -e "${RED}未知命令: $ACTION${NC}"
        usage
        exit 1
        ;;
esac
