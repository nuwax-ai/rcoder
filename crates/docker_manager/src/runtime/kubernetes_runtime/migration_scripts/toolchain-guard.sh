#!/bin/bash
# 软链完整性校验：迁移 Job 里跑，接在 rclone check 与目录树比对之后（migrate-user.sh 把本文件的
# 内容渲染进 Job 模板）。
#
# 核对：目标卷上悬空的软链，如果在源上同一个位置能解析，就是迁移弄坏的。
# 为什么要单独核对：rclone check 与目录树比对两边用同一套过滤规则，按设计不迁的内容它们看不见；
# 过滤规则误伤时它们照样通过——实测: pnpm 版本切换目录的 node_modules 被排除规则带走, bin/pnpm
# 悬空, check 仍报 0 differences, 迁完用户一装依赖就报 `spawnSync … ENOENT`。
#
# 不算损坏：
#   - 源上同一个位置也解析不了的软链：本来就坏，或者目标在卷外（/usr 之类——这里的文件系统里
#     判断不了）
#   - 源上能解析、但解析到用户目录之外的软链：共享卷上用户目录的旁边还有别的东西（例如顶层的
#     .agent-store），相对路径多退几层就出去了。这种软链在用户容器里从来就不通——容器的根
#     就是用户目录
#   - 位置在按设计不迁的缓存里（各级 .cache、npm 的 _npx）
#
# 用法: toolchain-guard.sh <源目录> <目标目录> [<相对路径>...]
#   给了相对路径，只核对这些路径之下的软链（补迁之后用）；不给就核对整棵目标卷。
# 退出码: 0=完好  1=有损坏（逐条打印到 stdout，前缀 [verify]）  2=没能核对
set -uo pipefail

[ "$#" -ge 2 ] || { echo "用法: toolchain-guard.sh <源目录> <目标目录> [<相对路径>...]" >&2; exit 2; }
# macOS 的 /tmp 是软链，readlink 给的是物理路径：两边先取物理路径，后面按前缀取相对路径
SRC="$(cd "$1" 2>/dev/null && pwd -P)" || { echo "[verify] 读不了源目录: $1" >&2; exit 2; }
DST="$(cd "$2" 2>/dev/null && pwd -P)" || { echo "[verify] 读不了目标目录: $2" >&2; exit 2; }
shift 2

# 待核对的软链：目标卷上（或给定范围之内）的软链。缓存不进——按设计不迁，里面的软链注定悬空。
candidates="$(mktemp)" || exit 2
trap 'rm -f "$candidates"' EXIT
walk_links() {  # $@=起始目录
  find "$@" \( -name .cache -o -path "*/.npm/_npx" \) -prune -o -type l -print0
}
if [ "$#" -gt 0 ]; then
  for rel in "$@"; do
    case "/$rel/" in
      //*) echo "[verify] 相对路径不能以 / 开头: $rel" >&2; exit 2 ;;
      */../*) echo "[verify] 相对路径不能带 ..: $rel" >&2; exit 2 ;;
    esac
    # `.` = 整棵树（从 $DST 起走, 报出来的才是干净的相对路径）。目标卷上没有的目录 = 没补到
    # 东西（源上也没有），无从核对
    if [ "$rel" = "." ]; then
      walk_links "$DST"
    elif [ -d "$DST/$rel" ]; then
      walk_links "$DST/$rel"
    fi
  done >"$candidates" || exit 2
else
  walk_links "$DST" >"$candidates" || exit 2
fi

links=0 broken=0
report() { echo "[verify] $*"; broken=$((broken + 1)); }

while IFS= read -r -d '' path; do
  links=$((links + 1))
  [ -e "$path" ] && continue                      # 目标卷上能解析
  rel="${path#"$DST"/}"
  [ -e "$SRC/$rel" ] || continue                  # 源上也解析不了：本来就坏，或目标在卷外
  case "$(readlink -f "$SRC/$rel")" in
    "$SRC"/*) ;;
    *) continue ;;                                # 源上解析到了用户目录之外：在容器里本来就不通
  esac
  report "软链坏了: $rel -> $(readlink "$path")"
done <"$candidates"

echo "[verify] 软链完整性：核对软链 $links 条，损坏 $broken 处"
[ "$broken" -eq 0 ] || exit 1
