//! per-agent workspace 迁移编排（P2 第二批, engine 侧原子编排）。
//!
//! 测试期语义（与 migration_api 的边界）: API 层管白名单与协议, 本模块管
//! **数据面原子编排**——把"停 agent → 防呆 → rclone 同步 → 翻标记 → 起 agent"
//! 收成单次调用, 任一环节失败即整体失败并留下可回滚状态。
//!
//! ```text
//! phase_a:  ensure PVC(不翻标记) → sync(Full, 限速)              → NotMigrated
//! cutover: stop agent → 等 detach(防呆 fail-closed) → sync(Incremental)
//!           → set Migrated → ensure agent(重建挂 RBD)              → Migrated
//! rollback: set NotMigrated（共享卷数据全程保留, 回切即恢复）
//! ```
//!
//! 与外部脚本（k8s/migration）的关系: 脚本是**人工单次**工具; 本编排是
//! API 驱动的批量安全生产路径（翻标记与起 agent 原子完成, 不存在"脚本跑到
//! 一半人去翻标记"的中间态）。

use container_runtime_api::ContainerRuntimeError;
use shared_types::{ServiceType, WorkspaceMigrationState, WorkspaceSyncMode};

use crate::app_state::AppState;
use crate::service::computer_container_manager::{
    ComputerContainerManager, ContainerCreateOptions,
};

/// 编排结果（API 层原样投影给调用方）。
#[derive(Debug, Clone)]
pub struct MigrationOutcome {
    pub user_id: String,
    pub action: &'static str,
    pub migration_state: WorkspaceMigrationState,
    /// 人类可读的步骤轨迹（排障用）。
    pub steps: Vec<String>,
}

/// phase-a: 确保 per-agent PVC 存在（**不翻标记**）+ 全量预同步（限速）。
///
/// 用户 agent 不受影响（未 migrated → 卷选择/转发都仍走共享卷）。
pub async fn phase_a(
    state: &AppState,
    user_id: &str,
    src_pvc: &str,
) -> Result<MigrationOutcome, ContainerRuntimeError> {
    let service_type = ServiceType::ComputerAgentRunner;
    let mut steps = Vec::new();

    state
        .runtime()
        .ensure_workspace(user_id, &service_type, None)
        .await
        .map_err(|e| wrap("ensure_workspace", e))?;
    steps.push("pvc ensured".to_string());

    state
        .runtime()
        .run_workspace_migration_sync(user_id, &service_type, WorkspaceSyncMode::Full, src_pvc)
        .await
        .map_err(|e| wrap("sync(full)", e))?;
    steps.push("full sync done".to_string());

    Ok(MigrationOutcome {
        user_id: user_id.to_string(),
        action: "phase-a",
        migration_state: WorkspaceMigrationState::NotMigrated,
        steps,
    })
}

/// cutover: 停 agent → 防呆等 detach → 增量 sync → 翻 Migrated → 起 agent。
///
/// 顺序不可换: sync 前必须确认 RBD 无挂载（同节点双挂 = ext4 损坏）; 翻标记
/// 必须在 sync 校验后（翻早了agent 重建会挂到半数据卷）; 起 agent 必须最后
/// （migrated 已生效, 重建即挂 RBD）。
pub async fn cutover(
    state: &AppState,
    user_id: &str,
    src_pvc: &str,
) -> Result<MigrationOutcome, ContainerRuntimeError> {
    let service_type = ServiceType::ComputerAgentRunner;
    let mut steps = Vec::new();

    // 1. 停 agent（删 STS → 卷卸载开始）
    state
        .runtime()
        .stop_container_by_identifier(user_id, &service_type)
        .await
        .map_err(|e| wrap("stop_agent", e))?;
    steps.push("agent stopped".to_string());

    // 2. 防呆: 等 RBD 完全卸载（fail-closed——查询出错立即中止, 不放行双挂）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(150);
    loop {
        let attached = state
            .runtime()
            .workspace_volume_attached(user_id, &service_type)
            .await
            .map_err(|e| wrap("volume_attached_check", e))?;
        if !attached {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(ContainerRuntimeError::ConnectionError(format!(
                "RBD volume for '{user_id}' still attached 150s after agent stop"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    steps.push("volume detached".to_string());

    // 3. 增量 sync（agent 已停, 数据面冻结, 增量体积极小）
    state
        .runtime()
        .run_workspace_migration_sync(
            user_id,
            &service_type,
            WorkspaceSyncMode::Incremental,
            src_pvc,
        )
        .await
        .map_err(|e| wrap("sync(incremental)", e))?;
    steps.push("incremental sync done".to_string());

    // 4. 翻 Migrated（此后转发/卷选择立即切 RBD; 缓存即时失效）
    state
        .runtime()
        .set_workspace_migration_state(user_id, &service_type, WorkspaceMigrationState::Migrated)
        .await
        .map_err(|e| wrap("set_migrated", e))?;
    steps.push("migration state = migrated".to_string());

    // 5. 起 agent（ensure 幂等; migrated 已生效 → 重建即挂 per-agent RBD）
    let options = ContainerCreateOptions {
        user_id: user_id.to_string(),
        project_id: user_id.to_string(),
        resource_limits: state
            .config
            .kubernetes_config
            .as_ref()
            .and_then(|kc| kc.get_service_config(&service_type))
            .map(|sc| sc.resource_limits.clone()),
        pod_id: None,
        isolation_type: None,
        tenant_id: None,
        space_id: None,
        service_type,
    };
    ComputerContainerManager::ensure_container_for_user_with_type(&options, state.runtime())
        .await
        .map_err(|e| wrap("ensure_agent", runtime_error_from_app(e)))?;
    steps.push("agent restarted on RBD".to_string());

    Ok(MigrationOutcome {
        user_id: user_id.to_string(),
        action: "cutover",
        migration_state: WorkspaceMigrationState::Migrated,
        steps,
    })
}

/// rollback: 翻回 NotMigrated（共享卷数据全程保留, 用户回共享路径;
/// 下一次 agent 重建即挂共享卷）。
pub async fn rollback(
    state: &AppState,
    user_id: &str,
) -> Result<MigrationOutcome, ContainerRuntimeError> {
    let service_type = ServiceType::ComputerAgentRunner;
    state
        .runtime()
        .set_workspace_migration_state(
            user_id,
            &service_type,
            WorkspaceMigrationState::NotMigrated,
        )
        .await
        .map_err(|e| wrap("set_not_migrated", e))?;
    Ok(MigrationOutcome {
        user_id: user_id.to_string(),
        action: "rollback",
        migration_state: WorkspaceMigrationState::NotMigrated,
        steps: vec!["migration state = not-migrated".to_string()],
    })
}

fn wrap(step: &str, error: ContainerRuntimeError) -> ContainerRuntimeError {
    ContainerRuntimeError::K8sError(format!("migration step '{step}' failed: {error}"))
}

/// ComputerContainerManager 返回 AppError（engine 域错误）; 编排链路统一
/// ContainerRuntimeError, 此处投影（消息保留原文）。
fn runtime_error_from_app(error: crate::AppError) -> ContainerRuntimeError {
    ContainerRuntimeError::K8sError(error.to_string())
}
