//! dbx-web（DBX 数据库 GUI，容器内 supervisor 恒起 `DBX_PORT`=4224）只读就绪探测契约。
//!
//! 观察不是准入依据：不唤醒、不建 dev 容器、不刷新闲置计时——前端据
//! `data.ready` 决定 DB 面板呈现；容器唤醒由 dbx 代理路径的"有请求即唤醒"
//! 自然承担。语义对齐业务 readiness 族（查询成功恒 200，`ready` 才是可用）。

use crate::UserappStage;
use serde::{Deserialize, Serialize};
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

/// `GET /api/v1/userapp/{app_id}/{app_stage}/dbx/readiness` 响应体。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DbxReadinessResponse {
    /// 前端直接消费的布尔（= `status == Ready`）。
    pub ready: bool,
    pub status: DbxReadinessStatus,
    /// 结构化原因码（starting/unknown 时提供排障线索）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

/// 宿主注入的 dbx 探测器（app_manager 经 `set_dbx_prober` 装配；实现由
/// rcoder-engine 提供——经 runtime 观察定位实例，与 dbx 代理同源只读）。
#[async_trait::async_trait]
pub trait DbxReadinessProber: Send + Sync {
    /// 只读探测 dbx-web；实现不得唤醒、不得创建容器；预算内返回。
    async fn probe(&self, app_id: &str, stage: UserappStage) -> Result<DbxReadinessStatus, String>;
}
