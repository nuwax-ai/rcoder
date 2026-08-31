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

SRC_DIR=/app/src
BIN_PATH=/app/bin/rcoder

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
# tokio-console 恒编入（独立 target-console 目录——RUSTFLAGS 与普通缓存指纹
# 不同，隔离避免交替全量重编；恒定后不再切换，无重编代价）。启用与否是
# 运行期 DEV_CONSOLE env（默认关），经 `make console-on/off` 重建容器切换，
# 功能切换不再触发重编。宿主机连 localhost:6669。
echo "🔨 cargo build --release --bin rcoder --features console（tokio-console 恒编入；独立 target）..."
export RUSTFLAGS="--cfg tokio_unstable"
export CARGO_TARGET_DIR="$SRC_DIR/target-console"
cargo build --release --bin rcoder --features console
BIN_SRC="$CARGO_TARGET_DIR/release/rcoder"
# start-rcoder.sh 优先用 /app/src/target/release/rcoder——必须清掉另一模式的
# 旧产物，否则新二进制（/app/bin/rcoder）被跳过（8/19 陈旧产物事故同款坑）
rm -f "$SRC_DIR/target/release/rcoder"

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
