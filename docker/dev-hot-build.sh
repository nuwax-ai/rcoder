#!/bin/bash
# ============================================================================
# rcoder 容器内热编译（本地开发测试用）
# ============================================================================
# 用途：改 Rust 源码后，在 rcoder 容器内增量编译 rcoder binary 并替换运行版，
#       替代 make dev-restart（全量重建镜像，10+ 分钟）。
#
# 调用：make dev-hot（= docker exec rcoder-rcoder-1 bash /app/src/docker/dev-hot-build.sh
#                       + docker restart rcoder-rcoder-1）
#
# 前提：docker-compose.yml 已挂载源码 ..:/app/src + cargo/target 缓存 volume
#       （make dev-restart 一次应用该挂载，之后即可反复 dev-hot）
#
# 首次较慢：补装 build 依赖 + cargo 全量编译；后续增量秒级。
# ============================================================================
set -euo pipefail

# 默认使用容器路径；契约测试可指向私有临时目录，避免替换共享开发环境。
SRC_DIR="${RCODER_DEV_HOT_SRC_DIR:-/app/src}"
BIN_PATH="${RCODER_DEV_HOT_BIN_PATH:-/app/bin/rcoder}"

echo "🔥 rcoder 容器内热编译"

# 1. 补装 build 依赖（dev-master-rcoder 运行镜像缺 cmake/protoc；幂等）
#    rcoder 依赖 tonic（gRPC → protoc/libprotobuf）+ duckdb（→ cmake）
if ! command -v protoc >/dev/null 2>&1; then
    echo "📦 首次补装 build 依赖 (cmake / protobuf-compiler / libprotobuf-dev)..."
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends cmake protobuf-compiler libprotobuf-dev pkg-config
else
    echo "✅ build 依赖已就绪 (protoc 存在)"
fi

# 2. 校验源码已挂载
if [ ! -f "$SRC_DIR/Cargo.toml" ]; then
    echo "❌ $SRC_DIR/Cargo.toml 不存在——源码未挂载" >&2
    echo "   请先 make dev-restart 应用 docker-compose.yml 的源码挂载" >&2
    exit 1
fi

# 3. 增量编译 rcoder binary（release；cargo target volume 持久化 → 增量）
cd "$SRC_DIR"
# dial9 默认编入（使用 target-unstable——tokio_unstable RUSTFLAGS 与普通
# 缓存指纹不同；未选 dial9 时使用普通 target，避免交替全量重编）。
# 启用与否是运行期 DIAL9_ENABLED env（默认关闭录制），经
# `make dial9-on/off` 重建容器切换，功能切换不触发重编。
# trace 落 /app/logs/dial9（compose 挂载 → 宿主 docker/logs/dial9），离线
# `make dial9-view` 查看。hotpath 同默认编入（本地 dev 默认观测；docker
# restart 的 SIGTERM → graceful shutdown 自动落报告，见 AGENTS.md「AI 调试
# 路由」）。
#
# feature 透传（specs observability T2.4）：$1 接受 make dev-hot 注入的
# CARGO_FEATURES（"--features a,b,c" 或裸逗号列表），与 dev-restart 的构建
# feature 集保持一致——dev-restart 选了 hotpath-mcp 等附加 feature 时热编译
# 不得静默丢弃。未传参数回落 hotpath,dial9；显式空参数保留 Cargo 默认集。
FEATURES_ARG="${1-hotpath,dial9}"
case "$FEATURES_ARG" in
    --features\ *) FEATURES_ARG="${FEATURES_ARG#--features }" ;;
esac
CARGO_ARGS=(build --release --bin rcoder)
if [ -n "$FEATURES_ARG" ]; then
    CARGO_ARGS+=(--features "$FEATURES_ARG")
fi
# 同 dev-restart：只有 dial9 构建需要 Tokio hooks。保留调用方已有的 flags，
# 识别逗号/空白分隔的 feature 与 crate/dial9，避免按名字子串误启用。
export RUSTFLAGS="${RUSTFLAGS:-}"
DIAL9_FEATURE_PATTERN='(^|[,[:space:]])([^,[:space:]]*/)?dial9([,[:space:]]|$)'
if [[ "$FEATURES_ARG" =~ $DIAL9_FEATURE_PATTERN ]]; then
    export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--cfg tokio_unstable"
    # Cargo gives encoded flags precedence, including an explicitly empty
    # value. Preserve caller flags while enabling hooks through that input too.
    if [ "${CARGO_ENCODED_RUSTFLAGS+x}" ]; then
        if [ -n "$CARGO_ENCODED_RUSTFLAGS" ]; then
            CARGO_ENCODED_RUSTFLAGS+=$'\x1f'
        fi
        export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}--cfg"$'\x1f'"tokio_unstable"
    fi
    export CARGO_TARGET_DIR="$SRC_DIR/target-unstable"
else
    export CARGO_TARGET_DIR="$SRC_DIR/target"
fi
echo "🔨 cargo ${CARGO_ARGS[*]}（target: $CARGO_TARGET_DIR）..."
cargo "${CARGO_ARGS[@]}"
BIN_SRC="$CARGO_TARGET_DIR/release/rcoder"
# target directories are compilation caches only; start-rcoder.sh always uses
# the container-local executable atomically installed below.

# 4. 替换运行 binary
if [ ! -f "$BIN_SRC" ]; then
    echo "❌ 编译产物 $BIN_SRC 未生成" >&2
    exit 1
fi
# 原子替换：直接 cp 覆盖正在运行的 binary 会 ETXTBSY（Text file busy），
# 改为 cp 到临时文件 + mv（rename(2) 不受 ETXTBSY 限制；旧进程持旧 inode 继续跑，
# 新进程用新文件）。docker restart 后拉起新 binary。
cp "$BIN_SRC" "$BIN_PATH.new"
chmod +x "$BIN_PATH.new"
mv -f "$BIN_PATH.new" "$BIN_PATH"

echo "✅ 热编译完成: $BIN_PATH 已更新"
echo "👉 进程重启由 make dev-hot 的 docker restart 步骤完成"
