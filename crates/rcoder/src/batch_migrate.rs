//! 阶段2 批量迁移: rcoder 启动时后台 task, 将共享 PVC 老数据一次性 copy 到 per-agent PVC。
//!
//! env `RCODER_BATCH_MIGRATE_ON_STARTUP=true` 启用 (默认 false)。
//! 需配套 `RCODER_PER_AGENT_PVC_ENABLED=true` + cephfsRootAggregation + SC Immediate。
//!
//! 流程:
//! 1. resolve 共享 PVC subvolumePath (经挂根)
//! 2. 遍历共享 subvolume /workspace/{projectId} (Web) + /{userId} (Computer)
//! 3. 对每个: ensure_workspace_pvc (SC Immediate 立即 Bound) → resolve per-agent → copy → .migrated marker
//! 4. 不删共享源 (安全; 手动确认后删)

use std::path::PathBuf;
use std::sync::Arc;

use container_runtime_api::ContainerRuntime;
use shared_types::ServiceType;
use tracing::{info, warn};

/// 启动批量迁移后台 task (不阻塞 rcoder 主流程)。
///
/// 仅当 `FeatureFlags.batch_migrate_on_startup=true` 且 `per_agent_pvc=true` 时执行。
pub fn spawn_if_enabled(runtime: Arc<dyn ContainerRuntime>) {
    let flags = shared_types::FeatureFlags::get();
    if !flags.batch_migrate_on_startup {
        return;
    }
    if !flags.per_agent_pvc {
        info!("[BATCH_MIGRATE] per_agent_pvc disabled, skip batch migration");
        return;
    }
    info!("[BATCH_MIGRATE] starting batch migration (background task)");
    tokio::spawn(async move {
        if let Err(e) = run_batch_migrate(&runtime).await {
            warn!("[BATCH_MIGRATE] batch migration failed: {e}");
        }
    });
}

async fn run_batch_migrate(runtime: &Arc<dyn ContainerRuntime>) -> Result<(), String> {
    let mut total_migrated = 0u32;
    let mut total_skipped = 0u32;
    let mut total_failed = 0u32;

    // Web projects: 共享 rcoder-workspace PVC subPath=workspace → /workspace/{projectId}
    total_migrated += migrate_shared_pvc(
        runtime,
        "RCODER_WORKSPACE_PVC_NAME",
        &["workspace"],
        ServiceType::WebAgentRunner,
        false,
        &mut total_skipped,
        &mut total_failed,
    )
    .await;

    // Computer users: 共享 rcoder-computer-workspace PVC → /{userId}
    total_migrated += migrate_shared_pvc(
        runtime,
        "RCODER_COMPUTER_WORKSPACE_PVC_NAME",
        &[],
        ServiceType::ComputerAgentRunner,
        true,
        &mut total_skipped,
        &mut total_failed,
    )
    .await;

    info!(
        "[BATCH_MIGRATE] completed: migrated={}, skipped={}, failed={}",
        total_migrated, total_skipped, total_failed
    );
    Ok(())
}

/// 迁移一个共享 PVC 的所有子目录到 per-agent PVC。
/// 返回成功迁移的项目数。
async fn migrate_shared_pvc(
    runtime: &Arc<dyn ContainerRuntime>,
    pvc_env: &str,
    subpath: &[&str],
    service_type: ServiceType,
    dst_at_root: bool,
    skipped: &mut u32,
    failed: &mut u32,
) -> u32 {
    let Some(shared_pvc) = std::env::var(pvc_env).ok().filter(|s| !s.is_empty()) else {
        info!("[BATCH_MIGRATE] {} not set, skip", pvc_env);
        return 0;
    };

    // resolve 共享 PVC subvolumePath (经挂根)
    let shared_base = match runtime.resolve_workspace_path_by_pvcname(&shared_pvc).await {
        Ok(Some(p)) => PathBuf::from(p),
        _ => {
            warn!("[BATCH_MIGRATE] cannot resolve shared PVC {}: skip", pvc_env);
            return 0;
        }
    };

    let mut shared_root = shared_base;
    for s in subpath {
        shared_root = shared_root.join(s);
    }

    // 遍历共享 PVC 子目录 (各 project/user)
    let mut rd = match tokio::fs::read_dir(&shared_root).await {
        Ok(rd) => rd,
        Err(e) => {
            warn!(
                "[BATCH_MIGRATE] read_dir {} failed: {} (可能无老数据)",
                shared_root.display(),
                e
            );
            return 0;
        }
    };

    let mut migrated = 0u32;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        // 跳过 marker / 隐藏文件
        if name.starts_with('.') {
            continue;
        }

        let identifier = &name;

        // ensure per-agent PVC (SC Immediate 立即 Bound)
        if let Err(e) = runtime.ensure_workspace(identifier, &service_type, None).await {
            warn!("[BATCH_MIGRATE] ensure_workspace {} failed: {}, skip", identifier, e);
            *failed += 1;
            continue;
        }

        // resolve per-agent subvolumePath
        let per_agent_base = match runtime.resolve_workspace_path(identifier, &service_type).await {
            Ok(Some(p)) => PathBuf::from(p),
            _ => {
                warn!(
                    "[BATCH_MIGRATE] resolve per-agent {} failed, skip",
                    identifier
                );
                *failed += 1;
                continue;
            }
        };

        let src_item = entry.path();
        let dst_item = if dst_at_root {
            per_agent_base.clone()
        } else {
            per_agent_base.join(identifier)
        };

        // 幂等: .migrated marker 存在 → skip
        let marker = dst_item.join(".migrated");
        if tokio::fs::try_exists(&marker).await.unwrap_or(false) {
            *skipped += 1;
            continue;
        }

        // ensure dst 存在
        if let Err(e) = tokio::fs::create_dir_all(&dst_item).await {
            warn!(
                "[BATCH_MIGRATE] create_dir_all {} failed: {}, skip",
                dst_item.display(),
                e
            );
            *failed += 1;
            continue;
        }

        // copy (不删源, 安全)
        match crate::workspace_migrate::copy_dir_recursive_pub(&src_item, &dst_item).await {
            Ok(()) => {
                let _ = tokio::fs::write(&marker, b"1").await;
                migrated += 1;
                info!(
                    "[BATCH_MIGRATE] {} {} copied {} -> {}",
                    service_type, identifier,
                    src_item.display(), dst_item.display()
                );
            }
            Err(e) => {
                warn!(
                    "[BATCH_MIGRATE] {} {} copy failed: {}",
                    service_type, identifier, e
                );
                *failed += 1;
            }
        }
    }

    migrated
}
