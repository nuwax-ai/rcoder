//! dbx-web（DBX 数据库 GUI，容器内 supervisor 恒起 `DBX_PORT`=4224）只读就绪探测契约。
//!
//! 观察不是准入依据：不唤醒、不建 dev 容器、不刷新闲置计时——前端据
//! `data.ready` 决定 DB 面板呈现；容器唤醒由 dbx 代理路径的"有请求即唤醒"
//! 自然承担。语义对齐业务 readiness 族（查询成功恒 200，`ready` 才是可用）。

use crate::UserappStage;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa::ToSchema;

/// dbx 就绪四态（前端渲染口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DbxReadinessStatus {
    /// 4224 已应答（任意 HTTP 状态码——GUI 起来就会应答，不苛求 200）。
    Ready,
    /// 容器在、dbx 未应答（含刚触发唤醒/启动窗口，前端重试）。
    Starting,
    /// prod 无计算资源（未部署/已停止）；dev builder 未建/已回收。
    Stopped,
    /// 定位/探测预算耗尽或内部错误。
    Unknown,
}

/// 稳定的观察原因；诊断详情放在 message，调用方不解析底层错误文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DbxReadinessReason {
    ComputeMissing,
    ComputeStopped,
    ComputeStarting,
    ComputeStopping,
    ComputeFailed,
    ComputeUnknown,
    DbxUnreachable,
    ObserveIncomplete,
    ObservationFailed,
    InstanceChanged,
    ProbeUnsupported,
    ProbeProtocolInvalid,
}

/// `GET /api/v1/userapp/{app_id}/{app_stage}/dbx/readiness` 响应体。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DbxReadinessResponse {
    /// 前端直接消费的布尔（= `status == Ready`）。
    pub ready: bool,
    /// 就绪状态：ready（HTTP 可达）、starting（启动中）、stopped（无运行实例）、unknown（无法确认）。
    pub status: DbxReadinessStatus,
    /// 结构化原因码：COMPUTE_MISSING（无实例）、COMPUTE_STOPPED（已停止）、
    /// COMPUTE_STARTING（启动中）、COMPUTE_STOPPING（停止中）、COMPUTE_FAILED（实例失败）、
    /// COMPUTE_UNKNOWN（实例状态未知）、DBX_UNREACHABLE（DBX 不可达）、
    /// OBSERVE_INCOMPLETE（观察超时）、OBSERVATION_FAILED（观察失败）、
    /// INSTANCE_CHANGED（实例换代）、PROBE_UNSUPPORTED（不支持探测）、
    /// PROBE_PROTOCOL_INVALID（探测响应无效）。ready 时省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<DbxReadinessReason>,
    /// 可选诊断详情；程序分支应使用 reason_code。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 内部观察结果；输出 ready 时统一从 status 派生，避免两个字段矛盾。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbxReadinessObservation {
    pub status: DbxReadinessStatus,
    pub reason_code: Option<DbxReadinessReason>,
    pub message: Option<String>,
}

impl DbxReadinessObservation {
    pub fn new(status: DbxReadinessStatus, reason_code: Option<DbxReadinessReason>) -> Self {
        Self {
            status,
            reason_code,
            message: None,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
}

impl From<DbxReadinessObservation> for DbxReadinessResponse {
    fn from(observation: DbxReadinessObservation) -> Self {
        Self {
            ready: observation.status == DbxReadinessStatus::Ready,
            status: observation.status,
            reason_code: observation.reason_code,
            message: observation.message,
        }
    }
}

/// 宿主注入的 dbx 探测器（app_manager 经 `set_dbx_prober` 装配；实现由
/// rcoder-engine 提供——经 runtime 观察定位实例，与 dbx 代理同源只读）。
#[async_trait::async_trait]
pub trait DbxReadinessProber: Send + Sync {
    /// 只读探测 dbx-web；实现不得唤醒、不得创建容器；预算内返回。
    async fn probe(
        &self,
        app_id: &str,
        stage: UserappStage,
        budget: Duration,
    ) -> Result<DbxReadinessObservation, String>;
}
