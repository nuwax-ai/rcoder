//! Cold deployment waits for the matching operation's durable artifact-stage result.
//! Request release IDs and manifest identities are diagnostic values, never fences.
//! Old pods may answer during replacement; their terminal states cannot complete or
//! fail a newer operation. Business readiness remains a separate runtime concern.

use std::time::Duration;

use tracing::{info, warn};

use crate::error::AppOperationError;
use crate::error::AppResult;
use crate::service::AppService;

/// Reconciliation has a hard ceiling; HTTP response waiting remains 300 seconds.
// HTTP waiting is bounded separately; the owned coordinator retains its lease.
// Replaced by staged budgets from DeployBudgetConfig; kept for test assertions.
#[allow(dead_code)]
const DEPLOY_STAGE_BUDGET: Duration = Duration::from_secs(30 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The runtime's deploy status endpoint answering successfully while never
/// naming the expected operation means the environment declaration was not
/// consumed. The grace covers container boot and first acceptance; sustained
/// absence beyond it is a deterministic mismatch, not slow progress.
const RUNTIME_DECLARATION_GRACE: Duration = Duration::from_secs(180);
/// app-cli 管理 API 端口（容器内恒绑 0.0.0.0）。
const APP_CLI_ADMIN_PORT: u16 = shared_types::APP_CLI_ADMIN_PORT;

/// `/v1/deploy/status` 响应的有效载荷（信封裁剪后供判定核心消费）。
#[derive(Debug, Default, Clone)]
pub(crate) struct DeployStatusProbe {
    pub protocol_version: Option<u32>,
    pub operation: Option<shared_types::AppDeploymentOperation>,
    /// 能力声明（progress_v1 等）。
    pub capabilities: Vec<String>,
    /// 部署进度（progress_v1 能力声明后有值）。
    pub progress: Option<shared_types::AppDeploymentProgress>,
}

/// Parse either envelope or bare JSON without weakening the operation contract.
pub(crate) fn parse_deploy_status(value: &serde_json::Value) -> DeployStatusProbe {
    let data = value.get("data").unwrap_or(value);
    DeployStatusProbe {
        protocol_version: data
            .get("protocol_version")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok()),
        operation: data
            .get("operation")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        capabilities: data
            .get("capabilities")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
        progress: data
            .get("progress")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
    }
}

/// 等待核心判定（纯函数，判据矩阵单测入口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StageVerdict {
    /// 部署段完成（编排已启动）——同步等待成功返回。
    Done,
    /// 部署段未完成/不可观测——继续轮询。
    Pending,
    /// **已验证的**容器侧终态失败：协议 ∧ generation ∧ operation_id ∧ persisted
    /// ∧ deploy_stage==Failed 全部匹配。唯一可释放租约（mark_completed）的失败。
    VerifiedFailed(String),
    /// 响应契约错误（协议版本/generation 不匹配等）——结果未知，保持围栏，
    /// 不得据此释放租约（计划 1c：修旧版"同 op_id 错 generation 的 persisted
    /// Failed 也会释放"漏洞）。
    ContractError(String),
}

/// Artifact-stage completion is immutable even if later business orchestration fails.
pub(crate) fn judge_stage(probe: &DeployStatusProbe, expected_operation_id: &str) -> StageVerdict {
    use shared_types::AppDeploymentStage;
    let Some(operation) = &probe.operation else {
        return StageVerdict::Pending;
    };
    if operation.operation_id != expected_operation_id {
        return StageVerdict::Pending;
    }
    if probe.protocol_version != Some(shared_types::APP_CLI_UNIFIED_DEPLOY_PROTOCOL)
        || operation.deployment_generation_id != expected_operation_id
    {
        return StageVerdict::ContractError("invalid cold deployment operation contract".into());
    }
    if !operation.persisted {
        return StageVerdict::Pending;
    }
    match operation.deploy_stage {
        AppDeploymentStage::Succeeded => StageVerdict::Done,
        AppDeploymentStage::Pending => StageVerdict::Pending,
        // 到达此分支前协议 ∧ generation ∧ op_id 已全匹配（上方门卫）——
        // persisted Failed 是已验证终态
        AppDeploymentStage::Failed => StageVerdict::VerifiedFailed(
            operation
                .error
                .clone()
                .unwrap_or_else(|| "artifact deployment failed".into()),
        ),
    }
}

/// Tracks successful deploy-status responses that never name the expected
/// operation. Absence is only accumulated on successful responses: transport
/// failures leave the timer untouched and remain governed by the stage and
/// absolute budgets.
#[derive(Debug, Default)]
struct DeclarationWatchdog {
    absent_since: Option<tokio::time::Instant>,
}

impl DeclarationWatchdog {
    /// Observe one successful response; returns `true` when the expected
    /// operation has been persistently absent beyond the grace budget.
    fn observe(&mut self, expected_present: bool, now: tokio::time::Instant) -> bool {
        if expected_present {
            self.absent_since = None;
            return false;
        }
        let since = *self.absent_since.get_or_insert(now);
        now.duration_since(since) >= RUNTIME_DECLARATION_GRACE
    }
}

impl AppService {
    /// 冷部署同步等待：部署段完成（编排启动）即 Ok。超时/容器 Error 态/部署
    /// 期被删/容器侧 Failed 即 Err——错误信息面向 Java/前端排查直读。
    pub(crate) async fn wait_deploy_stage(
        &self,
        app_id: &str,
        operation_id: &str,
        operation_guard: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        use super::deploy_signals::{FailureSignalThresholds, FailureSignalTracker};

        let budget = &self.config.deploy_budget;
        let mut client: Option<reqwest::Client> = None;
        let now = tokio::time::Instant::now();
        // R10：bind-once 持久 deadline 单一预算——首次执行与恢复重放共用
        // 受理时绑定的同一截止时间（不重开 now+预算 窗口截断/放宽语义）；
        // 无绑定记录（legacy/防御路径）退配置兜底。
        let absolute_deadline = match self
            .metadata
            .store
            .operation_deadline(app_id, operation_id)
            .await
        {
            Ok(Some(deadline_ms)) => {
                let now_epoch_ms = chrono::Utc::now().timestamp_millis();
                let remaining_ms = (deadline_ms - now_epoch_ms).max(0) as u64;
                tokio::time::Instant::now() + Duration::from_millis(remaining_ms)
            }
            _ => now + Duration::from_secs(budget.absolute_budget_secs),
        };
        // pre_appcli 阶段预算（app-cli 未响应时的默认超时）
        let pre_appcli_budget = Duration::from_secs(budget.pre_appcli_stage_budget_secs);
        let no_progress_budget = Duration::from_secs(budget.no_progress_timeout_secs);
        let sql_stage_budget = Duration::from_secs(budget.sql_stage_budget_secs);
        // 确定性失败信号跟踪：结构化 Pod 观察 + 同 pod 防抖。
        let thresholds = FailureSignalThresholds {
            crash_restart: budget.failure_restart_threshold,
            oom_restart: budget.oom_restart_threshold,
        };
        let mut failure_tracker = FailureSignalTracker::default();
        let mut declaration_watchdog = DeclarationWatchdog::default();

        // 高水位指纹跟踪
        let mut capability_fixed = false; // progress_v1 能力是否已固定
        let mut has_progress_v1 = false; // 能力固定后的值
        let mut last_progress_at = now; // 最后有效进展时刻
        let mut in_sql_stage = false; // 当前是否在 SQL 阶段
        let mut sql_stage_start: Option<tokio::time::Instant> = None; // SQL 阶段起点
        let mut last_activity: u64 = 0; // 高水位 activity
        let mut last_step = String::new(); // 高水位 step

        // 初始预算：pre_appcli（app-cli 未响应前不得短于现状）
        let mut effective_budget = pre_appcli_budget;

        loop {
            let now = tokio::time::Instant::now();
            // 绝对 deadline 始终有效
            if now >= absolute_deadline {
                return Err(AppOperationError::Backend(format!(
                    "deploy stage not confirmed within absolute budget {}s \
                     (app {app_id}, operation {operation_id})",
                    budget.absolute_budget_secs
                )));
            }
            // 阶段预算检查
            let stage_deadline = last_progress_at + effective_budget;
            let active_deadline = std::cmp::min(absolute_deadline, stage_deadline);
            if now >= active_deadline {
                return Err(AppOperationError::Backend(format!(
                    "deploy stage no progress for {}s (app {app_id}, operation {operation_id}); \
                     container may still be deploying in background — GET /apps/{app_id} to \
                     check status (or app logs) before retrying",
                    effective_budget.as_secs()
                )));
            }

            let desired = match self.runtime.app_env_snapshot(app_id).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    warn!(app_id, operation_id, %error, "Deployment configuration unavailable; retaining reconciliation ownership");
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };
            let token = desired
                .env
                .get("APP_CLI_DEPLOY_TOKEN")
                .filter(|t| !t.trim().is_empty());
            if desired
                .env
                .get(shared_types::APP_DEPLOY_OPERATION_ID)
                .map(String::as_str)
                != Some(operation_id)
            {
                return Err(AppOperationError::Conflict(format!(
                    "deployment operation {operation_id} was superseded on app {app_id}"
                )));
            }
            let status = match self.fetch_runtime_status_or_err(app_id).await {
                Ok(status) => status,
                Err(AppOperationError::NotFound(_)) => {
                    return Err(AppOperationError::Backend(format!(
                        "app {app_id} was deleted while waiting for deploy stage"
                    )));
                }
                Err(e) => {
                    // 瞬态观察失败 ≠ 进展：不重置任何计时（预算检查在循环顶恒生效）
                    warn!(
                        app_id = app_id,
                        %e,
                        "deploy stage poll transient error, retrying within budget"
                    );
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };

            // 确定性失败信号（仅 K8s——DeploymentStatus.deployment_uid 存在才有
            // 身份锚点；Docker/Mock 默认空观察天然 no-op）。观察失败按"无观察
            // 证据"处理（不重置计时、不判失败）。
            if let Some(deployment_uid) = status.deployment_uid.as_deref().filter(|u| !u.is_empty())
            {
                let target = container_runtime_api::AppDeployTarget {
                    deployment_uid: deployment_uid.to_string(),
                    // 模板令牌 = 本次操作 operation_id（create/patch 写入 pod template）
                    template_token: operation_id.to_string(),
                };
                match self.runtime.observe_app_pods(app_id, &target).await {
                    Ok(observations) => {
                        if let Some(classification) =
                            failure_tracker.observe(&observations, &thresholds)
                        {
                            return Err(AppOperationError::Backend(format!(
                                "deterministic deployment failure on app {app_id} \
                                 (operation {operation_id}): {} — failing fast within budget; \
                                 retry after correcting the underlying cause \
                                 (state fenced until safe recovery)",
                                classification.describe()
                            )));
                        }
                    }
                    Err(error) => {
                        warn!(
                            app_id = app_id,
                            %error,
                            "deployment pod observation failed; continuing within budget"
                        );
                    }
                }
            }

            let ip = status
                .pod_ip
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(ip) = ip {
                let client = match client {
                    Some(ref client) => client,
                    ref mut slot @ None => {
                        slot.insert(reqwest::Client::builder().no_proxy().build().map_err(
                            |error| {
                                AppOperationError::Backend(format!(
                                    "build direct container deployment status client: {error}"
                                ))
                            },
                        )?)
                    }
                };
                let mut req = client
                    .get(format!("http://{ip}:{APP_CLI_ADMIN_PORT}/v1/deploy/status"))
                    .timeout(STATUS_REQUEST_TIMEOUT);
                if let Some(token) = token {
                    req = req.header("X-Deploy-Token", token);
                }
                // 非 2xx / 解码失败不再静默 continue：预算耗尽前的最后线索往往
                // 只有这批 warn（连接类失败仍是冷启动窗口的常态，维持静默）。
                let body = match req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        match resp.json::<serde_json::Value>().await {
                            Ok(body) => Some(body),
                            Err(error) => {
                                warn!(
                                    app_id,
                                    operation_id,
                                    %error,
                                    "deploy status response decode failed; continuing within budget"
                                );
                                None
                            }
                        }
                    }
                    Ok(resp) => {
                        warn!(
                            app_id,
                            operation_id,
                            status = %resp.status(),
                            "deploy status endpoint returned non-success; continuing within budget"
                        );
                        None
                    }
                    Err(_) => None,
                };
                if let Some(body) = body {
                    let probe = parse_deploy_status(&body);
                    // 部署声明未消费看门狗：端点成功应答但持续不出现本次操作。
                    // 与 judge_stage 的 Pending 不同，这是"声明根本没被接受"的
                    // 确定性失配——超宽限即快速失败，不再静默耗满阶段预算。
                    let expected_present = probe
                        .operation
                        .as_ref()
                        .is_some_and(|operation| operation.operation_id == operation_id);
                    if declaration_watchdog.observe(expected_present, tokio::time::Instant::now()) {
                        return Err(AppOperationError::Backend(format!(
                            "runtime reported no deployment for operation {operation_id} on app \
                             {app_id} within {}s (deployment declaration was not consumed); \
                             verify the app-runtime image consumes environment deployments, \
                             then retry",
                            RUNTIME_DECLARATION_GRACE.as_secs()
                        )));
                    }
                    // 能力固定：首次通过 protocol 校验且携带 operation 的有效响应上固定，
                    // 之后不翻转（解析失败/字段缺失不重新协商）。
                    // operation_id 校验由 env snapshot 检查 + judge_stage 保证——
                    // 能力固定不做终态判定，只决定看门狗策略。
                    if !capability_fixed
                        && probe.protocol_version
                            == Some(shared_types::APP_CLI_UNIFIED_DEPLOY_PROTOCOL)
                        && probe.operation.is_some()
                    {
                        capability_fixed = true;
                        has_progress_v1 = probe.capabilities.iter().any(|c| c == "progress_v1");
                        if has_progress_v1 {
                            info!(
                                app_id,
                                operation_id,
                                "progress_v1 capability declared; enabling staged budgets"
                            );
                            effective_budget = no_progress_budget;
                            // 能力首次固定 = 收到有效响应，重置无进展计时起点
                            last_progress_at = tokio::time::Instant::now();
                        }
                    }
                    // 高水位指纹：同 step/activity 严格增长才算进展
                    // SQL 阶段内 activity 增长不重置阶段起点、不退出 SQL 预算
                    // （第二个700s文件不被600s无进展截断）
                    if let Some(ref progress) = probe.progress {
                        let new_activity = progress.activity > last_activity;
                        let new_step = progress.step != last_step;
                        if new_activity || new_step {
                            if new_activity {
                                last_activity = progress.activity;
                            }
                            if new_step {
                                last_step = progress.step.clone();
                            }
                            // SQL 阶段内：activity 增长只更新进度，不重置计时
                            // 只有离开 SQL 阶段的 step 变化才重置 last_progress_at
                            if !in_sql_stage || new_step {
                                last_progress_at = tokio::time::Instant::now();
                            }
                        }
                        // SQL 阶段切换：经身份校验进入 running_sql → SQL 预算
                        if has_progress_v1 && progress.step == "running_sql" && !in_sql_stage {
                            in_sql_stage = true;
                            sql_stage_start = Some(tokio::time::Instant::now());
                            effective_budget = sql_stage_budget;
                            info!(
                                app_id,
                                operation_id,
                                "entering SQL stage; switching to {}s budget",
                                sql_stage_budget.as_secs()
                            );
                        }
                        // 经身份校验明确离开 running_sql → 恢复普通看门狗
                        if has_progress_v1 && progress.step != "running_sql" && in_sql_stage {
                            in_sql_stage = false;
                            let sql_stage_elapsed =
                                sql_stage_start.take().map(|start| start.elapsed());
                            effective_budget = no_progress_budget;
                            last_progress_at = tokio::time::Instant::now();
                            match sql_stage_elapsed {
                                Some(elapsed) => info!(
                                    app_id,
                                    operation_id,
                                    sql_stage_secs = elapsed.as_secs_f64(),
                                    "leaving SQL stage; restoring no-progress budget"
                                ),
                                None => info!(
                                    app_id,
                                    operation_id, "leaving SQL stage; restoring no-progress budget"
                                ),
                            }
                        }
                    }
                    match judge_stage(&probe, operation_id) {
                        StageVerdict::Done => {
                            info!(
                                "[APP] deploy stage done (orchestration started): \
                                 app_id={app_id}, operation_id={operation_id}"
                            );
                            return Ok(());
                        }
                        StageVerdict::VerifiedFailed(err) => {
                            // VerifiedFailed 已含"协议 ∧ generation ∧ op_id ∧ persisted
                            // ∧ Failed"全匹配——可信终态，释放租约供人工重试
                            operation_guard.mark_completed();
                            return Err(AppOperationError::Backend(format!(
                                "deploy stage failed on app {app_id} (operation {operation_id}): {err}"
                            )));
                        }
                        StageVerdict::ContractError(err) => {
                            // 契约错误 = 结果未知：保持围栏（不 mark_completed）
                            return Err(AppOperationError::Backend(format!(
                                "deploy stage contract error on app {app_id} \
                                 (operation {operation_id}): {err}; outcome uncertain — \
                                 lease retained"
                            )));
                        }
                        StageVerdict::Pending => {}
                    }
                }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wait_signal_tests;
