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
pub fn spawn_if_enabled(
    runtime: Arc<dyn ContainerRuntime>,
    stop: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<Result<(), String>>> {
    let flags = shared_types::FeatureFlags::get();
    if !flags.batch_migrate_on_startup {
        return None;
    }
    if !flags.per_agent_pvc {
        info!("[BATCH_MIGRATE] per_agent_pvc disabled, skip batch migration");
        return None;
    }
    info!("[BATCH_MIGRATE] starting batch migration (background task)");
    Some(tokio::spawn(async move {
        let result = run_batch_migrate(&runtime, &stop).await;
        if let Err(error) = &result {
            warn!(%error, "Batch migration failed");
        }
        result
    }))
}

async fn run_batch_migrate(
    runtime: &Arc<dyn ContainerRuntime>,
    stop: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let mut total_migrated = 0u32;
    let mut counters = MigrationCounters::default();

    // Web projects: 共享 rcoder-workspace PVC subPath=workspace → /workspace/{projectId}
    total_migrated += migrate_shared_pvc(
        runtime.as_ref(),
        stop,
        "RCODER_WORKSPACE_PVC_NAME",
        &["workspace"],
        ServiceType::WebAgentRunner,
        false,
        &mut counters,
    )
    .await;

    // Computer users: 共享 rcoder-computer-workspace PVC → /{userId}
    total_migrated += migrate_shared_pvc(
        runtime.as_ref(),
        stop,
        "RCODER_COMPUTER_WORKSPACE_PVC_NAME",
        &[],
        ServiceType::ComputerAgentRunner,
        true,
        &mut counters,
    )
    .await;

    info!(
        "[BATCH_MIGRATE] completed: migrated={}, skipped={}, failed={}",
        total_migrated, counters.skipped, counters.failed
    );
    if counters.failed > 0 {
        return Err(format!(
            "Batch migration failed for {} workspace(s)",
            counters.failed
        ));
    }
    Ok(())
}

#[derive(Default)]
struct MigrationCounters {
    skipped: u32,
    failed: u32,
}

/// 迁移一个共享 PVC 的所有子目录到 per-agent PVC。
/// 返回成功迁移的项目数。
async fn migrate_shared_pvc(
    runtime: &dyn container_runtime_api::WorkspaceRuntime,
    stop: &tokio_util::sync::CancellationToken,
    pvc_env: &str,
    subpath: &[&str],
    service_type: ServiceType,
    dst_at_root: bool,
    counters: &mut MigrationCounters,
) -> u32 {
    if stop.is_cancelled() {
        return 0;
    }
    let Some(shared_pvc) = std::env::var(pvc_env).ok().filter(|s| !s.is_empty()) else {
        info!("[BATCH_MIGRATE] {} not set, skip", pvc_env);
        return 0;
    };

    // resolve 共享 PVC subvolumePath (经挂根)
    let shared_base = match runtime.resolve_workspace_path_by_pvcname(&shared_pvc).await {
        Ok(Some(p)) => PathBuf::from(p),
        _ => {
            warn!(
                "[BATCH_MIGRATE] cannot resolve shared PVC {}: skip",
                pvc_env
            );
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
    loop {
        // Let an in-flight ensure/copy/marker finish before checking stop again.
        if stop.is_cancelled() {
            break;
        }
        let entry = match rd.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                // 遍历中断要留痕: 静默退出会让后续 identifier 看似"无老数据"
                warn!(
                    "[BATCH_MIGRATE] list {} entries interrupted: {} (未遍历项下次运行重试)",
                    shared_root.display(),
                    e
                );
                break;
            }
        };
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
        if let Err(e) = runtime
            .ensure_workspace(identifier, &service_type, None)
            .await
        {
            warn!(
                "[BATCH_MIGRATE] ensure_workspace {} failed: {}, skip",
                identifier, e
            );
            counters.failed += 1;
            continue;
        }

        // resolve per-agent subvolumePath
        let per_agent_base = match runtime
            .resolve_workspace_path(identifier, &service_type)
            .await
        {
            Ok(Some(p)) => PathBuf::from(p),
            _ => {
                warn!(
                    "[BATCH_MIGRATE] resolve per-agent {} failed, skip",
                    identifier
                );
                counters.failed += 1;
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
            counters.skipped += 1;
            continue;
        }

        // ensure dst 存在
        if let Err(e) = tokio::fs::create_dir_all(&dst_item).await {
            warn!(
                "[BATCH_MIGRATE] create_dir_all {} failed: {}, skip",
                dst_item.display(),
                e
            );
            counters.failed += 1;
            continue;
        }

        // copy (不删源, 安全)
        match crate::workspace_migrate::copy_dir_recursive_pub(&src_item, &dst_item).await {
            Ok(()) => {
                if let Err(e) = tokio::fs::write(&marker, b"1").await {
                    warn!(
                        "[BATCH_MIGRATE] copy 后写 marker {} 失败: {}",
                        marker.display(),
                        e
                    );
                    counters.failed += 1;
                    continue;
                }
                migrated += 1;
                info!(
                    "[BATCH_MIGRATE] {} {} copied {} -> {}",
                    service_type,
                    identifier,
                    src_item.display(),
                    dst_item.display()
                );
            }
            Err(e) => {
                warn!(
                    "[BATCH_MIGRATE] {} {} copy failed: {}",
                    service_type, identifier, e
                );
                counters.failed += 1;
            }
        }
    }

    migrated
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    struct Workspace {
        source: PathBuf,
        destination: PathBuf,
        stop: tokio_util::sync::CancellationToken,
        ensured: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl container_runtime_api::WorkspaceRuntime for Workspace {
        async fn resolve_workspace_path_by_pvcname(
            &self,
            _: &str,
        ) -> container_runtime_api::ContainerRuntimeResult<Option<String>> {
            Ok(Some(self.source.to_string_lossy().into_owned()))
        }
        async fn ensure_workspace(
            &self,
            _: &str,
            _: &ServiceType,
            _: Option<&str>,
        ) -> container_runtime_api::ContainerRuntimeResult<()> {
            self.ensured
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.stop.cancel();
            Ok(())
        }
        async fn resolve_workspace_path(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> container_runtime_api::ContainerRuntimeResult<Option<String>> {
            Ok(Some(self.destination.to_string_lossy().into_owned()))
        }
    }
    #[tokio::test]
    async fn batch_shutdown_finishes_current_copy_and_marker_before_stopping() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        for name in ["one", "two"] {
            tokio::fs::create_dir(source.path().join(name))
                .await
                .unwrap();
            tokio::fs::write(source.path().join(name).join("data.txt"), name)
                .await
                .unwrap();
        }
        let stop = tokio_util::sync::CancellationToken::new();
        let workspace = Workspace {
            source: source.path().into(),
            destination: destination.path().into(),
            stop: stop.clone(),
            ensured: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut counters = MigrationCounters::default();
        // PATH supplies only a present env entry; the controlled resolver owns
        // all paths and no production volume or process is involved.
        let count = migrate_shared_pvc(
            &workspace,
            &stop,
            "PATH",
            &[],
            ServiceType::WebAgentRunner,
            false,
            &mut counters,
        )
        .await;
        assert_eq!(count, 1);
        assert_eq!(counters.failed, 0);
        assert_eq!(
            workspace.ensured.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let entries: Vec<_> = std::fs::read_dir(destination.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].join(".migrated").exists());
        assert!(entries[0].join("data.txt").exists());
        assert!(source.path().join("one/data.txt").exists());
        assert!(source.path().join("two/data.txt").exists());
    }
}
