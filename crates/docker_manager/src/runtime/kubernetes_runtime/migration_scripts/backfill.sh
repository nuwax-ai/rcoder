#!/bin/bash
# 补迁：往已切换的目标卷上从源只补缺失的文件——不覆盖、不删除（rclone copy --ignore-existing）。
# 迁移 Job 里跑，两边是同一个用户的 home：源 /src/<user> 只读，目标 /dst 可写。两个调用方渲染的
# 是同一份文件——k8s/migration 的 migrate-user.sh（job 模式）和 rcoder 的补迁接口
# （docker_manager 的 migration_scripts/backfill.sh，逐字拷贝；改了一边要同步另一边）。
#
# 补的是"按给定规则该迁、而目标卷上没有"的文件。过滤规则与 sync 共用，重跑幂等，也不会把缓存
# 带回来。目标卷上已有的文件一律不动——切换之后用户改过、新建的东西原样保留；反过来，切换之后
# 用户删掉、源上（切换时刻）还有的文件会被补回来——所以范围要选得准：
#   - 缺省、`*` 都只看源与目标都有的顶层目录，切换后用户删掉的目录不会复活；
#   - --only 能把范围再收窄到某几类条目（例如旧排除规则带走的 node_modules、dist）；范围之内不匹配的
#     不拷，目标卷上也不会多出空目录；
#   - `.`（整棵树）不带 --only 时，切换后删掉的东西都会回来，只在明确要这样做时用。
#
# 用法: backfill.sh <源目录> <目标目录> [<过滤规则>...] -- [--only <模式>]... [<相对路径>|<范围>]...
#   过滤规则带 +/- 号（rclone --filter 的写法），排在最前（rclone 先匹配先生效）；
#   --only <模式>   只补匹配该模式的条目（模式相对范围之下任意层级；可重复给）。不给 = 范围之下全部
#   相对路径省略     = home 顶层点目录里源与目标都有的（工具链状态：pnpm 的版本切换目录与全局包、
#                    编辑器扩展…；`.cache` 除外）
#   `*`            = home 顶层非点开头的目录（项目）里源与目标都有的
#   `.`            = 整棵树
#
# 每次拷贝都有上限：目标卷可用空间减去余量（卷大小的 5%，至少 4GiB）。到了上限 rclone 会停下来
# （退出码 8），不会把用户的卷写满——写满了容器里的 PG、agent 都会跟着出问题。余量留得比可能超出
# 的部分大：到了上限 rclone 要过一小会儿才停，在传的几个文件还会再写进去一些。
#
# 退出码: 0=完成  1=有路径没拷成功或空间不够  2=参数不对
# stdout: 每个拷过的相对路径一行 `[backfill] copied: <相对路径>`，调用方据此确定校验范围；
#        源上没有、跳过的范围一行 `[backfill] skipped: <相对路径>`
set -uo pipefail

[ "$#" -ge 2 ] || { echo "用法: backfill.sh <源目录> <目标目录> [<过滤规则>...] -- [--only <模式>]... [<相对路径>|<范围>]..." >&2; exit 2; }
SRC="$(cd "$1" 2>/dev/null && pwd -P)" || { echo "[backfill] 读不了源目录: $1" >&2; exit 2; }
DST="$(cd "$2" 2>/dev/null && pwd -P)" || { echo "[backfill] 读不了目标目录: $2" >&2; exit 2; }
shift 2

# 过滤规则每条一个参数；范围的放行由下面逐路径追加，再以 `- *` 兜住其余
rclone_args=(--links --metadata --ignore-existing
             --transfers 8 --checkers 8 --stats-one-line --stats 60s)
while [ "${1:-}" != "--" ]; do
  [ "$#" -gt 0 ] || { echo "[backfill] 参数里缺少分隔符 --" >&2; exit 2; }
  rclone_args+=(--filter "$1")
  shift
done
shift

only=()
while [ "${1:-}" = "--only" ]; do
  [ "$#" -ge 2 ] || { echo "[backfill] --only 后面要跟一个模式" >&2; exit 2; }
  only+=("$2")
  shift 2
done

# 不带 --only：范围之下整个都要，源上的空目录也算数（rclone 默认不建空目录）。
# 带 --only：不建空目录——否则 rclone 会把源上的目录骨架在目标卷上建出来（旧规则排除的 dist/、
# 切换之后用户删掉的子目录都会以空目录的形式回来），与"只补匹配的条目"不符；匹配的条目所在的
# 目录随文件一起建。
if [ "${#only[@]}" -eq 0 ]; then rclone_args+=(--create-empty-src-dirs); fi

# 顶层目录里源与目标都有的。$1=dots（点目录，`.cache` 按设计不迁）| projects（非点开头的目录）。
# Job 脚本关着 glob，这里是固定模式，临时打开。
top_level_dirs() {
  local mode="$1" d name had_glob=0
  if [[ $- == *f* ]]; then had_glob=1; fi
  set +f
  if [ "$mode" = dots ]; then set -- "$SRC"/.[!.]*; else set -- "$SRC"/*; fi
  for d; do
    if [ ! -d "$d" ]; then continue; fi
    name="${d##*/}"
    case "$name" in
      .cache) if [ "$mode" = dots ]; then continue; fi ;;
      .*) if [ "$mode" != dots ]; then continue; fi ;;
      *) if [ "$mode" != projects ]; then continue; fi ;;
    esac
    if [ ! -d "$DST/$name" ]; then continue; fi
    printf '%s\n' "$name"
  done
  if [ "$had_glob" = 1 ]; then set -f; fi
  return 0
}

# 把 `*` 换成一个个项目目录；没有给任何路径就是缺省范围（点目录）
expanded=()
explicit=0
for rel in "$@"; do
  if [ "$rel" = "*" ]; then
    explicit=1
    while IFS= read -r name; do
      [ -n "$name" ] && expanded+=("$name")
    done < <(top_level_dirs projects)
  else
    explicit=1
    expanded+=("$rel")
  fi
done
if [ "$explicit" = 0 ]; then
  while IFS= read -r rel; do
    [ -n "$rel" ] && expanded+=("$rel")
  done < <(top_level_dirs dots)
fi

# 一个范围的放行规则（每条一行）。没有 --only: 范围之下全部。有 --only: 范围之下任意层级匹配
# 某个模式的条目——要给两条，`范围/模式` 管正好在范围根下的，`范围/**/模式` 管更深层的。
scope_rules() {
  local rel="$1" cat
  if [ "${#only[@]}" -eq 0 ]; then
    if [ "$rel" = "." ]; then printf '+ **\n'; else printf '+ %s/**\n' "$rel"; fi
    return 0
  fi
  for cat in "${only[@]}"; do
    if [ "$rel" = "." ]; then
      printf '+ %s\n+ **/%s\n' "$cat" "$cat"
    else
      printf '+ %s/%s\n+ %s/**/%s\n' "$rel" "$cat" "$rel" "$cat"
    fi
  done
  return 0
}

# 这次拷贝最多能写多少字节：目标卷可用空间减去余量（卷大小的 5%，至少 4GiB）
transfer_budget() {
  local free_kib size_kib reserve_kib
  free_kib="$(df -Pk "$DST" | awk 'NR == 2 { print $4 }')"
  size_kib="$(df -Pk "$DST" | awk 'NR == 2 { print $2 }')"
  reserve_kib=$(( size_kib / 20 ))
  if [ "$reserve_kib" -lt 4194304 ]; then reserve_kib=4194304; fi
  echo $(( (free_kib - reserve_kib) * 1024 ))
}

copied=0 skipped=0
for rel in "${expanded[@]}"; do
  case "/$rel/" in
    //*) echo "[backfill] 相对路径不能以 / 开头: $rel" >&2; exit 2 ;;
    */../*) echo "[backfill] 相对路径不能带 ..: $rel" >&2; exit 2 ;;
  esac
  if [ ! -e "$SRC/$rel" ]; then
    echo "[backfill] skipped: $rel"
    skipped=$((skipped + 1))
    continue
  fi
  # 始终以用户目录为根：过滤规则是相对它写的（`.npm/_npx/`），换根就对不上了
  scope_args=()
  while IFS= read -r rule; do
    scope_args+=(--filter "$rule")
  done < <(scope_rules "$rel")
  scope_args+=(--filter '- *')
  budget="$(transfer_budget)"
  if [ "$budget" -le 0 ]; then
    echo "[backfill] 目标卷剩余空间不够（可用空间已低于要留的余量）: ${rel}。扩容后重跑 backfill" >&2
    exit 1
  fi
  echo "[backfill] copy: $rel"
  rc=0
  rclone copy "$SRC" "$DST" "${rclone_args[@]}" --max-transfer "$budget" "${scope_args[@]}" || rc=$?
  case $rc in
    0) ;;
    8) echo "[backfill] 目标卷剩余空间不够（已按上限停下，没有写满卷）: ${rel}。扩容后重跑 backfill" >&2; exit 1 ;;
    *) echo "[backfill] 没拷成功（rclone 退出码 ${rc}）: ${rel}" >&2; exit 1 ;;
  esac
  echo "[backfill] copied: $rel"
  copied=$((copied + 1))
done
echo "[backfill] 完成：拷了 $copied 个路径（跳过 $skipped 个）；只补缺失，未覆盖、未删除"
