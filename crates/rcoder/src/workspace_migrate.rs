//! 阶段3 lazy mv: 共享 PVC 数据 → per-agent subvolume (经挂根 rename, 瞬间 + 无重复)。
//!
//! rcoder 挂 CephFS 根 (/app/cephfs-root), 同 mount 内 rename 是 MDS 元数据操作,
//! 瞬间完成, 不复制数据。move 后共享 PVC 该子目录自动空 (无重复, 无需清理)。
//! 配额 enforce 在 write 非 rename → mv 不触发配额拒绝。
//!
//! 幂等: per-agent 目标非空 → 已迁移跳过; 共享源不存在 → 新项目跳过。
//! 回滚: 反向 mv (per-agent → 共享), 数据不丢。

use std::path::PathBuf;
use std::sync::Arc;

use container_runtime_api::ContainerRuntime;
use shared_types::ServiceType;
use tracing::{debug, info, warn};

/// resolve per-agent dst 聚合路径, 等 PVC Bound (重试)。
///
/// UserApp `create_deployment` 不等 Pod Ready, PVC 刚 ensure 可能未 Bound
/// (WaitForFirstConsumer 需 Pod 调度)。重试几秒等 ceph-csi provision 完成。
/// Web/Computer `create_container` 等 Pod Ready, 首次即 Bound, 不实际重试。
async fn resolve_dst_with_retry(
    runtime: &Arc<dyn ContainerRuntime>,
    identifier: &str,
    service_type: &ServiceType,
) -> Option<String> {
    const MAX_RETRIES: u32 = 6;
    for attempt in 0..MAX_RETRIES {
        match runtime.resolve_workspace_path(identifier, service_type).await {
            Ok(Some(base)) => return Some(base),
            Ok(None) => return None, // Docker 模式 (runtime 不提供聚合视角)
            Err(e) => {
                if attempt + 1 < MAX_RETRIES {
                    debug!(
                        "[MIGRATE] dst resolve {} pending (PVC 可能未 Bound), 重试 {}/{}: {}",
                        identifier,
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                } else {
                    warn!(
                        "[MIGRATE] dst resolve {} 重试 {} 次后仍失败 (PVC 未 Bound?), 跳过迁移: {}",
                        identifier, MAX_RETRIES, e
                    );
                    return None;
                }
            }
        }
    }
    None
}

/// 经挂根把共享 PVC 子目录数据 mv 到 per-agent subvolume (首次懒迁移, 瞬间无重复)。
///
/// - `shared_pvc_env`: 共享 PVC 名的 env var (`RCODER_WORKSPACE_PVC_NAME` / `RCODER_COMPUTER_WORKSPACE_PVC_NAME`)。
/// - `shared_subpath`: 共享 subvol 下的子路径段 (web=`["workspace"]`, computer=`[]`, UserApp=`["workspace","apps"]`)。
/// - `identifier`/`service_type`: per-agent PVC (resolve_workspace_path)。
/// - `leaf`: 共享 subpath 下的源目录名 (= project_id / user_id / app_id)。
/// - `dst_at_root`: true=dst 用 per-agent subvol 根 (computer, 吸收 user_id); false=dst=base/leaf。
///
/// env 未设 / runtime 无聚合视角 (Docker 模式) → 静默跳过 (兼容现状, 不破坏非 K8s/未启用场景)。
/// mv 失败只 warn 不阻断 (数据仍在共享, agent 启动空 PVC, 可重试)。
pub async fn lazy_migrate(
    runtime: &Arc<dyn ContainerRuntime>,
    shared_pvc_env: &str,
    shared_subpath: &[&str],
    identifier: &str,
    service_type: &ServiceType,
    leaf: &str,
    dst_at_root: bool,
) {
    // 共享 PVC 名 (env); 未设 → Docker 模式或未启用挂根聚合, 跳过
    let Some(shared_pvc) = std::env::var(shared_pvc_env)
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    // per-agent dst base: {cephfs_root}/{per-agent-subvol}; 等 PVC Bound
    // (UserApp create_deployment 不等 Pod Ready, PVC 可能刚 ensure 未 Bound → 重试;
    //  Web/Computer create_container 等 Pod Ready, 首次即 Bound, 不实际重试)
    let dst_base = match resolve_dst_with_retry(runtime, identifier, service_type).await {
        Some(base) => PathBuf::from(base),
        None => return, // Docker 模式或 PVC 长时间未 Bound, 跳过
    };
    let dst = if dst_at_root {
        dst_base // computer: PVC 根 (per-user 吸收 user_id)
    } else {
        dst_base.join(leaf) // web/UserApp: {subvol}/{leaf}
    };
    // 幂等: dst 非空 → 已迁移, 跳过
    if let Ok(mut rd) = tokio::fs::read_dir(&dst).await
        && rd.next_entry().await.ok().flatten().is_some()
    {
        return;
    }
    // 共享 src: {cephfs_root}/{shared-subvol}/{subpath}/{leaf}
    let src_base = match runtime.resolve_workspace_path_by_pvcname(&shared_pvc).await {
        Ok(Some(base)) => PathBuf::from(base),
        _ => return,
    };
    let mut src = src_base;
    for s in shared_subpath {
        src = src.join(s);
    }
    src = src.join(leaf);
    // src 不存在 → 新项目 (无旧数据), 跳过
    if tokio::fs::metadata(&src).await.is_err() {
        return;
    }
    // mv (经挂根同 mount rename, 瞬间 MDS 元数据, 无重复; 共享子目录自动空)
    match tokio::fs::rename(&src, &dst).await {
        Ok(()) => info!(
            "[MIGRATE] lazy mv {} -> {} ({} {})",
            src.display(),
            dst.display(),
            service_type,
            identifier
        ),
        Err(e) => warn!(
            "[MIGRATE] lazy mv failed {} -> {}: {} (agent 将启动; 数据仍在共享, 下次 ensure 重试)",
            src.display(),
            dst.display(),
            e
        ),
    }
}
