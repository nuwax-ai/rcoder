//! 会话通知路径参数 DTO（从 agent_session_notification.rs 拆出）。
//!
//! pod_id/tenant_id/space_id/isolation_type 是 API 契约预留参数（前端可传，
//! 当前服务端未消费），不可删除——dead_code 压制随结构体同走。

use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
// pod_id/tenant_id/space_id/isolation_type 是 API 契约预留参数（前端可传，
// 当前服务端未消费），不可删除，故显式压制 dead_code 警告
#[allow(dead_code)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
    /// Pod ID，用于共享容器模式下的容器定位（可选）
    #[param(example = "pod_abc123")]
    #[serde(default)]
    pub pod_id: Option<String>,
    /// 租户ID（可选）
    #[param(example = "tenant_001")]
    #[serde(
        default,
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    pub tenant_id: Option<String>,
    /// 空间ID（可选）
    #[param(example = "space_001")]
    #[serde(
        default,
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    pub space_id: Option<String>,
    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[param(example = "project")]
    #[serde(default)]
    pub isolation_type: Option<String>,
    /// 客户端消费游标（可选）：重连时带上最后收到的 seq（`ProgressEvent.seq`），
    /// rcoder 只补齐 seq > last_seq 的消息（增量补齐，消除重复）。
    /// 缺省 = 补齐该 session 全量历史（首次连接合理；重连建议前端带上）。
    #[param(example = "12")]
    #[serde(default)]
    pub last_seq: Option<u64>,
}
