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

use container_runtime_api::WorkspaceRuntime;
use shared_types::ServiceType;
use tracing::{debug, info, warn};

/// resolve per-agent dst 聚合路径, 等 PVC Bound (重试)。
///
/// UserApp `create_deployment` 不等 Pod Ready, PVC 刚 ensure 可能未 Bound
/// (WaitForFirstConsumer 需 Pod 调度)。重试等 ceph-csi provision 完成 (典型秒级,
/// 慢调度可达 30s+)。Web/Computer `create_container` 等 Pod Ready, 首次即 Bound, 不实际重试。
async fn resolve_dst_with_retry(
    runtime: &Arc<dyn WorkspaceRuntime>,
    identifier: &str,
    service_type: &ServiceType,
) -> Option<String> {
    const MAX_RETRIES: u32 = 30;
    for attempt in 0..MAX_RETRIES {
        match runtime
            .resolve_workspace_path(identifier, service_type)
            .await
        {
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
///
/// **dst_at_root=true (Computer)**: **逐子项 rename** (遍历 src `{user_id}` 子项, 逐个 rename 到
/// per-user PVC 根, skip 已存在)。原因: Computer agent 启动会装 `acp-agent` 到 PVC 根 (`/home/user`),
/// rename 整个 `{user_id} → PVC 根` 会 ENOTEMPTY; 逐子项迁项目 `{cid}` (不存在 → 成功), agent 装的
/// acp-agent 已存在自动 skip。Web/UserApp 不受影响 (单 leaf rename, dst 不存在, 不碰 agent 写)。
///
/// # 类型 (ISP 阶段3)
/// 取 `Arc<dyn WorkspaceRuntime>` (按值): lazy_migrate 只用 workspace 方法 (resolve),
/// 不需 agent/UserApp 能力。调用方传 `Arc<dyn ContainerRuntime>` 时, Rust trait upcasting
/// (1.86+) 自动把 `Arc<dyn ContainerRuntime>` → `Arc<dyn WorkspaceRuntime>` (super-trait coercion).
/// 按值而非 `&Arc` 是为绕过 `&Arc<dyn Sub>` → `&Arc<dyn Super>` 不自动 coercible 的限制
/// (借用背后的临时值生命周期有歧义). Arc::clone 是原子计数, 廉价.
pub async fn lazy_migrate(
    runtime: Arc<dyn WorkspaceRuntime>,
    shared_pvc_env: &str,
    shared_subpath: &[&str],
    identifier: &str,
    service_type: &ServiceType,
    leaf: &str,
    dst_at_root: bool,
) {
    // 回滚开关 false → 跳过迁移 (共享 PVC 模式)
    if !shared_types::per_agent_pvc_enabled() {
        return;
    }
    // 共享 PVC 名 (env); 未设 → Docker 模式或未启用挂根聚合, 跳过
    let Some(shared_pvc) = std::env::var(shared_pvc_env).ok().filter(|s| !s.is_empty()) else {
        return;
    };
    // 共享 src base: {cephfs_root}/{shared-subvol} (共享 PVC 早已 Bound, resolve 快 + cache 命中)
    let src_base = match runtime.resolve_workspace_path_by_pvcname(&shared_pvc).await {
        Ok(Some(base)) => PathBuf::from(base),
        _ => return,
    };
    let mut src = src_base;
    for s in shared_subpath {
        src = src.join(s);
    }
    src = src.join(leaf);
    // src 不存在 → 新项目/无旧数据 (UserApp 新应用常态, 见 application-management-service-v2-design.md;
    // 新 project/user 同理), 早退。提前检查避免新项目白等 dst resolve (per-agent PVC 等 Bound
    // 重试最长 60s), 性能关键 —— 新应用 create_app 不该被迁移逻辑阻塞。
    if tokio::fs::metadata(&src).await.is_err() {
        return;
    }
    // per-agent dst base: {cephfs_root}/{per-agent-subvol}; 等 PVC Bound (仅旧数据迁移才需)
    let dst_base = match resolve_dst_with_retry(&runtime, identifier, service_type).await {
        Some(base) => PathBuf::from(base),
        None => return, // Docker 模式或 PVC 长时间未 Bound, 跳过 (数据仍在共享)
    };

    if dst_at_root {
        // Computer: 逐子项迁移 (src/{user_id}/* → dst/{subvol}/* = per-user PVC 根)
        migrate_children(&src, &dst_base, identifier).await;
        return;
    }

    // Web/UserApp: 单 leaf 迁移 (src → dst={subvol}/{leaf})
    let dst = dst_base.join(leaf);
    // 幂等: dst/.migrated 存在 → 已迁移, 跳过 (防 copy 中途失败后半 copy 误判)
    let marker = dst.join(".migrated");
    if tokio::fs::try_exists(&marker).await.unwrap_or(false) {
        return;
    }
    // 迁移: 先试 rename (同 subvolume 快), EXDEV → fallback copy+remove (跨 subvolume)
    match tokio::fs::rename(&src, &dst).await {
        Ok(()) => {
            if let Err(e) = tokio::fs::write(&marker, b"1").await {
                warn!(
                    "[MIGRATE] rename 后写 marker {} 失败: {} (下次 ensure 会重判)",
                    marker.display(),
                    e
                );
            }
            info!(
                "[MIGRATE] rename {} -> {} ({} {})",
                src.display(),
                dst.display(),
                service_type,
                identifier
            );
        }
        Err(e) if e.raw_os_error() == Some(18) => {
            // EXDEV: 跨 CephFS subvolume → copy + remove (shell mv 行为)
            match copy_dir_recursive(&src, &dst).await {
                Ok(()) => {
                    if let Err(e) = tokio::fs::write(&marker, b"1").await {
                        warn!(
                            "[MIGRATE] copy 后写 marker {} 失败: {} (下次 ensure 会重判)",
                            marker.display(),
                            e
                        );
                    }
                    if let Err(e) = tokio::fs::remove_dir_all(&src).await {
                        warn!(
                            "[MIGRATE] copy 后删除源 {} 失败: {} (数据已复制, 源残留)",
                            src.display(),
                            e
                        );
                    }
                    info!(
                        "[MIGRATE] copy+remove {} -> {} ({} {})",
                        src.display(),
                        dst.display(),
                        service_type,
                        identifier
                    );
                }
                Err(e) => warn!(
                    "[MIGRATE] copy failed {} -> {}: {} (agent 将启动; 数据仍在共享, 下次 ensure 重试)",
                    src.display(),
                    dst.display(),
                    e
                ),
            }
        }
        Err(e) => warn!(
            "[MIGRATE] rename failed {} -> {}: {} (agent 将启动; 数据仍在共享, 下次 ensure 重试)",
            src.display(),
            dst.display(),
            e
        ),
    }
}

/// Computer 逐子项迁移: 遍历 src (`{shared}/{user_id}`) 子项, 逐个迁移到 dst (per-user PVC 根)。
/// rename EXDEV 时 fallback copy+remove (跨 CephFS subvolume)。
/// skip 已存在 (agent 启动装的 `acp-agent` 等)。幂等: dst/.migrated 存在 → 全量 skip。
async fn migrate_children(src: &std::path::Path, dst: &std::path::Path, identifier: &str) {
    // 幂等: dst/.migrated → 已迁移
    let marker = dst.join(".migrated");
    if tokio::fs::try_exists(&marker).await.unwrap_or(false) {
        return;
    }
    let mut rd = match tokio::fs::read_dir(src).await {
        Ok(rd) => rd,
        Err(_) => return, // src 不存在 → 新 user (无旧数据), 跳过
    };
    let mut migrated = 0u32;
    let mut skipped = 0u32;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let src_item = entry.path();
        let dst_item = dst.join(entry.file_name());
        if tokio::fs::try_exists(&dst_item).await.unwrap_or(false) {
            skipped += 1;
            continue;
        }
        // rename, EXDEV → copy + remove
        match tokio::fs::rename(&src_item, &dst_item).await {
            Ok(()) => migrated += 1,
            Err(e) if e.raw_os_error() == Some(18) => {
                match copy_dir_recursive(&src_item, &dst_item).await {
                    Ok(()) => {
                        if let Err(e) = tokio::fs::remove_dir_all(&src_item).await {
                            warn!(
                                "[MIGRATE] 子项 copy 后删除源 {} 失败: {} (数据已复制, 源残留)",
                                src_item.display(),
                                e
                            );
                        }
                        migrated += 1;
                    }
                    Err(e) => warn!(
                        "[MIGRATE] copy {} -> {} failed: {} (skip, 继续其他子项)",
                        src_item.display(),
                        dst_item.display(),
                        e
                    ),
                }
            }
            Err(e) => warn!(
                "[MIGRATE] rename {} -> {} failed: {} (skip, 继续其他子项)",
                src_item.display(),
                dst_item.display(),
                e
            ),
        }
    }
    if migrated > 0 || skipped > 0 {
        if let Err(e) = tokio::fs::write(&marker, b"1").await {
            warn!(
                "[MIGRATE] 逐子项迁移后写 marker {} 失败: {} (下次 ensure 会重判)",
                marker.display(),
                e
            );
        }
        info!(
            "[MIGRATE] {} 逐子项迁移 (迁 {}, skip {}): {} -> {}",
            identifier,
            migrated,
            skipped,
            src.display(),
            dst.display()
        );
    }
}

/// 递归 copy 目录树 (纯 tokio::fs, 无外部依赖)。
/// 用于 rename EXDEV 时 fallback (跨 CephFS subvolume) + 批量迁移。
async fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut rd = tokio::fs::read_dir(src).await?;
    while let Some(entry) = rd.next_entry().await? {
        let src_item = entry.path();
        let dst_item = dst.join(entry.file_name());
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            Box::pin(copy_dir_recursive(&src_item, &dst_item)).await?;
        } else if ft.is_file() {
            tokio::fs::copy(&src_item, &dst_item).await?;
        } else if ft.is_symlink() {
            let target = tokio::fs::read_link(&src_item).await?;
            #[cfg(unix)]
            tokio::fs::symlink(&target, &dst_item).await?;
        }
    }
    Ok(())
}

/// pub wrapper for batch_migrate (bin 模块调用, lib 编译看不到 → allow dead_code)
#[allow(dead_code)]
pub(crate) async fn copy_dir_recursive_pub(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> std::io::Result<()> {
    copy_dir_recursive(src, dst).await
}
