#!/bin/bash
# ============================================================================
# taskdump 诊断构建（observability P5 / specs T5.1）
#
# 用途：在 Linux 构建一个带 dial9 taskdump 的 rcoder 诊断二进制。
#       taskdump 需要 dial9/taskdump → tokio/taskdump（仅 Linux x86_64/aarch64）
#       且要求 RUSTFLAGS="--cfg tokio_unstable"——该约束与常规缓存/CI 指纹互斥，
#       因此【绝不进主仓 feature 面】（--all-features 门禁，见 AGENTS.md 第 7 节），
#       只以本脚本 + 独立诊断副本存在，主仓零改动。
#
# 做什么：
#   1. 校验：Linux、与源目录真实路径不同（防误改开发源目录）
#   2. rsync 源码到诊断副本（排除 target/.git 等重目录），记录 source SHA/uname
#   3. 仅改【副本】根 Cargo.toml 的 dial9 依赖：features 加 "taskdump"
#   4. 仅在【副本】cargo update -p dial9 更新锁图（taskdump 引入 backtrace 边），
#      校验 dial9 仍为 0.5.2，之后 --locked
#   5. RUSTFLAGS="--cfg tokio_unstable" 独立 target-taskdump 目录构建
#
# 用法（Linux 容器/主机内）：
#   bash tools/taskdump/build-taskdump-linux.sh [源码目录] [诊断目录]
#   默认：源码目录=$RCODER_TASKDUMP_SRC 或脚本内 SRC_DIR_DEFAULT；
#         诊断目录=$HOME/diag/rcoder-taskdump
#
# 运行产物：
#   $DIAG_DIR/target-taskdump/release/rcoder
#   运行期：DIAL9_ENABLED=1 DIAL9_TASK_DUMP_ENABLED=1 \
#          DIAL9_TRACE_DIR=$DIAG_DIR/traces <binary>
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_DIR_DEFAULT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SRC_DIR="$(realpath "${1:-${RCODER_TASKDUMP_SRC:-$SRC_DIR_DEFAULT}}")"
DIAG_DIR="${2:-${HOME}/diag/rcoder-taskdump}"

# ---- 1. 平台与路径护栏（fail fast）----------------------------------------
case "$(uname -s)" in
  Linux) ;;
  *) echo "❌ taskdump 仅 Linux x86_64/aarch64（tokio/taskdump 上游约束），当前: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|aarch64) ;;
  *) echo "❌ 不支持的 arch: $(uname -m)（tokio/taskdump 仅 x86_64/aarch64）" >&2; exit 1 ;;
esac
if [ ! -f "$SRC_DIR/Cargo.toml" ]; then
  echo "❌ 源码目录缺 Cargo.toml: $SRC_DIR" >&2; exit 1
fi
mkdir -p "$DIAG_DIR"
DIAG_DIR="$(realpath "$DIAG_DIR")"
if [ "$SRC_DIR" = "$DIAG_DIR" ]; then
  echo "❌ 诊断目录不得等于源码目录: $DIAG_DIR" >&2; exit 1
fi

# ---- 2. 复制源码 + 记录身份 ------------------------------------------------
echo "📦 复制 $SRC_DIR → $DIAG_DIR/source/"
mkdir -p "$DIAG_DIR/source"
if command -v rsync >/dev/null 2>&1; then
  rsync -a --delete \
    --exclude 'target*' --exclude '.git' --exclude '.remote-k8s' \
    --exclude 'specs' --exclude 'node_modules' --exclude '.devspace' \
    "$SRC_DIR"/ "$DIAG_DIR/source/"
else
  # 无 rsync 的最小镜像：tar 管道复制（排除同上；不增量、整树覆盖）
  (cd "$SRC_DIR" && tar -cf - \
    --exclude='target*' --exclude='.git' --exclude='.remote-k8s' \
    --exclude='specs' --exclude='node_modules' --exclude='.devspace' \
    .) | (cd "$DIAG_DIR/source" && rm -rf ./* && tar -xf -)
fi
{
  echo "source_sha=$(git -C "$SRC_DIR" rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "source_dirty=$(git -C "$SRC_DIR" status --short 2>/dev/null | wc -l | tr -d ' ')"
  echo "built_at=$(date -Is)"
  echo "uname=$(uname -a)"
  echo "src_dir=$SRC_DIR"
} > "$DIAG_DIR/taskdump-build-meta.txt"

# ---- 3. 仅改副本的 dial9 依赖（taskdump 追加，保留 default-features=false）--
COPY_MANIFEST="$DIAG_DIR/source/Cargo.toml"
python3 - "$COPY_MANIFEST" <<'PYEOF'
import sys
path = sys.argv[1]
s = open(path).read()
old = 'dial9 = { version = "0.5", default-features = false, features = ["tokio"] }'
new = 'dial9 = { version = "0.5", default-features = false, features = ["tokio", "taskdump"] }'
if old not in s:
    sys.exit("❌ 副本 Cargo.toml 未找到 dial9 依赖声明原文（上游声明形态变化？拒绝盲改）")
open(path, "w").write(s.replace(old, new, 1))
print("✅ 副本 dial9 依赖已追加 taskdump feature（仅诊断副本）")
PYEOF

cd "$DIAG_DIR/source"

# ---- 4. 副本锁图先更新，校验 dial9 版本未漂移 ------------------------------
echo "🔒 副本 cargo update -p dial9（taskdump 引入 backtrace 等边）..."
cargo update -p dial9
LOCKED_DIAL9=$(awk '/^name = "dial9"$/{getline; sub(/^version = "/,""); sub(/"$/,""); print; exit}' Cargo.lock)
if [ "$LOCKED_DIAL9" != "0.5.2" ]; then
  echo "❌ 副本 dial9 锁定版本漂移: $LOCKED_DIAL9（要求 0.5.2，方案固定基线）" >&2
  exit 1
fi
echo "✅ 副本 dial9 锁定 0.5.2"

# ---- 5. 独立 target 构建（tokio_unstable 指纹隔离）-------------------------
echo "🔨 构建 rcoder（dial9+taskdump, tokio_unstable, 独立 target）..."
export RUSTFLAGS="--cfg tokio_unstable"
export CARGO_TARGET_DIR="$DIAG_DIR/target-taskdump"
cargo build -p rcoder --bin rcoder --features dial9 --locked

BIN="$CARGO_TARGET_DIR/release/rcoder"
[ -f "$BIN" ] || { echo "❌ 构建产物缺失: $BIN" >&2; exit 1; }

cat <<EOF

✅ taskdump 诊断构建完成
   binary : $BIN
   source : $SRC_DIR（SHA 见 $DIAG_DIR/taskdump-build-meta.txt）
   运行示例:
     DIAL9_ENABLED=1 DIAL9_TASK_DUMP_ENABLED=1 \\
     DIAL9_TRACE_DIR=$DIAG_DIR/traces \\
     $BIN <rcoder 启动参数>
   录到 dump 后解码（宿主 macOS 亦可）:
     rcoder/tools/dial9-probe --expect-dump $DIAG_DIR/traces
EOF
