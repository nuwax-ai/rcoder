#!/bin/bash
# ============================================================================
# start-up-k8s-extra.sh: K8s 部署特有
# ============================================================================
# /home/user 是 PVC subPath (rcoder-computer-workspace/{user_id}), CephFS RWX。
# 文件权限由 pod securityContext.fsGroup 管理 (rcoder 创建 pod 时设置),
# 容器内无需 chown/chmod。Docker Compose 的权限修复在这里跳过。

fix_mount_permissions() {
    # K8s: /home/user 是 PVC, fsGroup 处理 owner, 跳过 chown/chmod
    log "K8s: skip mount permissions fix (/home/user is PVC subPath, fsGroup handles ownership)"
}
