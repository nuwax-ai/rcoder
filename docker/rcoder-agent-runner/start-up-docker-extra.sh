#!/bin/bash
# ============================================================================
# start-up-docker-extra.sh: Docker Compose 部署特有
# ============================================================================
# /home/user 由宿主机 bind mount, 宿主机 UID 可能改坏文件 owner,
# 导致容器内 user 用户访问失败 (花屏/配置加载失败)。这里修复权限。
# (K8s 部署不 source 本文件, 见 start-up-k8s-extra.sh)

# 修复挂载目录权限 (chown/chmod, 非递归大目录优化, 实测 <40ms)
# 用法: fix_mount_permissions [USER_HOME]  (默认 /home/user)
fix_mount_permissions() {
    local USER_HOME="${1:-/home/user}"

    log "Fixing permissions for mounted directories (Docker Compose, optimized)..."

    # 方案 1: 顶层目录所有权 (非递归, <0.1s)
    chown user:user "$USER_HOME" "$USER_HOME/.config" "$USER_HOME/.cache" "$USER_HOME/Desktop" 2>/dev/null || true

    # 方案 2: XFCE 配置目录 (文件少, 递归 chown+chmod, 让 root 也能读)
    if [ -d "$USER_HOME/.config/xfce4" ]; then
        find "$USER_HOME/.config/xfce4" \( -type f -o -type d \) \
            -exec chown user:user {} + \
            -exec chmod o+rX {} + 2>/dev/null || true
        log_success "  XFCE config ownership and permissions fixed"
    fi

    # 方案 3: Desktop 递归 o+rX + 顶层 chmod = 给 other/root 的【读权限】(非所有权), 后台跑不阻塞 agent_runner exec。
    #   方案1/2 的 chown(所有权) 已同步完成, 桌面 user 启动即可读写自己的文件; 这里的 chmod 仅影响 other 读权限,
    #   桌面渲染图标时才读 Desktop 文件, 后台 ~20ms 会在 xfdesktop 渲染前完成, 无感知。
    (
        [ -d "$USER_HOME/Desktop" ] && chmod -R o+rX "$USER_HOME/Desktop" 2>/dev/null
        for dir in "$USER_HOME/.cache" "$USER_HOME/.local" "$USER_HOME/.config"; do
            [ -d "$dir" ] && chmod o+rX "$dir" 2>/dev/null
        done
    ) &

    log_success "Permissions fixed (Docker Compose - no recursive chown on large dirs)"
}
