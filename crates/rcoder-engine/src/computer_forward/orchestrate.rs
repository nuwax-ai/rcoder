//! per-agent workspace 迁移编排（P2 第二批, engine 侧原子编排）。
//!
//! 测试期语义（与 migration_api 的边界）: API 层管白名单与协议, 本模块管
//! **数据面原子编排**——把"停 agent → 防呆 → rclone 同步 → 翻标记 → 起 agent"
//! 收成单次调用, 任一环节失败即整体失败并留下可回滚状态。
//!
//! ```text
//! phase_a:  ensure PVC(不翻标记) → sync(Full, 限速)              → NotMigrated
//! cutover: stop agent → 等 detach(防呆 fail-closed) → sync(Incremental)
//!           → set Migrated → 恢复 agent 切换前形态                 → Migrated
//!              (在运行 → 重建挂 RBD; 闲置已停 → 保持停止;
//!               切换期间在旧卷被唤醒 → 替换到 RBD)
//! rollback: stop agent → set NotMigrated → 恢复 agent 切换前形态(共享卷)
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

/// cutover 写门禁守卫：Drop 时解除门禁（任何失败路径 `?` 提前返回都会
/// 走 Drop），成功路径显式 `forget` 后手动 end（保证步骤信息完整）。
struct MigrationGateGuard {
    user_id: String,
    token: u64,
    disarmed: bool,
}

impl MigrationGateGuard {
    /// None = 同用户已有门禁（旧后台任务仍在切换中）——调用方应中止编排。
    fn acquire(user_id: &str) -> Option<Self> {
        let token = super::migration_gate::begin(user_id)?;
        Some(Self {
            user_id: user_id.to_string(),
            token,
            disarmed: false,
        })
    }
    /// 成功路径：按自身 token 解除门禁（替代 Drop）。
    fn release(mut self) {
        self.disarmed = true;
        super::migration_gate::end_with_token(&self.user_id, self.token);
    }
}

impl Drop for MigrationGateGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            super::migration_gate::end_with_token(&self.user_id, self.token);
        }
    }
}

/// agent 工作负载当前是否存在（运行中或启动中）。
async fn agent_present(state: &AppState, user_id: &str) -> Result<bool, ContainerRuntimeError> {
    Ok(state
        .runtime()
        .find_container(user_id, &ServiceType::ComputerAgentRunner)
        .await?
        .is_some())
}

/// 切换收尾对 agent 的处置（纯决策，便于单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentRestore {
    /// 切换前 agent 在运行：按原样重建（标记已翻 → 挂目标卷）。
    Restart,
    /// 切换前 agent 不在（闲置已回收）：保持停止，不为它拉起 agent。
    KeepStopped,
    /// 切换前不在、翻标记时已在：切换期间（agent 已停、标记未翻）其他副本
    /// 的流量唤醒在**旧卷**上重建了 agent——ensure 幂等会原样复用它，必须
    /// 先停再重建到目标卷。
    ReplaceWoken,
}

impl AgentRestore {
    fn decide(was_present: bool, present_after_flip: bool) -> Self {
        match (was_present, present_after_flip) {
            (true, _) => Self::Restart,
            (false, true) => Self::ReplaceWoken,
            (false, false) => Self::KeepStopped,
        }
    }
}

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

    // 编排互斥(终审 B/D): 与 ensure/chat 共用 per-workload 锁——封死并发
    // 编排(Job 同名互删/同节点双挂 mirror 删数据)与编排期间 ensure 穿透
    // 重建共享形态 agent。持锁后入口校验重读(排队方拿到锁时状态已可见)。
    let _migration_lock = ComputerContainerManager::acquire_migration_lock(user_id)
        .await
        .map_err(|e| wrap("acquire_migration_lock", runtime_error_from_app(e)))?;

    // 入口校验: 已 Migrated 用户重跑 phase-a, rclone mirror 会把 RBD 上
    // cutover 后新写、共享卷没有的文件**全部删除**——必须显式拒绝。
    // (想重迁: 先 rollback → agent 重建回共享 → 再 phase-a。)
    if matches!(
        state
            .runtime()
            .workspace_migration_state(user_id, &service_type)
            .await
            .map_err(|e| wrap("migration_state_check", e))?,
        WorkspaceMigrationState::Migrated
    ) {
        return Err(ContainerRuntimeError::Conflict(format!(
            "user '{user_id}' is already migrated; \
             rollback before re-running phase-a (mirror sync would delete \
             post-cutover writes on the RBD volume)"
        )));
    }
    // RBD 卷仍被挂载时(rollback 后 agent 未重建仍挂 RBD), 双卷 Job 的 RWO
    // 第二挂载会卡 Pending 直到 activeDeadlineSeconds(4h)——防呆拒绝。
    if state
        .runtime()
        .workspace_volume_attached(user_id, &service_type)
        .await
        .map_err(|e| wrap("volume_attached_check", e))?
    {
        return Err(ContainerRuntimeError::Conflict(format!(
            "RBD volume for '{user_id}' is still attached (agent not yet \
             rebuilt on shared volume after rollback?); restart the agent \
             before phase-a"
        )));
    }

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

    // 编排互斥(同 phase_a)。
    let migration_lock = ComputerContainerManager::acquire_migration_lock(user_id)
        .await
        .map_err(|e| wrap("acquire_migration_lock", runtime_error_from_app(e)))?;
    // 入口校验①: 已 Migrated 重跑 = mirror 删 RBD 新写数据(P0)。
    if matches!(
        state
            .runtime()
            .workspace_migration_state(user_id, &service_type)
            .await
            .map_err(|e| wrap("migration_state_check", e))?,
        WorkspaceMigrationState::Migrated
    ) {
        return Err(ContainerRuntimeError::Conflict(format!(
            "user '{user_id}' is already migrated; cutover is not idempotent-safe \
             to re-run (mirror sync would delete post-cutover writes)"
        )));
    }
    // 入口校验②(终审补): dst PVC 必须存在(跑过 phase-a)——否则 NotFound
    // 要到停 agent **之后**才触发, 留下停机残留。
    if !state
        .runtime()
        .workspace_pvc_exists(user_id, &service_type)
        .await
        .map_err(|e| wrap("pvc_exists_check", e))?
    {
        return Err(ContainerRuntimeError::ContainerNotFound(format!(
            "per-agent PVC for '{user_id}' not found; run phase-a before cutover"
        )));
    }

    // 写门禁开(终审 F1): 从停 agent 起该用户文件写一律 503。guard 的 Drop
    // 覆盖所有 `?` 失败路径(失败即解除, 该用户恢复可写——失败中间态下写
    // 共享卷是安全方向: 标记未翻, 数据面本就仍在共享卷)。
    let _migration_gate_guard = MigrationGateGuard::acquire(user_id).ok_or_else(|| {
        wrap(
            "migration_gate",
            ContainerRuntimeError::Conflict(
                "another migration orchestration is still settling for this user; retry shortly"
                    .to_string(),
            ),
        )
    })?;

    // 在途写排空(复审 P1): 门禁开启前已受理的写(长上传/慢盘)还在异步执行,
    // 不等它们落盘就停 agent + 最终 sync 会把这些写留在旧卷 → 源/目标分叉。
    // 数据面此刻仍活着(agent 未停), 10s 内排空是常态; 超时残留则中止切换
    // (释放门禁, 用户可写共享卷语义完好), 下次重试 cutover。
    let drain_timeout = std::time::Duration::from_secs(10);
    let remaining = super::migration_gate::drain_user_writes(user_id, drain_timeout).await;
    if remaining > 0 {
        return Err(ContainerRuntimeError::ConnectionError(format!(
            "cutover aborted: {remaining} in-flight write(s) for '{user_id}' did not drain within {drain_timeout:?}"
        )));
    }
    steps.push("in-flight writes drained".to_string());

    // 切换前 agent 形态采样：收尾据此恢复原状（夜间批量迁移空闲用户时，
    // agent 本就被闲置回收，不为每个用户拉起）。
    let agent_was_present = agent_present(state, user_id)
        .await
        .map_err(|e| wrap("agent_present_check", e))?;

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

    // 翻标记后再采样一次（持锁）：此刻若 agent 已在，只能是切换期间在旧
    // 共享卷上被其他副本唤醒重建的——见 AgentRestore::ReplaceWoken。
    let agent_present_after_flip = agent_present(state, user_id)
        .await
        .map_err(|e| wrap("agent_present_check", e))?;

    // 5. 恢复 agent 的切换前形态（ensure 幂等; migrated 已生效 → 重建即挂
    // per-agent RBD）。先释放编排锁——ensure 内部 acquire 同一 key, tokio
    // Mutex 不可重入, 持锁调用必死锁(creation::acquire 的 WAITER_BUDGET 30s
    // 全耗尽)。
    let restart_step = match AgentRestore::decide(agent_was_present, agent_present_after_flip) {
        AgentRestore::KeepStopped => {
            drop(migration_lock);
            steps.push("agent left stopped (not running before cutover)".to_string());
            _migration_gate_guard.release();
            return Ok(MigrationOutcome {
                user_id: user_id.to_string(),
                action: "cutover",
                migration_state: WorkspaceMigrationState::Migrated,
                steps,
            });
        }
        AgentRestore::ReplaceWoken => {
            state
                .runtime()
                .stop_container_by_identifier(user_id, &service_type)
                .await
                .map_err(|e| wrap("stop_agent_woken_during_cutover", e))?;
            "agent woken during cutover replaced on RBD"
        }
        AgentRestore::Restart => "agent restarted on RBD",
    };
    drop(migration_lock);
    let resource_limits = state
        .config
        .kubernetes_config
        .as_ref()
        .and_then(|kc| kc.get_service_config(&service_type))
        .map(|sc| sc.resource_limits.clone());
    if resource_limits.is_none() {
        // 与 intercept.rs wake 路径同口径 fail-fast——None 会回落
        // DEFAULT_AGENT_* 常量, 同一用户两条创建路径 limits 分裂。
        return Err(ContainerRuntimeError::Conflict(
            "kubernetes_config missing resource_limits for computer-agent-runner; \
             refusing to restart agent with fallback limits"
                .to_string(),
        ));
    }
    let options = ContainerCreateOptions {
        user_id: user_id.to_string(),
        project_id: user_id.to_string(),
        resource_limits,
        pod_id: None,
        isolation_type: None,
        tenant_id: None,
        space_id: None,
        service_type,
    };
    ComputerContainerManager::ensure_container_for_user_with_type(&options, state.runtime())
        .await
        .map_err(|e| wrap("ensure_agent", runtime_error_from_app(e)))?;
    steps.push(restart_step.to_string());

    _migration_gate_guard.release();
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
    let mut steps = Vec::new();

    let migration_lock = ComputerContainerManager::acquire_migration_lock(user_id)
        .await
        .map_err(|e| wrap("acquire_migration_lock", runtime_error_from_app(e)))?;
    // 未迁移用户 rollback = no-op(幂等)
    if matches!(
        state
            .runtime()
            .workspace_migration_state(user_id, &service_type)
            .await
            .map_err(|e| wrap("migration_state_check", e))?,
        WorkspaceMigrationState::NotMigrated
    ) {
        return Ok(MigrationOutcome {
            user_id: user_id.to_string(),
            action: "rollback",
            migration_state: WorkspaceMigrationState::NotMigrated,
            steps: vec!["already not-migrated, no-op".to_string()],
        });
    }
    // 切换前 agent 形态采样（与 cutover 对称）：收尾据此恢复原状。
    let agent_was_present = agent_present(state, user_id)
        .await
        .map_err(|e| wrap("agent_present_check", e))?;
    // C-1(终审): 翻标记前先停 agent——否则挂 RBD 的存量 agent 继续写 RBD,
    // 而转发层判 NotMigrated 放行共享卷(双卷读写分裂, 窗口无界)。
    state
        .runtime()
        .stop_container_by_identifier(user_id, &service_type)
        .await
        .map_err(|e| wrap("stop_agent", e))?;
    steps.push("agent stopped".to_string());
    // 复审第四轮 P1: 新用户直上 RBD 后 rollback, 共享目录本就可能不存在——
    // 卷上的 fresh-init 标记必须改写为 rolled-back, 否则 ensure 的首建续接
    // 分支会重放 Migrated, 撤销刚写入的切回决定。
    // 顺序(自查修正): **先改标记后翻状态**——两步间中断时 label=rolled-back
    // + marker 仍 Migrated(回滚未完成, 重试 rollback 即可); 反序中断则会
    // 留下 NotMigrated+fresh-init, ensure 续接分支会撤销回滚。
    state
        .runtime()
        .mark_workspace_volume_rolled_back(user_id, &service_type)
        .await
        .map_err(|e| wrap("mark_volume_rolled_back", e))?;
    steps.push("volume origin = rolled-back".to_string());
    state
        .runtime()
        .set_workspace_migration_state(user_id, &service_type, WorkspaceMigrationState::NotMigrated)
        .await
        .map_err(|e| wrap("set_not_migrated", e))?;
    steps.push("migration state = not-migrated".to_string());
    // 翻标记后再采样（与 cutover 对称）：此刻若 agent 已在，只能是回滚期间
    // 在 RBD 上被其他副本唤醒重建的（标记当时仍 Migrated）。
    let agent_present_after_flip = agent_present(state, user_id)
        .await
        .map_err(|e| wrap("agent_present_check", e))?;
    let restore = AgentRestore::decide(agent_was_present, agent_present_after_flip);
    let resource_limits = state
        .config
        .kubernetes_config
        .as_ref()
        .and_then(|kc| kc.get_service_config(&service_type))
        .map(|sc| sc.resource_limits.clone());
    if restore == AgentRestore::ReplaceWoken && resource_limits.is_some() {
        // 仅在随后能重建时才停（缺 limits 时停了起不来，不如留着）。
        state
            .runtime()
            .stop_container_by_identifier(user_id, &service_type)
            .await
            .map_err(|e| wrap("stop_agent_woken_during_rollback", e))?;
    }
    // 与 cutover 对称: 恢复 agent 的切换前形态(此时 NotMigrated → 共享卷形态)。
    // ensure 前释放锁(不可重入)。
    drop(migration_lock);
    if restore == AgentRestore::KeepStopped {
        steps.push("agent left stopped (not running before rollback)".to_string());
    } else if let Some(limits) = resource_limits {
        let restart_step = if restore == AgentRestore::Restart {
            "agent restarted on shared volume"
        } else {
            "agent woken during rollback replaced on shared volume"
        };
        let options = ContainerCreateOptions {
            user_id: user_id.to_string(),
            project_id: user_id.to_string(),
            resource_limits: Some(limits),
            pod_id: None,
            isolation_type: None,
            tenant_id: None,
            space_id: None,
            service_type,
        };
        match ComputerContainerManager::ensure_container_for_user_with_type(
            &options,
            state.runtime(),
        )
        .await
        {
            Ok(_) => steps.push(restart_step.to_string()),
            Err(e) => {
                // 重建失败不阻断 rollback(标记已翻, 下次请求自会 ensure 重建)
                tracing::warn!(user_id, error = %e, "[MIGRATION] rollback ensure agent failed");
                steps.push("agent restart deferred (will rebuild on next request)".to_string());
            }
        }
    }
    Ok(MigrationOutcome {
        user_id: user_id.to_string(),
        action: "rollback",
        migration_state: WorkspaceMigrationState::NotMigrated,
        steps,
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

#[cfg(test)]
mod agent_restore_tests {
    use super::AgentRestore;

    #[test]
    fn running_agent_is_restarted_whatever_the_later_observation() {
        assert_eq!(AgentRestore::decide(true, false), AgentRestore::Restart);
        assert_eq!(AgentRestore::decide(true, true), AgentRestore::Restart);
    }

    #[test]
    fn idle_agent_stays_stopped() {
        assert_eq!(
            AgentRestore::decide(false, false),
            AgentRestore::KeepStopped
        );
    }

    #[test]
    fn agent_woken_during_cutover_is_replaced() {
        assert_eq!(
            AgentRestore::decide(false, true),
            AgentRestore::ReplaceWoken
        );
    }
}
