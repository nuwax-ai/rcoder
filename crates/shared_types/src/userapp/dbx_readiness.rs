//! dbx-web（DBX 数据库 GUI，容器内 supervisor 恒起 `DBX_PORT`=4224）只读就绪探测契约。
//!
//! 观察不是准入依据：不唤醒、不建 dev 容器、不刷新闲置计时——前端据
//! `data.ready` 决定 DB 面板呈现；容器唤醒由 dbx 代理路径的"有请求即唤醒"
//! 自然承担。语义对齐业务 readiness 族（查询成功恒 200，`ready` 才是可用）。

use crate::{UserAppContainerReadiness, UserAppNoComputeState, UserappStage};
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

/// 探测器对计算资源的定位事实（内部透出给容器状态推导；与 4224 探测成败解耦）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbxComputeFact {
    /// 实例定位且复核稳定——容器物理在跑（不代表 DBX 已应答）。
    Located,
    /// 运行时报告无计算资源（保留六态细节，供 `/readiness` 同款容器推导）。
    NotRunning(UserAppNoComputeState),
    /// 定位前预算耗尽/定位失败，本次观察无计算资源事实。
    Unobserved,
}

/// `GET /api/v1/userapp/{app_id}/{app_stage}/dbx/readiness` 响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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
    /// 与 `/readiness` 同源的容器物理状态与控制操作回执（存储控制意图 + 运行时观察
    /// 双源合并）。与 DBX 探测成败解耦：容器 running 不代表 DBX 已应答。
    /// 旧响应缺该字段 = unknown（与 `UserAppReadinessResponse.container` 同款兼容约定）。
    #[serde(default)]
    pub container: UserAppContainerReadiness,
}

/// 内部观察结果；输出 ready 时统一从 status 派生，避免两个字段矛盾。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbxReadinessObservation {
    pub status: DbxReadinessStatus,
    pub reason_code: Option<DbxReadinessReason>,
    pub message: Option<String>,
    pub compute: DbxComputeFact,
}

impl DbxReadinessObservation {
    pub fn new(status: DbxReadinessStatus, reason_code: Option<DbxReadinessReason>) -> Self {
        Self {
            status,
            reason_code,
            message: None,
            compute: DbxComputeFact::Unobserved,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_compute_fact(mut self, compute: DbxComputeFact) -> Self {
        self.compute = compute;
        self
    }

    /// 组装响应（容器字段由调用方按存储控制意图推导后传入；`From` 拿不到控制侧输入）。
    pub fn into_response(self, container: UserAppContainerReadiness) -> DbxReadinessResponse {
        DbxReadinessResponse {
            ready: self.status == DbxReadinessStatus::Ready,
            status: self.status,
            reason_code: self.reason_code,
            message: self.message,
            container,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 兼容锁：旧响应（无 container 字段）必须可解码，容器视作 unknown——
    /// 与 `UserAppReadinessResponse.container` 的缺省语义同款。
    #[test]
    fn legacy_response_without_container_decodes_as_unknown() {
        let legacy = serde_json::json!({
            "ready": false,
            "status": "stopped",
            "reason_code": "COMPUTE_STOPPED",
            "message": "diag",
        });
        let response: DbxReadinessResponse = serde_json::from_value(legacy).unwrap();
        assert!(!response.ready);
        assert_eq!(response.status, DbxReadinessStatus::Stopped);
        assert_eq!(
            response.container.status,
            crate::UserAppContainerStatus::Unknown
        );
        assert!(response.container.operation.is_none());
        // 新响应序列化包含 container，且可无损往返。
        let value = serde_json::to_value(&response).unwrap();
        assert!(value.get("container").is_some());
        let back: DbxReadinessResponse = serde_json::from_value(value).unwrap();
        assert_eq!(back, response);
    }

    /// 观察构造默认无计算事实；builder 显式覆盖。
    #[test]
    fn observation_compute_fact_defaults_and_builder() {
        let observation = DbxReadinessObservation::new(DbxReadinessStatus::Starting, None);
        assert_eq!(observation.compute, DbxComputeFact::Unobserved);
        let observation = observation
            .with_compute_fact(DbxComputeFact::NotRunning(UserAppNoComputeState::Stopped))
            .with_message("m");
        assert_eq!(
            observation.compute,
            DbxComputeFact::NotRunning(UserAppNoComputeState::Stopped)
        );
        let response = observation.into_response(Default::default());
        assert_eq!(response.message.as_deref(), Some("m"));
        assert_eq!(
            response.container.status,
            crate::UserAppContainerStatus::Unknown
        );
    }
}
