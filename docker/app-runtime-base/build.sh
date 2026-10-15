#!/bin/bash
# ============================================================================
# 应用运行时镜像构建脚本（base + 统一多语言 runtime）
# ============================================================================
# 产物两个镜像：
#   1. app-runtime-base:${TAG}   —— 基础设施层（PG/dbx/ttyd/supervisor/排查工具），Dockerfile
#   2. app-runtime:${TAG}        —— 统一多语言运行时（Node+Python+Java+Go+Rust），Dockerfile.runtime
# 用户部署 UserApp 只用 app-runtime:${TAG}（任意语言 / workspace 多语言同容器）。
# 原 Dockerfile.{node,python,java,go,rust} 已合并进 Dockerfile.runtime，不再按语言拆镜像。

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
REGISTRY="${DOCKER_REGISTRY:-}"
TAG="${DOCKER_TAG:-latest}"

# 颜色输出
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

log_info()  { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

push_if_registry() {
    local image=$1
    if [ -n "${REGISTRY}" ]; then
        local full="${REGISTRY}/${image}"
        log_info "Tagging and pushing ${full}"
        docker tag "${image}" "${full}"
        docker push "${full}"
    fi
}

# 构建基础设施层镜像
build_base() {
    log_info "Building base image: app-runtime-base:${TAG}"
    docker build -t "app-runtime-base:${TAG}" -f "${SCRIPT_DIR}/Dockerfile" "${REPO_ROOT}"
    push_if_registry "app-runtime-base:${TAG}"
}

# 构建统一多语言运行时镜像
build_runtime() {
    log_info "Building unified runtime: app-runtime:${TAG}"
    docker build -t "app-runtime:${TAG}" -f "${SCRIPT_DIR}/Dockerfile.runtime" "${REPO_ROOT}"
    push_if_registry "app-runtime:${TAG}"
}

show_help() {
    cat <<EOF
Usage: $0 [OPTIONS]

构建 app-runtime-base（基础设施）+ app-runtime（统一多语言运行时）两个镜像。

Options:
  --registry URL    Docker registry URL（设了则构建后 tag + push）
  --tag TAG         镜像 tag（默认 latest）
  --base-only       只构建 base（调试基础设施层时用）
  --runtime-only    只构建 runtime（base 已构建过，跳过）
  --help            显示帮助

Examples:
  $0                          # 构建 base + runtime（不推 registry）
  $0 --registry my-reg.com    # 构建并推 base + runtime
  $0 --runtime-only           # 只重建 runtime（base 未变）
EOF
}

main() {
    local base_only=false
    local runtime_only=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --registry)      REGISTRY="$2"; shift 2 ;;
            --tag)           TAG="$2"; shift 2 ;;
            --base-only)     base_only=true; shift ;;
            --runtime-only)  runtime_only=true; shift ;;
            --help|-h)       show_help; exit 0 ;;
            *)               log_error "Unknown option: $1"; show_help; exit 1 ;;
        esac
    done

    if [ "$base_only" = true ] && [ "$runtime_only" = true ]; then
        log_error "--base-only 和 --runtime-only 互斥"
        exit 1
    fi

    if [ "$runtime_only" = false ]; then
        build_base
    fi
    if [ "$base_only" = false ]; then
        build_runtime
    fi

    log_info "Build completed! (tag=${TAG}, registry=${REGISTRY:-<local only>})"
}

main "$@"
