#!/bin/bash
# ============================================================
# RCoder K8s 开发环境部署脚本 (Kustomize)
# 用于日常开发测试验证
# ============================================================

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

NAMESPACE="${NAMESPACE:-nuwax-rcoder-dev}"
KUSTOMIZE_DIR="${KUSTOMIZE_DIR:-./manifests/overlays/dev}"

echo -e "${GREEN}=========================================="
echo "  RCoder K8s 开发环境部署 (Kustomize)"
echo "==========================================${NC}"

# ============================================================
# 步骤 1: 检查 K8s 集群
# ============================================================
echo ""
echo -e "${GREEN}[1/6] 检查 K8s 集群...${NC}"

if ! command -v kubectl &> /dev/null; then
    echo -e "${RED}Error: kubectl not found${NC}"
    echo "请安装 kubectl: https://kubernetes.io/docs/tasks/tools/"
    exit 1
fi

if ! kubectl cluster-info &> /dev/null; then
    echo -e "${RED}Error: 无法连接到 K8s 集群${NC}"
    echo ""
    echo -e "${YELLOW}请先部署 K3s 集群 (中国镜像):${NC}"
    echo ""
    echo "  curl -sfL https://rancher-mirror.rancher.cn/k3s/k3s-install.sh | INSTALL_K3S_MIRROR=cn sh -"
    echo ""
    echo "  # 安装完成后, 配置 kubectl:"
    echo "  mkdir -p ~/.kube"
    echo "  sudo cp /etc/rancher/k3s/k3s.yaml ~/.kube/config"
    echo "  chmod 600 ~/.kube/config"
    echo ""
    echo "  # 重新运行此脚本"
    exit 1
fi

echo -e "${GREEN}✅ K8s 集群连接正常${NC}"
kubectl get nodes

# ============================================================
# 步骤 1.5: 检查/安装 open-iscsi (Longhorn 依赖)
# ============================================================
echo ""
echo -e "${GREEN}[1.5/6] 检查 open-iscsi 依赖...${NC}"

install_open_iscsi() {
    echo -e "${YELLOW}正在检测操作系统...${NC}"

    # 检测操作系统类型
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        OS_ID="$ID"
        OS_VERSION="$VERSION_ID"
    else
        OS_ID="unknown"
    fi

    echo -e "${YELLOW}检测到操作系统: $OS_ID${NC}"

    case "$OS_ID" in
        ubuntu|debian)
            echo -e "${YELLOW}安装 open-iscsi...${NC}"
            apt-get update -qq
            apt-get install -y -qq open-iscsi > /dev/null 2>&1
            ;;
        centos|rhel|rocky|almalinux)
            echo -e "${YELLOW}安装 iscsi-initiator-utils...${NC}"
            yum install -y -q iscsi-initiator-utils > /dev/null 2>&1
            ;;
        sles|opensuse-leap|opensuse-tumbleweed)
            echo -e "${YELLOW}安装 open-iscsi...${NC}"
            zypper install -y -q open-iscsi > /dev/null 2>&1
            ;;
        *)
            echo -e "${RED}不支持的操作系统: $OS_ID${NC}"
            echo -e "${YELLOW}请手动安装 open-iscsi 或 iscsi-initiator-utils${NC}"
            return 1
            ;;
    esac

    echo -e "${YELLOW}启动 iscsid 服务...${NC}"
    systemctl enable --now iscsid 2>/dev/null || true
    systemctl enable --now iscsid.socket 2>/dev/null || true

    return 0
}

# 检查 iscsiadm 是否存在
if command -v iscsiadm &> /dev/null; then
    echo -e "${GREEN}✅ open-iscsi 已安装: $(iscsiadm --version 2>&1 | head -1)${NC}"
else
    echo -e "${YELLOW}⚠️  open-iscsi 未安装${NC}"
    echo -e "${YELLOW}Longhorn 需要 open-iscsi 来提供 iSCSI 存储${NC}"

    # 尝试自动安装
    if [ "$EUID" -eq 0 ]; then
        install_open_iscsi
        if [ $? -eq 0 ]; then
            echo -e "${GREEN}✅ open-iscsi 安装成功${NC}"
        fi
    else
        echo -e "${YELLOW}请使用 sudo 运行此脚本，或手动安装:${NC}"
        echo -e "  ${CYAN}Ubuntu/Debian: sudo apt install open-iscsi${NC}"
        echo -e "  ${CYAN}CentOS/RHEL:   sudo yum install iscsi-initiator-utils${NC}"
        echo -e "  ${CYAN}SUSE:          sudo zypper install open-iscsi${NC}"
        echo ""
        echo -e "${YELLOW}安装完成后，重新运行此脚本${NC}"
        exit 1
    fi
fi

# 验证 iscsiadm 可用
if ! command -v iscsiadm &> /dev/null; then
    echo -e "${RED}❌ open-iscsi 安装失败${NC}"
    exit 1
fi

echo -e "${GREEN}✅ open-iscsi 检查完成${NC}"

# ============================================================
# 步骤 2: 部署 Longhorn 存储
# ============================================================
echo ""
echo -e "${GREEN}[2/6] 检查/部署 Longhorn 存储...${NC}"

if kubectl get sc longhorn &> /dev/null; then
    echo -e "${GREEN}✅ Longhorn 已安装${NC}"
else
    echo -e "${YELLOW}Longhorn 未安装，正在部署...${NC}"
    kubectl apply -f https://raw.githubusercontent.com/longhorn/longhorn/master/deploy/longhorn.yaml
    echo -e "${GREEN}⏳ 等待 Longhorn 就绪...${NC}"

    # 等待 Longhorn Manager 就绪
    kubectl wait --for=condition=ready pod -l app=longhorn-manager \
        -n longhorn-system --timeout=300s 2>/dev/null || true

    # 等待 Longhorn UI 就绪
    kubectl wait --for=condition=ready pod -l app=longhorn-ui \
        -n longhorn-system --timeout=300s 2>/dev/null || true

    echo -e "${GREEN}✅ Longhorn 部署完成${NC}"
fi

kubectl get sc | grep longhorn || echo -e "${YELLOW}Warning: Longhorn StorageClass 未就绪${NC}"

# ============================================================
# 步骤 3: 部署 JuiceFS CSI
# ============================================================
echo ""
echo -e "${GREEN}[3/6] 检查/部署 JuiceFS CSI Driver...${NC}"

if kubectl get ds juicefs-csi-driver-node -n kube-system &> /dev/null; then
    echo -e "${GREEN}✅ JuiceFS CSI Driver 已安装${NC}"
else
    echo -e "${YELLOW}JuiceFS CSI Driver 未安装，正在部署...${NC}"

    if ! command -v helm &> /dev/null; then
        echo -e "${YELLOW}Helm 未安装，跳过 JuiceFS CSI 部署${NC}"
        echo "请手动安装: helm repo add juicefs https://juicefs.github.io/charts"
    else
        helm repo add juicefs https://juicefs.github.io/charts 2>/dev/null || true
        helm repo update
        helm install juicefs-csi-driver juicefs/juicefs-csi-driver \
            --namespace kube-system \
            --set webhook.enabled=false

        echo -e "${GREEN}⏳ 等待 JuiceFS CSI Driver 就绪...${NC}"
        kubectl wait --for=condition=ready pod -l app=juicefs-csi-driver-node \
            -n kube-system --timeout=120s 2>/dev/null || true

        echo -e "${GREEN}✅ JuiceFS CSI Driver 部署完成${NC}"
    fi
fi

# ============================================================
# 步骤 4: 使用 Kustomize 部署 RCoder
# ============================================================
echo ""
echo -e "${GREEN}[4/6] 部署 RCoder 到 namespace: $NAMESPACE${NC}"

# 检查 kubectl kustomize 插件
if ! kubectl kustomize --help &> /dev/null; then
    echo -e "${RED}Error: kubectl kustomize 插件未找到 (需要 kubectl 1.14+)${NC}"
    exit 1
fi

# 部署
kubectl apply -k "$KUSTOMIZE_DIR"

echo -e "${GREEN}✅ RCoder 部署完成${NC}"

# ============================================================
# 步骤 5: 验证部署
# ============================================================
echo ""
echo -e "${GREEN}[5/6] 验证部署状态...${NC}"

echo ""
echo "--- Pods ---"
kubectl get pods -n "$NAMESPACE"

echo ""
echo "--- Services ---"
kubectl get svc -n "$NAMESPACE"

echo ""
echo "--- StorageClass ---"
kubectl get sc | grep -E "longhorn|juicefs"

echo ""
echo "--- PVC ---"
kubectl get pvc -n "$NAMESPACE"

# 获取访问信息
NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || echo "localhost")
SVC_TYPE=$(kubectl get svc rcoder -n "$NAMESPACE" -o jsonpath='{.spec.type}' 2>/dev/null)

echo ""
echo -e "${GREEN}=========================================="
echo "  部署完成!"
echo "==========================================${NC}"
echo ""
echo -e "访问方式:"

if [ "$SVC_TYPE" = "NodePort" ]; then
    NODE_PORT=$(kubectl get svc rcoder -n "$NAMESPACE" -o jsonpath='{.spec.ports[0].nodePort}')
    echo -e "  ${GREEN}http://${NODE_IP}:${NODE_PORT}/health${NC}"
    echo -e "  ${GREEN}http://${NODE_IP}:${NODE_PORT}/chat${NC}"
elif [ "$SVC_TYPE" = "LoadBalancer" ]; then
    EXT_IP=$(kubectl get svc rcoder -n "$NAMESPACE" -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "<pending>")
    echo -e "  ${GREEN}http://${EXT_IP}:8087/health${NC}"
else
    echo -e "  ${GREEN}kubectl port-forward svc/rcoder 8087:8087 -n $NAMESPACE${NC}"
    echo -e "  然后访问: ${GREEN}http://localhost:8087/health${NC}"
fi

echo ""
echo -e "Kustomize 管理命令:"
echo -e "  ${YELLOW}kubectl apply -k $KUSTOMIZE_DIR${NC}      # 部署/更新"
echo -e "  ${YELLOW}kubectl delete -k $KUSTOMIZE_DIR${NC}      # 删除"
echo -e "  ${YELLOW}kubectl get all -n $NAMESPACE${NC}          # 查看状态"
echo ""
echo -e "Longhorn 控制台:"
echo -e "  ${YELLOW}kubectl port-forward svc/longhorn-frontend 8080:80 -n longhorn-system${NC}"
echo -e "  访问: ${GREEN}http://localhost:8080${NC}"
echo ""
