//! Userapp 活动追踪与流量唤醒接口（trait）
//!
//! 支撑「闲置自动回收 + 流量唤醒」特性：
//! - [`AppAccessTracker`]（短期身份缓存，miss 异步刷新）由 Pingora 代理热路径调用，记录每个 Userapp 的最近 HTTP 访问时间，
//!   作为闲置回收的活动信号。缓存命中不查库；并发 miss 合流并重新核验 lifecycle。
//! - [`AppWakeControl`]（异步）由 Pingora 在请求过滤阶段调用：当目标 app 处于 stopped 或 starting 时，
//!   hold-and-wait 拉起（scale→1）并轮询 Ready，超时返回 [`WakeOutcome::Timeout`]。
//!
//! 两个 trait 仅暴露 Pingora 代理层（跨 crate 消费者）需要的方法（ISP：接口最小化）。
//! 其余同 crate 调用者（AppService / 回收扫描器）持具体 `AppActivityRegistry` 类型，直接用其 pub 方法
//! （`last_accessed_at` / `mark_running` / `mark_stopped` / `is_waking` / `seed_accessed`）。

use std::sync::Arc;

/// Userapp HTTP 访问追踪；使用已核验并可失效的 lifecycle 缓存登记该代次活动。
///
/// 由 Pingora `request_filter` 对 `/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/...` 路由调用。
/// 存储查证失败不得将旧活动时间重新绑定到新的 lifecycle。
#[async_trait::async_trait]
pub trait AppAccessTracker: Send + Sync {
    /// 记录 app 的最近一次真实 HTTP 访问（实现内部节流）。
    /// 返回该次登记使用的实际时间；None 表示身份未确认或已换代，不能报告保活成功。
    async fn touch(&self, app_id: &str) -> Option<chrono::DateTime<chrono::Utc>>;
}

/// 唤醒结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    /// 已就绪（本次唤醒成功，或本就 Running）
    Ready,
    /// 调用前已是 Running（无需唤醒）
    AlreadyRunning,
    /// 唤醒超时：携带执行阶段和结果证据，不能据此宣称 app 仍在启动。
    Timeout(WakeFailure),
    /// 唤醒失败（scale 失败、runtime 未就绪、app 进入 Error 相等）
    Failed(WakeFailure),
    /// Another durable or runtime operation owns the prod execution slot.
    Blocked {
        message: String,
        blocker: crate::UserAppOperationBlocker,
    },
}

/// Immutable failure from the original wake attempt, shared with its followers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeFailure {
    pub code: Arc<str>,
    pub cause_code: Arc<str>,
    pub stage: Arc<str>,
    pub message: String,
    pub retryable: bool,
    pub operation_id: Option<String>,
    pub blocker: Option<Box<crate::UserAppOperationBlocker>>,
    pub command_diagnostic: Option<Box<crate::PgCommandDiagnostic>>,
}

impl WakeFailure {
    pub fn new(
        code: impl Into<Arc<str>>,
        stage: impl Into<Arc<str>>,
        message: impl Into<String>,
    ) -> Self {
        let code = code.into();
        Self {
            cause_code: Arc::clone(&code),
            code,
            stage: stage.into(),
            message: message.into(),
            retryable: false,
            operation_id: None,
            blocker: None,
            command_diagnostic: None,
        }
    }

    pub fn timeout(stage: &'static str, operation_id: Option<String>, retryable: bool) -> Self {
        Self {
            operation_id,
            retryable,
            ..Self::new(
                crate::ERR_RUNTIME_TIMEOUT,
                stage,
                "Application wake deadline exceeded",
            )
        }
    }

    pub fn unknown_timeout(stage: &'static str, operation_id: String) -> Self {
        Self {
            code: crate::ERR_OPERATION_OUTCOME_UNKNOWN.into(),
            cause_code: crate::ERR_RUNTIME_TIMEOUT.into(),
            ..Self::timeout(stage, Some(operation_id), false)
        }
    }

    pub fn into_app_error(self) -> crate::AppError {
        let detail = crate::ErrorDetail::new(
            self.cause_code.as_ref(),
            self.stage.as_ref(),
            self.message.clone(),
        )
        .with_retryable(self.retryable);
        let mut error = match self.command_diagnostic {
            Some(diagnostic) => diagnostic.into_app_error(
                self.code.as_ref(),
                self.cause_code.as_ref(),
                self.stage.as_ref(),
                &self.message,
                self.retryable,
            ),
            None => crate::AppError::with_message(self.code.as_ref(), self.message)
                .with_error_detail(detail),
        };
        // Parent identity was captured before dispatch; never replace it with a child observation.
        if let Some(operation_id) = self.operation_id {
            error = error.with_operation_id(operation_id);
        }
        if let Some(blocker) = self.blocker {
            error = error.with_blocker(*blocker);
        }
        error
    }
}

impl From<String> for WakeFailure {
    fn from(message: String) -> Self {
        Self::new(crate::ERR_USERAPP_WAKE_FAILED, "wake", message)
    }
}
impl From<&str> for WakeFailure {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}
impl std::fmt::Display for WakeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A fresh runtime observation after a failed upstream connection. Cached
/// Running observations are intentionally bypassed at this recovery boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteWakeState {
    WakePending,
    Running,
    Unavailable,
}

/// UserApp 流量唤醒控制。代理在 stopped 或 starting 时调用
/// [`AppWakeControl::ensure_running`]，hold-and-wait 到 Running；
/// 并发请求由实现内部合流为一次 scale-up。
#[async_trait::async_trait]
pub trait AppWakeControl: Send + Sync {
    /// app 是否处于 stopped（scale replicas==0）。读内存表，O(1)，供 Pingora 快速短路。
    fn is_stopped(&self, app_id: &str) -> bool;

    /// Ensure a stopped application is running, subject to durable lifecycle
    /// admission and physical identity fences. 拍板 2026-09-23：手动 stop 与
    /// 闲置回收统一——被动流量（rcoder-proxy/文件转发）与显式动作
    /// （pod/ensure）共用本语义，有请求即唤醒。Local flags are advisory
    /// across replicas; the coordinator validates ownership under the
    /// operation lock before mutation. Concurrent callers share the same
    /// bounded wake result.
    async fn ensure_running(&self, app_id: &str) -> WakeOutcome;

    /// Remote stopped or starting status, cached to keep hot requests cheap.
    /// Probe errors do not authorize a wake and are not cached.
    async fn remote_wake_pending(&self, _app_id: &str) -> bool {
        false
    }

    /// 保留只读状态查询的失败原因；旧实现默认复用 bool advisory 接口。
    /// Err 不授权唤醒，不缓存为 running 或 starting。
    async fn remote_wake_pending_result(&self, app_id: &str) -> Result<bool, WakeFailure> {
        Ok(self.remote_wake_pending(app_id).await)
    }

    /// Bypass the advisory cache after an actual upstream connection failure.
    /// Error, missing workload and query failures are Unavailable, never wakeable.
    async fn remote_wake_state_fresh(&self, _app_id: &str) -> RemoteWakeState {
        RemoteWakeState::Unavailable
    }

    /// One budget covers lifecycle wake and connection recovery for a request.
    fn wake_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }
}

/// Userapp 活动状态的持久化行（AppActivityRegistry ↔ 存储后端的数据载体）
#[derive(Debug, Clone)]
pub struct ActivityRow {
    /// Userapp 应用 ID
    pub app_id: String,
    /// Captured lifecycle identity; delayed activity must never follow app_id reuse.
    pub lifecycle_id: String,
    pub lifecycle_epoch: i64,
    /// 最近真实 HTTP 访问时间（wall-clock；None=从未访问）
    pub last_accessed: Option<chrono::DateTime<chrono::Utc>>,
}

/// AppActivityRegistry 的影子持久化契约（跨 crate：app_manager 产出/消费，rcoder-storage 实现）
///
/// registry 本体保持内存单例语义（wake single-flight/RecycleTransition 是进程内协调机制），
/// 本契约只负责数据的跨重启持久化：flusher 周期批量落库、启动时全量加载回内存。
/// 实现须保证幂等（upsert）。
#[async_trait::async_trait]
pub trait ActivityPersistence: Send + Sync {
    /// 批量 upsert 活动状态行（flusher 每 ~5s 调用）
    async fn flush_batch(&self, rows: Vec<ActivityRow>) -> anyhow::Result<()>;

    /// 全量加载（启动时调用；空表返回空 Vec）
    async fn load_all(&self) -> anyhow::Result<Vec<ActivityRow>>;

    /// 删除单行（forget_app/delete_app 后调用）
    async fn delete(&self, app_id: &str, lifecycle_id: &str) -> anyhow::Result<()>;
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn wake_failure_stays_below_large_error_payload_threshold() {
        assert!(
            size_of::<WakeFailure>() < 128,
            "WakeFailure is {} bytes; diagnostic propagation must stay below the large error threshold",
            size_of::<WakeFailure>()
        );
    }

    #[test]
    fn wake_failure_keeps_original_code_phase_operation_and_blocker() {
        let blocker = crate::UserAppOperationBlocker {
            scope: crate::UserAppOperationScope::Prod,
            operation_id: "blocking-stop".into(),
            kind: crate::UserAppOperationKind::Stop,
            state: crate::UserAppOperationState::Running,
            step: "stop".into(),
        };
        let failure = WakeFailure {
            operation_id: Some("original-wake".into()),
            blocker: Some(Box::new(blocker.clone())),
            ..WakeFailure::new(
                crate::ERR_CONFLICT,
                "wake_admission",
                "Original admission denied",
            )
        };
        let result = failure.into_app_error().into_http_result::<()>("en-US");
        assert_eq!(result.code, crate::ERR_CONFLICT);
        assert_eq!(result.operation_id.as_deref(), Some("original-wake"));
        assert_eq!(result.blocker, Some(blocker));
        assert_eq!(result.error_detail.unwrap().stage, "wake_admission");
    }

    #[test]
    fn command_diagnostic_preserves_child_task_but_captured_parent_operation_wins() {
        let failure = WakeFailure {
            operation_id: Some("captured-parent-start".into()),
            command_diagnostic: Some(Box::new(crate::PgCommandDiagnostic {
                code: crate::ERR_RUNTIME_TIMEOUT.into(),
                operation_id: Some("downstream-db-command".into()),
                blocker: None,
                error_detail: Some(
                    crate::ErrorDetail::new(
                        crate::ERR_RUNTIME_TIMEOUT,
                        "database_reverify",
                        "Original database observation timed out",
                    )
                    .with_task_id("real-downstream-task")
                    .with_retryable(true),
                ),
            })),
            ..WakeFailure::new(
                crate::ERR_RECOVERY_REQUIRED,
                "credential_verification",
                "Password write was applied but not verified",
            )
        };
        let response = failure.into_app_error().into_http_result::<()>("en-US");
        assert_eq!(response.code, crate::ERR_RECOVERY_REQUIRED);
        assert_eq!(
            response.operation_id.as_deref(),
            Some("captured-parent-start")
        );
        let detail = response.error_detail.expect("downstream detail");
        assert_eq!(detail.task_id.as_deref(), Some("real-downstream-task"));
        assert_eq!(detail.stage, "database_reverify");
        assert!(!detail.retryable);
    }

    #[test]
    fn unknown_wake_timeout_is_not_a_replayable_readiness_timeout() {
        let failure =
            WakeFailure::unknown_timeout("wake_admission_outcome_unknown", "admitted-wake".into());
        let result = failure.into_app_error().into_http_result::<()>("en-US");
        assert_eq!(result.code, crate::ERR_OPERATION_OUTCOME_UNKNOWN);
        assert_eq!(result.operation_id.as_deref(), Some("admitted-wake"));
        let detail = result.error_detail.unwrap();
        assert_eq!(detail.reason_code, crate::ERR_RUNTIME_TIMEOUT);
        assert!(!detail.retryable);
    }
}
