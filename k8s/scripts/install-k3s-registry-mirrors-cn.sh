#!/usr/bin/env bash
# 为 K3s 配置中国大陆常用的 containerd 镜像加速（docker.io / ghcr.io）
# 用法（在 K3s 节点上）:
#   sudo k8s/scripts/install-k3s-registry-mirrors-cn.sh
#
# 可选：禁止回退到 Docker Hub / GHCR 官方地址（对已配置 mirrors 的仓库不再走官方 endpoint）
#   sudo env INSTALL_DISABLE_REGISTRY_FALLBACK=1 k8s/scripts/install-k3s-registry-mirrors-cn.sh
#
# 完成后会重启 k3s，短暂影响调度中的 Pod。

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

if [[ "${EUID:-}" -ne 0 ]]; then
  echo -e "${RED}请使用 root 运行: sudo $0${NC}"
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="${SCRIPT_DIR}/k3s-registries-cn.yaml"
DEST="/etc/rancher/k3s/registries.yaml"

if [[ ! -f "$SRC" ]]; then
  echo -e "${RED}找不到模板: $SRC${NC}"
  exit 1
fi

install -d /etc/rancher/k3s
cp -a "$SRC" "$DEST"
chmod 0644 "$DEST"

echo -e "${GREEN}已写入:${NC} $DEST"
echo -e "${YELLOW}内容预览:${NC}"
cat "$DEST"

# 可选：对已在 registries.yaml 中声明的仓库，不再回退到官方默认 endpoint（见 K3s 文档 Default Endpoint Fallback）
if [[ "${INSTALL_DISABLE_REGISTRY_FALLBACK:-0}" == "1" ]]; then
  install -d /etc/rancher/k3s/config.yaml.d
  DROPIN="/etc/rancher/k3s/config.yaml.d/99-rcoder-disable-default-registry-endpoint.yaml"
  cat >"$DROPIN" <<'EOF'
disable-default-registry-endpoint: true
EOF
  chmod 0644 "$DROPIN"
  echo ""
  echo -e "${GREEN}已写入 K3s 配置片段:${NC} $DROPIN"
  echo -e "${YELLOW}（仅对 registries.yaml 里配置过 mirrors 的仓库生效；避免回退到被墙地址导致长时间卡住）${NC}"
fi

if systemctl is-active --quiet k3s 2>/dev/null; then
  echo ""
  echo -e "${YELLOW}正在重启 k3s 使镜像配置生效...${NC}"
  systemctl restart k3s
  echo -e "${GREEN}k3s 已重启。${NC}"
elif systemctl is-active --quiet k3s-agent 2>/dev/null; then
  echo ""
  echo -e "${YELLOW}正在重启 k3s-agent...${NC}"
  systemctl restart k3s-agent
  echo -e "${GREEN}k3s-agent 已重启。${NC}"
else
  echo -e "${YELLOW}未检测到 k3s / k3s-agent 服务处于 active，请手动重启:${NC}"
  echo "  systemctl restart k3s"
  echo "  # 或 agent 节点:"
  echo "  systemctl restart k3s-agent"
fi

echo ""
echo -e "${YELLOW}说明:${NC} ${GREEN}k3s ctr images pull${NC} 往往 ${RED}不会${NC} 使用 /etc/rancher/k3s/registries.yaml，"
echo "  仍会直连 registry-1.docker.io，因此在国内容易误报「镜像加速没生效」。"
echo -e "  与 kubelet/CRI 一致的正确验证方式是用 ${GREEN}crictl pull${NC}："

RUNTIME_SOCK=""
for s in /run/k3s/containerd/containerd.sock /var/run/k3s/containerd/containerd.sock; do
  if [[ -S "$s" ]]; then
    RUNTIME_SOCK="$s"
    break
  fi
done

if [[ -n "$RUNTIME_SOCK" ]]; then
  echo ""
  echo -e "${GREEN}验证拉取（推荐）:${NC}"
  echo "  sudo crictl --runtime-endpoint unix://${RUNTIME_SOCK} pull docker.io/rancher/mirrored-pause:3.6"
  if command -v crictl &>/dev/null; then
    echo ""
    echo -e "${YELLOW}正在尝试自动拉取 pause 镜像（失败则忽略，你可手动执行上一行）...${NC}"
    if crictl --runtime-endpoint "unix://${RUNTIME_SOCK}" pull docker.io/rancher/mirrored-pause:3.6; then
      echo -e "${GREEN}crictl 拉取成功：镜像加速已对 CRI/kubelet 生效。${NC}"
    else
      echo -e "${RED}crictl 拉取失败，请把完整终端输出保存后排查（勿用 k3s ctr 判断）。${NC}"
    fi
  else
    echo -e "${YELLOW}未安装 crictl，可安装后再验证。${NC}"
  fi
else
  echo ""
  echo -e "${GREEN}验证拉取（推荐）:${NC}"
  echo "  sudo crictl --runtime-endpoint unix:///run/k3s/containerd/containerd.sock pull docker.io/rancher/mirrored-pause:3.6"
fi
