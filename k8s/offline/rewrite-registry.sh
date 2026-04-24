#!/bin/bash
# ============================================================================
# 把 docker daemon 里已加载的镜像 re-tag 并推送到私有 registry
#
# 输入: images.txt (一行一个镜像)
# 行为:
#   - RCoder 自有镜像 (nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/*)
#     -> 推到 $REGISTRY/{name}:{tag}
#   - 其他 (postgres / minio / juicedata / sig-storage / longhornio 等)
#     -> 推到 $THIRDPARTY_REGISTRY/{repo}:{tag}
#
# 依赖: docker (镜像已 load 在本地 daemon)
# ============================================================================
set -e

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'

REGISTRY=""
THIRDPARTY_REGISTRY=""
IMAGES_FILE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --registry) REGISTRY="$2"; shift 2 ;;
        --thirdparty-registry) THIRDPARTY_REGISTRY="$2"; shift 2 ;;
        --images-file) IMAGES_FILE="$2"; shift 2 ;;
        *) echo "未知参数: $1"; exit 1 ;;
    esac
done

[ -z "$REGISTRY" ] && { echo -e "${RED}--registry 必填${NC}"; exit 1; }
[ -z "$THIRDPARTY_REGISTRY" ] && THIRDPARTY_REGISTRY="$REGISTRY"
[ -z "$IMAGES_FILE" ] && { echo -e "${RED}--images-file 必填${NC}"; exit 1; }
[ ! -f "$IMAGES_FILE" ] && { echo -e "${RED}未找到 $IMAGES_FILE${NC}"; exit 1; }

# RCoder 自有镜像的 registry 前缀 (用于识别 + 剥离)
RCODER_OWN_PREFIX="nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/"

echo -e "${GREEN}Re-tag + push 镜像到私有 registry${NC}"
echo "  自有镜像 -> $REGISTRY/"
echo "  第三方   -> $THIRDPARTY_REGISTRY/"

while IFS= read -r line; do
    # 跳过注释和空行
    line="${line%%#*}"; line="${line##[[:space:]]}"; line="${line%%[[:space:]]}"
    [ -z "$line" ] && continue

    SRC="$line"
    DST=""

    if [[ "$SRC" == "$RCODER_OWN_PREFIX"* ]]; then
        # 自有镜像: 剥离前缀, 推到 REGISTRY
        STRIPPED="${SRC#$RCODER_OWN_PREFIX}"
        DST="$REGISTRY/$STRIPPED"
    else
        # 第三方: 保留完整 repo 路径, 推到 THIRDPARTY_REGISTRY
        # 处理 registry.k8s.io/sig-storage/xxx —— 去掉 registry.k8s.io/ 保留 sig-storage/xxx
        REPO="$SRC"
        case "$REPO" in
            registry.k8s.io/*) REPO="${REPO#registry.k8s.io/}" ;;
            docker.io/*)       REPO="${REPO#docker.io/}" ;;
            quay.io/*)         REPO="${REPO#quay.io/}" ;;
        esac
        DST="$THIRDPARTY_REGISTRY/$REPO"
    fi

    echo -e "${YELLOW}  $SRC${NC}"
    echo "    => $DST"
    docker tag "$SRC" "$DST"
    docker push "$DST"
done < "$IMAGES_FILE"

echo -e "${GREEN}✅ 所有镜像已推送到私有 registry${NC}"
