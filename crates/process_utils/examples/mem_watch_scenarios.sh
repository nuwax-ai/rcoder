#!/usr/bin/env bash
# mem_watch_scenarios 的宿主侧驱动（需要 Docker）。
#
#   bash crates/process_utils/examples/mem_watch_scenarios.sh [rust 镜像，默认 rust:1.99.1]
#
# 可信目录在 `mounted` 场景里是只读挂载，容器内写不了，只能由宿主侧改：本脚本
# 把一个临时目录只读挂到 /etc/agent-runtime，并按程序打印的 SCN-SET 行改写其中
# 的文件——对应线上运维改 ConfigMap、kubelet 同步进容器。
#
# 构建容器只读挂源码、target 放 docker volume：不改仓库，也不占宿主的 target。
set -euo pipefail

image="${1:-rust:1.99.1}"
repo="$(cd "$(dirname "$0")/../../.." && pwd)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
target_volume=mem-watch-scn-target
bin=/t/debug/examples/mem_watch_scenarios

# 镜像里的 cargo 用宿主的 registry/git 缓存（不挂整个 CARGO_HOME：里面有不进容器的凭据）。
caches=()
for dir in registry git; do
  [ -d "$cargo_home/$dir" ] && caches+=(-v "$cargo_home/$dir:/cargo/$dir")
done

docker volume create "$target_volume" >/dev/null
# 构建容器不限内存：编译本身就要几个 GB。默认离线，缺依赖时设 CARGO_NET_OFFLINE=false。
docker run --rm -v "$repo:/w:ro" -v "$target_volume:/t" "${caches[@]}" \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/t -e CARGO_NET_OFFLINE=true -w /w "$image" \
  cargo build -p process_utils --examples --locked

run() { docker run --rm --memory=512m -v "$target_volume:/t" "$image" "$bin" "$@"; }

echo "== unmounted: 可信目录没有只读挂载 =="
run unmounted

echo "== mounted: 可信目录是宿主侧目录的只读挂载 =="
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
docker run --rm -i --memory=512m -v "$stage:/etc/agent-runtime:ro" \
  -v "$target_volume:/t" "$image" "$bin" mounted | while IFS= read -r line; do
  case "$line" in
    "SCN-SET trusted=off") printf 'mode=off\n' >"$stage/mem-watch.conf" ;;
    "SCN-SET trusted=dry") printf 'mode=dry\n' >"$stage/mem-watch.conf" ;;
    "SCN-SET trusted=absent") rm -f "$stage/mem-watch.conf" ;;
  esac
  printf '%s\n' "$line"
done
