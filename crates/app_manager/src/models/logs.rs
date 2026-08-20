//! 应用日志查询模型（rcoder 转发到 app 容器内 app-cli :3010 的 /v1/logs/* API）

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 应用日志查询请求（sources/query、query、stream 三个端点共用）
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AppLogQueryRequest {
    /// 服务/日志源选择器（空 = 全部服务全部源）
    pub selectors: Vec<AppLogSelector>,
    /// 日志级别过滤（如 ["WARN","ERROR"]；空 = 不过滤）
    pub levels: Vec<String>,
    /// 关键字过滤（可选，子串匹配）
    pub keyword: Option<String>,
    /// 起始时间过滤（可选，RFC3339）
    pub since: Option<String>,
    /// 结束时间过滤（可选，RFC3339）
    pub until: Option<String>,
    /// 每源尾部行数限制（可选；app-cli 单源上限 10000）
    pub tail: Option<usize>,
    /// 增量拉取游标（可选；上次 query 响应返回的 cursor，支持断点续拉）
    pub cursor: Option<String>,
}

/// 服务/日志源选择器
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AppLogSelector {
    /// 服务 ID（manifest 中声明的 service 名，如 "api"、"web"）
    pub service_id: String,
    /// 日志源 ID 列表（空 = 该服务全部源）
    pub source_ids: Vec<String>,
}
