#!/bin/bash
# ============================================================================
# start-up-common.sh: Docker Compose / K8s 通用引导
# ============================================================================
# 由 start-up.sh 在日志函数定义后 source。
# 根据 DEPLOY_MODE 加载对应的 extra, 提供部署差异函数:
#   - fix_mount_permissions: 修复 /home/user 挂载目录权限
#     * Docker Compose (bind mount): 宿主机会改坏 owner, 需 chown/chmod
#     * K8s (PVC subPath): fsGroup 已管理权限, no-op
#
# DEPLOY_MODE 由 rcoder 创建容器时注入:
#   - K8s:    crates/docker_manager/src/runtime/kubernetes_runtime.rs (env_vars)
#   - Docker: crates/docker_manager/src/agent_container_starter.rs (env 循环)
# ============================================================================

case "${DEPLOY_MODE:-docker}" in
    k8s|kubernetes)
        if [ -f /usr/local/bin/start-up-k8s-extra.sh ]; then
            # shellcheck source=start-up-k8s-extra.sh
            source /usr/local/bin/start-up-k8s-extra.sh
        else
            log_warn "DEPLOY_MODE=k8s 但 start-up-k8s-extra.sh 不存在, fix_mount_permissions 退化为 no-op"
        fi
        ;;
    docker|compose|*)
        if [ -f /usr/local/bin/start-up-docker-extra.sh ]; then
            # shellcheck source=start-up-docker-extra.sh
            source /usr/local/bin/start-up-docker-extra.sh
        fi
        ;;
esac

# 兜底: extra 未 source 或未定义 fix_mount_permissions 时, 提供 no-op (永不阻塞启动)
if ! type fix_mount_permissions >/dev/null 2>&1; then
    fix_mount_permissions() { :; }
fi
