#!/bin/bash
# ============================================================================
# 应用运行时镜像构建脚本
# ============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REGISTRY="${DOCKER_REGISTRY:-}"
TAG="${DOCKER_TAG:-latest}"

# 颜色输出
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# 构建基础镜像
build_base() {
    log_info "Building base image: app-runtime-base:${TAG}"
    docker build -t app-runtime-base:${TAG} -f Dockerfile .
}

# 构建语言运行时镜像
build_language() {
    local lang=$1
    local dockerfile="Dockerfile.${lang}"

    if [ ! -f "${dockerfile}" ]; then
        log_error "Dockerfile not found: ${dockerfile}"
        return 1
    fi

    local image_name="app-runtime-${lang}:${TAG}"
    log_info "Building ${image_name}..."
    docker build -t ${image_name} -f ${dockerfile} .

    # 如果配置了 registry，推送到 registry
    if [ -n "${REGISTRY}" ]; then
        local full_image="${REGISTRY}/${image_name}"
        log_info "Tagging and pushing to ${full_image}"
        docker tag ${image_name} ${full_image}
        docker push ${full_image}
    fi
}

# 显示帮助
show_help() {
    echo "Usage: $0 [OPTIONS] [LANGUAGES...]"
    echo ""
    echo "Options:"
    echo "  --registry URL    Docker registry URL"
    echo "  --tag TAG         Image tag (default: latest)"
    echo "  --all             Build all language runtimes"
    echo "  --help            Show this help"
    echo ""
    echo "Languages:"
    echo "  java              Java runtime"
    echo "  python            Python runtime"
    echo "  node              Node.js/TypeScript runtime"
    echo "  go                Go runtime"
    echo "  rust              Rust runtime"
    echo ""
    echo "Examples:"
    echo "  $0 java python         # Build Java and Python runtimes"
    echo "  $0 --all               # Build all runtimes"
    echo "  $0 --registry my-registry.com --tag v1.0.0 java"
}

# 主函数
main() {
    local languages=()
    local build_all=false

    # 解析参数
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --registry)
                REGISTRY="$2"
                shift 2
                ;;
            --tag)
                TAG="$2"
                shift 2
                ;;
            --all)
                build_all=true
                shift
                ;;
            --help)
                show_help
                exit 0
                ;;
            *)
                languages+=("$1")
                shift
                ;;
        esac
    done

    # 构建基础镜像
    build_base

    # 构建语言运行时
    if [ "$build_all" = true ]; then
        languages=(java python node go rust)
    fi

    if [ ${#languages[@]} -eq 0 ]; then
        log_warn "No languages specified. Use --all to build all runtimes."
        show_help
        exit 1
    fi

    for lang in "${languages[@]}"; do
        build_language "$lang"
    done

    log_info "Build completed!"
}

main "$@"
