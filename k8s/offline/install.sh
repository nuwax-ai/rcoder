#!/bin/bash
# ============================================================================
# RCoder 离线部署脚本
#
# 使用场景: 政企内网, 无外网访问
#
# 运行流程:
#   1. 将镜像导入到节点 containerd (direct 模式) 或推到私有 registry (registry 模式)
#   2. 安装 Longhorn 存储 (可跳过)
#   3. 安装 JuiceFS CSI Driver (可跳过)
#   4. 安装 RCoder Helm chart
#
# 依赖 (离线机器上必须预装):
#   - kubectl (配置好 kubeconfig)
#   - helm 3.x
#   - docker (registry 模式) 或 k3s/nerdctl (direct 模式)
# ============================================================================
set -e

# ---- 默认参数 ----
MODE="direct"          # direct | registry
REGISTRY=""            # registry 模式下必填, 例: harbor.internal
THIRDPARTY_REGISTRY="" # 第三方镜像目标 registry, 留空同 REGISTRY
ENV="dev"              # dev | prod
RELEASE_NAME=""        # 留空 => rcoder-$ENV
SKIP_LONGHORN=0
SKIP_JUICEFS_CSI=0
SKIP_IMAGE_IMPORT=0    # 调试用: 跳过镜像导入
NAMESPACE=""           # 留空 => nuwax-rcoder-$ENV

# ---- 颜色 ----
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
    cat <<EOF
用法: $0 [选项]

选项:
  --mode=MODE              direct (默认) 或 registry
  --registry=URL           私有 registry 地址 (registry 模式必填)
  --thirdparty-registry=URL 第三方镜像目标 (留空同 --registry)
  --env=ENV                dev (默认) 或 prod
  --release=NAME           Helm release 名 (默认 rcoder-\$ENV)
  --namespace=NS           目标 namespace (默认 nuwax-rcoder-\$ENV)
  --skip-longhorn          跳过 Longhorn 安装 (集群已有 CSI 时用)
  --skip-juicefs-csi       跳过 JuiceFS CSI 安装
  --skip-image-import      跳过镜像导入 (调试/镜像已就位时用)
  -h|--help                显示本帮助

示例:
  # 最常见: 断网机器直接用节点 containerd 本地缓存
  $0 --mode=direct --env=dev

  # 有私有 registry: 先推送再部署
  $0 --mode=registry --registry=harbor.internal/rcoder --env=prod

  # 集群已有存储, 只装 RCoder 本身
  $0 --mode=direct --env=dev --skip-longhorn --skip-juicefs-csi
EOF
    exit 0
}

# ---- 参数解析 ----
while [ $# -gt 0 ]; do
    case "$1" in
        --mode=*) MODE="${1#*=}" ;;
        --registry=*) REGISTRY="${1#*=}" ;;
        --thirdparty-registry=*) THIRDPARTY_REGISTRY="${1#*=}" ;;
        --env=*) ENV="${1#*=}" ;;
        --release=*) RELEASE_NAME="${1#*=}" ;;
        --namespace=*) NAMESPACE="${1#*=}" ;;
        --skip-longhorn) SKIP_LONGHORN=1 ;;
        --skip-juicefs-csi) SKIP_JUICEFS_CSI=1 ;;
        --skip-image-import) SKIP_IMAGE_IMPORT=1 ;;
        -h|--help) usage ;;
        *) echo -e "${RED}未知参数: $1${NC}"; usage ;;
    esac
    shift
done

[ -z "$RELEASE_NAME" ] && RELEASE_NAME="rcoder-$ENV"
[ -z "$NAMESPACE" ] && NAMESPACE="nuwax-rcoder-$ENV"
[ -z "$THIRDPARTY_REGISTRY" ] && THIRDPARTY_REGISTRY="$REGISTRY"

if [ "$MODE" = "registry" ] && [ -z "$REGISTRY" ]; then
    echo -e "${RED}错误: registry 模式必须指定 --registry=URL${NC}"; exit 1
fi

echo -e "${GREEN}=========================================="
echo "  RCoder 离线部署"
echo "==========================================${NC}"
echo "  模式:         $MODE"
echo "  环境:         $ENV"
echo "  Release:      $RELEASE_NAME"
echo "  Namespace:    $NAMESPACE"
[ "$MODE" = "registry" ] && echo "  Registry:     $REGISTRY"
[ "$MODE" = "registry" ] && echo "  3rd-party:    $THIRDPARTY_REGISTRY"
echo "  Longhorn:     $([ $SKIP_LONGHORN = 1 ] && echo '跳过' || echo '安装')"
echo "  JuiceFS CSI:  $([ $SKIP_JUICEFS_CSI = 1 ] && echo '跳过' || echo '安装')"
echo ""

# ---- 前置检查 ----
for cmd in kubectl helm; do
    if ! command -v $cmd &>/dev/null; then
        echo -e "${RED}错误: 未安装 $cmd${NC}"; exit 1
    fi
done
if ! kubectl cluster-info &>/dev/null; then
    echo -e "${RED}错误: 无法连接 K8s 集群, 检查 kubeconfig${NC}"; exit 1
fi

# ============================================================================
# 步骤 1: 导入镜像
# ============================================================================
if [ "$SKIP_IMAGE_IMPORT" = "0" ]; then
    echo -e "${GREEN}[1/4] 导入镜像...${NC}"
    IMAGES_TAR="$SCRIPT_DIR/images/all-images.tar"
    if [ ! -f "$IMAGES_TAR" ]; then
        echo -e "${RED}错误: 未找到 $IMAGES_TAR${NC}"; exit 1
    fi

    case "$MODE" in
        direct)
            # 优先使用 k3s ctr, 回退到 nerdctl, 再回退到 ctr
            if command -v k3s &>/dev/null; then
                echo "  使用 k3s ctr 导入 (namespace: k8s.io)..."
                sudo k3s ctr -n k8s.io image import "$IMAGES_TAR"
            elif command -v nerdctl &>/dev/null; then
                echo "  使用 nerdctl 导入..."
                sudo nerdctl --namespace k8s.io load -i "$IMAGES_TAR"
            elif command -v ctr &>/dev/null; then
                echo "  使用 ctr 导入 (namespace: k8s.io)..."
                sudo ctr -n k8s.io image import "$IMAGES_TAR"
            else
                echo -e "${RED}错误: direct 模式需要 k3s / nerdctl / ctr 其中之一${NC}"; exit 1
            fi
            echo -e "${GREEN}  ✅ 镜像已导入节点 containerd${NC}"
            echo -e "${YELLOW}  ⚠️  如果是多节点集群, 需要在每个节点上运行 ctr image import${NC}"
            ;;
        registry)
            if ! command -v docker &>/dev/null; then
                echo -e "${RED}错误: registry 模式需要 docker${NC}"; exit 1
            fi
            echo "  加载到 docker daemon..."
            docker load -i "$IMAGES_TAR"
            echo "  重打 tag 并推送到 $REGISTRY..."
            bash "$SCRIPT_DIR/rewrite-registry.sh" \
                --registry "$REGISTRY" \
                --thirdparty-registry "$THIRDPARTY_REGISTRY" \
                --images-file "$SCRIPT_DIR/images/images.txt"
            ;;
    esac
else
    echo -e "${YELLOW}[1/4] 跳过镜像导入 (--skip-image-import)${NC}"
fi

# ============================================================================
# 步骤 2: 安装 Longhorn
# ============================================================================
echo -e "${GREEN}[2/4] Longhorn...${NC}"
if [ "$SKIP_LONGHORN" = "1" ]; then
    echo "  跳过 (--skip-longhorn)"
elif kubectl get sc longhorn &>/dev/null; then
    echo -e "${GREEN}  ✅ 已安装${NC}"
else
    LONGHORN_YAML=$(ls "$SCRIPT_DIR/longhorn/longhorn-"*.yaml 2>/dev/null | head -1)
    if [ -z "$LONGHORN_YAML" ]; then
        echo -e "${RED}错误: 未找到 longhorn/longhorn-*.yaml${NC}"; exit 1
    fi

    # registry 模式: 先把 manifest 里的 longhornio/* 改成 $THIRDPARTY_REGISTRY/longhornio/*
    APPLY_YAML="$LONGHORN_YAML"
    if [ "$MODE" = "registry" ]; then
        APPLY_YAML="/tmp/longhorn-rewritten.yaml"
        sed -E "s#(image:\s*)longhornio/#\1${THIRDPARTY_REGISTRY}/longhornio/#g" \
            "$LONGHORN_YAML" > "$APPLY_YAML"
    fi

    echo "  部署 Longhorn..."
    kubectl apply -f "$APPLY_YAML"
    echo "  等待 Longhorn manager 就绪 (最长 5 分钟)..."
    kubectl wait --for=condition=ready pod -l app=longhorn-manager \
        -n longhorn-system --timeout=300s || true
    echo -e "${GREEN}  ✅ Longhorn 已安装${NC}"
fi

# ============================================================================
# 步骤 3: 安装 JuiceFS CSI Driver (使用官方 deploy/k8s.yaml, 不走 helm)
# ============================================================================
echo -e "${GREEN}[3/4] JuiceFS CSI Driver...${NC}"
if [ "$SKIP_JUICEFS_CSI" = "1" ]; then
    echo "  跳过 (--skip-juicefs-csi)"
elif kubectl get ds -n kube-system juicefs-csi-driver-node &>/dev/null \
     || kubectl get ds -n kube-system juicefs-csi-node &>/dev/null; then
    echo -e "${GREEN}  ✅ 已安装${NC}"
else
    JUICEFS_YAML=$(ls "$SCRIPT_DIR/juicefs-csi/juicefs-csi-"*.yaml 2>/dev/null | head -1)
    if [ -z "$JUICEFS_YAML" ]; then
        echo -e "${RED}错误: 未找到 juicefs-csi/juicefs-csi-*.yaml${NC}"; exit 1
    fi

    # registry 模式: manifest 里所有 image 前面加上私有 registry 前缀
    APPLY_YAML="$JUICEFS_YAML"
    if [ "$MODE" = "registry" ]; then
        APPLY_YAML="/tmp/juicefs-csi-rewritten.yaml"
        sed -E \
          -e "s#(image:\s*)juicedata/#\1${REGISTRY}/juicedata/#g" \
          -e "s#(image:\s*)registry\.k8s\.io/sig-storage/#\1${THIRDPARTY_REGISTRY}/sig-storage/#g" \
          "$JUICEFS_YAML" > "$APPLY_YAML"
    fi

    echo "  部署 JuiceFS CSI ($(basename $JUICEFS_YAML))..."
    kubectl apply -f "$APPLY_YAML"

    # 设置 mount 镜像 (CE 版本, 与 RCoder 使用的 juicefs 客户端对齐)
    MOUNT_IMG="${THIRDPARTY_REGISTRY:+$THIRDPARTY_REGISTRY/}juicedata/mount:ce-v1.3.1"
    for ds in juicefs-csi-driver-node juicefs-csi-node; do
        kubectl -n kube-system get daemonset/$ds &>/dev/null && \
            kubectl -n kube-system set env daemonset/$ds -c juicefs-plugin \
                JUICEFS_CE_MOUNT_IMAGE="$MOUNT_IMG" \
                JUICEFS_MOUNT_IMAGE="$MOUNT_IMG" >/dev/null 2>&1 || true
    done
    kubectl -n kube-system get statefulset/juicefs-csi-controller &>/dev/null && \
        kubectl -n kube-system set env statefulset/juicefs-csi-controller -c juicefs-plugin \
            JUICEFS_CE_MOUNT_IMAGE="$MOUNT_IMG" \
            JUICEFS_MOUNT_IMAGE="$MOUNT_IMG" >/dev/null 2>&1 || true

    echo "  等待 CSI 就绪 (最长 2 分钟)..."
    for label in "app=juicefs-csi-driver-node" "app=juicefs-csi-node"; do
        kubectl wait --for=condition=ready pod -l "$label" -n kube-system --timeout=120s 2>/dev/null && break || true
    done
    echo -e "${GREEN}  ✅ JuiceFS CSI 已安装${NC}"
fi

# ============================================================================
# 步骤 4: 安装 RCoder
# ============================================================================
echo -e "${GREEN}[4/4] 安装 RCoder...${NC}"

RCODER_CHART=$(ls "$SCRIPT_DIR/charts/rcoder-"*.tgz 2>/dev/null | head -1)
if [ -z "$RCODER_CHART" ]; then
    echo -e "${RED}错误: 未找到 charts/rcoder-*.tgz${NC}"; exit 1
fi

VALUES_ARGS=(-f "$SCRIPT_DIR/values-$ENV.yaml")
if [ "$MODE" = "registry" ]; then
    VALUES_ARGS+=(
        --set "global.imageRegistry=$REGISTRY"
        --set "global.thirdPartyRegistry=$THIRDPARTY_REGISTRY"
    )
fi

echo "  helm upgrade --install $RELEASE_NAME ..."
helm upgrade --install "$RELEASE_NAME" "$RCODER_CHART" \
    --namespace "$NAMESPACE" \
    --create-namespace \
    "${VALUES_ARGS[@]}"

echo -e "${GREEN}  等待 rcoder deployment 就绪 (最长 5 分钟)...${NC}"
kubectl rollout status deploy/rcoder -n "$NAMESPACE" --timeout=300s || true

# ============================================================================
# 收尾
# ============================================================================
echo ""
echo -e "${GREEN}=========================================="
echo "  部署完成!"
echo "==========================================${NC}"
kubectl get pods -n "$NAMESPACE"
echo ""
NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || echo "<node-ip>")
NODE_PORT=$(kubectl get svc rcoder -n "$NAMESPACE" -o jsonpath='{.spec.ports[0].nodePort}' 2>/dev/null || echo "<nodeport>")
echo "访问: curl http://${NODE_IP}:${NODE_PORT}/health"
echo "日志: kubectl logs -n $NAMESPACE -l app=rcoder -f"
echo "卸载: helm uninstall $RELEASE_NAME --namespace $NAMESPACE"
