#!/bin/bash
set -e

# etcd 安装脚本
# 用法: ./install-etcd.sh [版本号]
# 示例: ./install-etcd.sh v3.5.17

ETCD_VERSION="${1:-v3.5.17}"

# 颜色输出
GREEN='\033[0;32m'
NC='\033[0m'

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

# 检测系统架构
detect_arch() {
    local arch=$(uname -m)
    case $arch in
        x86_64)
            echo "amd64"
            ;;
        aarch64|arm64)
            echo "arm64"
            ;;
        *)
            echo "不支持的架构: $arch" >&2
            exit 1
            ;;
    esac
}

# 检测操作系统
detect_os() {
    local os=$(uname -s | tr '[:upper:]' '[:lower:]')
    case $os in
        linux)
            echo "linux"
            ;;
        darwin)
            echo "darwin"
            ;;
        *)
            echo "不支持的操作系统: $os" >&2
            exit 1
            ;;
    esac
}

main() {
    local arch=$(detect_arch)
    local os=$(detect_os)

    log_info "安装 etcd ${ETCD_VERSION} (${os}-${arch})..."

    # 构建下载 URL
    local url="https://github.com/etcd-io/etcd/releases/download/${ETCD_VERSION}/etcd-${ETCD_VERSION}-${os}-${arch}.tar.gz"

    log_info "下载地址: ${url}"

    # 下载并解压
    curl -fsSL "${url}" | tar xz --strip-components=1 -C /usr/local/bin/

    # 验证安装
    if etcd --version; then
        log_info "etcd 安装成功"
    else
        echo "etcd 安装失败" >&2
        exit 1
    fi

    # 清理
    rm -rf /tmp/etcd-*
}

main "$@"
