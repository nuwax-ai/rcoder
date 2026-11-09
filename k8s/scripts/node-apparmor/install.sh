#!/usr/bin/env bash
# 节点 AppArmor 初始化：加载放行容器内 signal 的 Localhost profile。
#
# 解决的问题：Ubuntu 24.04+/新内核上，containerd 给容器挂的
# cri-containerd.apparmor.d（enforce）拒绝容器内进程互发信号——
# 新内核标签叠加后，父子进程标签为 cri-containerd.apparmor.d//&unconfined，
# 与默认规则的 peer=cri-containerd.apparmor.d 精确匹配失败 → kill() 返回 EPERM。
# 内存看门狗（及任何需要 kill 子进程的容器工具）会静默失效。
#
# 本脚本不修改/卸载已有 profile，只是额外加载一个新 profile：
#   cri-containerd-signal = 默认规则 + 追加 3 条 signal peer 放行
#
# 用法（节点上 root 执行）：
#   ./install.sh              # 加载 profile 并写入 /etc/apparmor.d（持久化）
#   ./install.sh --verify     # 仅验证当前容器能否发信号（无需加载）
#
# Pod 侧配套：
#   securityContext:
#     appArmorProfile: {type: Localhost, localhostProfile: cri-containerd-signal}
set -euo pipefail

SELF_DIR=$(cd "$(dirname "$0")" && pwd)
PROFILE_SRC="$SELF_DIR/cri-containerd-signal"
PROFILE_DST=/etc/apparmor.d/cri-containerd-signal
PROFILE_NAME=cri-containerd-signal

if [ "${1:-}" = "--verify" ]; then
  # 行为探测：当前环境能否给子进程发信号（最直接、不受 aa-status 权限限制）
  python3 - <<'PY'
import os, subprocess, sys, time
p = subprocess.Popen(["sleep", "5"])
time.sleep(0.3)
try:
    os.kill(p.pid, 15)
    print("SIGNAL_OK: 容器内可向子进程发信号，看门狗可工作")
    sys.exit(0)
except PermissionError:
    print("SIGNAL_DENIED: AppArmor/LSM 拒绝发信号——看门狗会静默失效")
    print("hint: dmesg | grep 'operation=\"signal\"'；加载 cri-containerd-signal profile")
    sys.exit(1)
PY
  exit $?
fi

[ "$(id -u)" -eq 0 ] || { echo "请用 root 运行" >&2; exit 1; }
command -v apparmor_parser >/dev/null || { echo "缺少 apparmor_parser（apt install apparmor）" >&2; exit 1; }

echo "== 1/3 校验 profile 语法 =="
apparmor_parser -Q "$PROFILE_SRC"   # -Q 只解析不加载

echo "== 2/3 加载到内核 =="
apparmor_parser -Kr "$PROFILE_SRC"
grep -q "$PROFILE_NAME" /sys/kernel/security/apparmor/profiles \
  && echo "内核已加载: $PROFILE_NAME" \
  || { echo "警告：内核未列出 $PROFILE_NAME" >&2; exit 1; }

echo "== 3/3 持久化到 /etc/apparmor.d（重启后自动加载）=="
install -m 0644 "$PROFILE_SRC" "$PROFILE_DST"

cat <<'TIP'

完成。Pod 侧启用方式（二选一，全局搜索 appArmorProfile 确认覆盖）：
  securityContext:
    appArmorProfile: {type: Localhost, localhostProfile: cri-containerd-signal}

行为探测（建议在目标 pod 内执行一次）：
  ./install.sh --verify   # 在节点执行可探节点默认形态；pod 内执行可探 pod 实际状态
TIP
