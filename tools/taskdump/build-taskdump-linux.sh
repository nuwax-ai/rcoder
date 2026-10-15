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
#   1. 校验：Linux、源/诊断目录无交叠（防误改开发源目录）
#   2. rsync 源码到诊断副本（排除 target/.git 等重目录），记录 source SHA/uname
#   3. 仅改【副本】根 Cargo.toml 的 dial9 依赖：features 加 "taskdump"
#   4. 仅在【副本】cargo update -p dial9 --precise 0.5.2 更新锁图，
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
SRC_DIR="${1:-${RCODER_TASKDUMP_SRC:-$SRC_DIR_DEFAULT}}"
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
command -v python3 >/dev/null || { echo "❌ 需要 python3" >&2; exit 1; }
command -v cargo >/dev/null || { echo "❌ 需要 cargo" >&2; exit 1; }
# 先解析尚未创建的目录和符号链接；source 或 target 的既有链接也不能
# 把复制/构建指回开发仓库或其他目录。所有检查先于 mkdir/rsync。
PATHS=$(python3 - "$SRC_DIR" "$DIAG_DIR" <<'PYEOF'
import os
import sys
from pathlib import Path

source, diagnostic = (Path(os.path.realpath(value)) for value in sys.argv[1:])
def overlaps(left, right):
    return left == right or left in right.parents or right in left.parents
if not source.is_dir() or not (source / "Cargo.toml").is_file() or not (source / "Cargo.lock").is_file():
    sys.exit(f"❌ 源码目录需要 Cargo.toml 和 Cargo.lock: {source}")
if overlaps(source, diagnostic):
    sys.exit("❌ 源码与诊断目录不能相同，也不能互为父子目录")
for name in ("Cargo.toml", "Cargo.lock"):
    if (source / name).is_symlink():
        sys.exit(f"❌ 待修改文件不能是符号链接: {source / name}")
for name in ("source", "target-taskdump", "taskdump-build-meta.txt"):
    target = diagnostic / name
    resolved = Path(os.path.realpath(target))
    if target.is_symlink() or resolved.parent != diagnostic or overlaps(source, resolved):
        sys.exit(f"❌ 诊断路径指向目录外部或开发源码: {target}")
target_root = diagnostic / "target-taskdump"
for name in ("release", "release/deps", "release/build", "release/incremental",
             "release/.fingerprint", "release/examples", "release/rcoder"):
    target = target_root / name
    resolved = Path(os.path.realpath(target))
    inside_target = resolved == target_root or target_root in resolved.parents
    if target.is_symlink() and (not inside_target or overlaps(source, resolved)):
        sys.exit(f"❌ 构建目标链接指向独立 target 外部或开发源码: {target}")
artifact = target_root / "release/rcoder"
source_artifact = source / "rcoder"
if artifact.is_file() and source_artifact.is_file() and os.path.samefile(artifact, source_artifact):
    sys.exit(f"❌ 构建产物与开发源码文件共享 inode: {artifact}")
if any("\n" in str(value) for value in (source, diagnostic)):
    sys.exit("❌ 目录路径不能包含换行")
print(source)
print(diagnostic)
PYEOF
)
SRC_DIR="${PATHS%%$'\n'*}"
DIAG_DIR="${PATHS#*$'\n'}"
mkdir -p "$DIAG_DIR"

# ---- 2. 复制源码 + 记录身份 ------------------------------------------------
echo "📦 复制 $SRC_DIR → $DIAG_DIR/source/"
mkdir -p "$DIAG_DIR/source"
if command -v rsync >/dev/null 2>&1; then
  rsync -a --delete \
    --exclude 'target*' --exclude '.git' --exclude '.remote-k8s' \
    --exclude 'specs' --exclude 'node_modules' --exclude '.devspace' \
    "$SRC_DIR"/ "$DIAG_DIR/source/"
else
  # 最小镜像用已有 Python 依赖复制，包含隐藏文件清理；不在同一 tar
  # 管道中边读边删除，也不把旧副本的隐藏配置留在新构建里。
  python3 - "$SRC_DIR" "$DIAG_DIR/source" <<'PYEOF'
import shutil
import sys
from pathlib import Path

source, destination = map(Path, sys.argv[1:])
shutil.rmtree(destination)
shutil.copytree(source, destination, symlinks=True, ignore=shutil.ignore_patterns(
    "target*", ".git", ".remote-k8s", "specs", "node_modules", ".devspace",
))
PYEOF
fi
# rsync 的 quick-check 会保留内容/属性相同的既有硬链接。复制完成后，
# 为将要改写的根 manifest/lock 分别创建独立 inode，避免修改原仓或旧副本。
python3 - "$DIAG_DIR/source" <<'PYEOF'
import os
import shutil
import stat
import sys
import tempfile
from pathlib import Path

root = Path(sys.argv[1])
for name in ("Cargo.toml", "Cargo.lock"):
    path = root / name
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode):
        sys.exit(f"❌ 待修改的副本文件必须是常规文件，不能是符号链接: {path}")
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=root, prefix=f".{name}.", delete=False) as output:
            temporary = Path(output.name)
            with path.open("rb") as original:
                shutil.copyfileobj(original, output)
            os.fchmod(output.fileno(), stat.S_IMODE(metadata.st_mode))
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        detached = path.lstat()
        if not stat.S_ISREG(detached.st_mode) or detached.st_nlink != 1:
            sys.exit(f"❌ 副本文件未形成独立常规文件: {path}")
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
PYEOF
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
echo "🔒 副本 cargo update -p dial9 --precise 0.5.2（taskdump 引入 backtrace 等边）..."
cargo update -p dial9 --precise 0.5.2
LOCKED_DIAL9=$(awk '/^name = "dial9"$/{getline; sub(/^version = "/,""); sub(/"$/,""); print; exit}' Cargo.lock)
if [ "$LOCKED_DIAL9" != "0.5.2" ]; then
  echo "❌ 副本 dial9 锁定版本漂移: $LOCKED_DIAL9（要求 0.5.2，方案固定基线）" >&2
  exit 1
fi
echo "✅ 副本 dial9 锁定 0.5.2"

# ---- 5. 独立 target 构建（tokio_unstable 指纹隔离）-------------------------
echo "🔨 构建 rcoder（dial9+taskdump, tokio_unstable, 独立 target）..."
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--cfg tokio_unstable"
# Cargo 优先读取 encoded 变量（即使显式为空）；也为该入口补 cfg，保留
# 调用方既有标志，防止产物静默变成没有 spawn hook 的稳定构建。
if [ "${CARGO_ENCODED_RUSTFLAGS+x}" ]; then
  if [ -n "$CARGO_ENCODED_RUSTFLAGS" ]; then
    CARGO_ENCODED_RUSTFLAGS+=$'\x1f'
  fi
  export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}--cfg"$'\x1f'"tokio_unstable"
fi
export CARGO_TARGET_DIR="$DIAG_DIR/target-taskdump"
cargo build --release -p rcoder --bin rcoder --features dial9 --locked

BIN="$CARGO_TARGET_DIR/release/rcoder"
[ -f "$BIN" ] || { echo "❌ 构建产物缺失: $BIN" >&2; exit 1; }

cat <<EOF

✅ taskdump 诊断构建完成
   binary : $BIN
   source : ${SRC_DIR}（SHA 见 $DIAG_DIR/taskdump-build-meta.txt）
   运行示例:
     DIAL9_ENABLED=1 DIAL9_TASK_DUMP_ENABLED=1 \\
     DIAL9_TRACE_DIR=$DIAG_DIR/traces \\
     $BIN <rcoder 启动参数>
   录到 dump 后解码（宿主 macOS 亦可）:
     rcoder/tools/dial9-probe --expect-dump $DIAG_DIR/traces
EOF
