//! UserApp 日志域契约（rcoder ↔ app-cli 单一事实源）。
//!
//! `/api/v1/userapp/{app_id}/logs/*` 由 rcoder **透明转发**到容器内 app-cli
//! :3010 的 `/v1/logs/*`；此前请求体两端各写一份（AppLogQueryRequest vs
//! LogQueryRequest）、响应侧只有 app-cli 有结构——靠 wire 测试人肉对齐。
//! 本模块收敛为单一事实源：两端共用同一批类型，OpenAPI 文档由 utoipa
//! 直接派生出具体字段定义（Scalar/Swagger 可见）。
//!
//! wire 规约（锁死勿漂移）：**全 snake_case**（Java 消费契约）；请求侧
//! `deny_unknown_fields` 兜底 camelCase 回潮（caef1f5 断链教训）。
//!
//! 仅沉淀跨进程 wire 面；游标内部持久态（CursorState/SourceCursor/
//! FileCursor，checkpoint base64 的载荷）属 app-cli 私有实现，留在本端。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 单 manifest 服务数上限（app-cli 侧校验口径）
pub const MAX_SERVICES: usize = 64;
/// 单服务日志源数上限
pub const MAX_SOURCES: usize = 128;
/// 单源尾部行数上限
pub const MAX_TAIL_PER_SOURCE: usize = 10_000;
/// 关键字字节上限
pub const MAX_KEYWORD_BYTES: usize = 256;
/// 游标字符串字节上限
pub const MAX_CURSOR_BYTES: usize = 64 * 1024;

/// 日志查询/源查询通用请求体（logs/sources/query 与 logs/query 同构）
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LogQueryRequest {
    /// 服务/日志源选择器（空 = 全部服务全部源）
    #[serde(default)]
    pub selectors: Vec<LogSelector>,
    /// 日志级别过滤（如 ["WARN","ERROR"]；空 = 不过滤）
    #[serde(default)]
    pub levels: Vec<String>,
    /// 关键字过滤（可选，子串匹配）
    #[serde(default)]
    pub keyword: Option<String>,
    /// 起始时间过滤（可选，RFC3339）
    #[serde(default)]
    pub since: Option<String>,
    /// 结束时间过滤（可选，RFC3339）
    #[serde(default)]
    pub until: Option<String>,
    /// 每源尾部行数限制（可选；单源上限 10000；stream 首轮默认 100 行）
    #[serde(default)]
    pub tail: Option<usize>,
    /// 增量拉取游标（可选；上次 query 响应返回的 cursor，支持断点续拉）
    #[serde(default)]
    pub cursor: Option<String>,
}

/// 服务/日志源选择器
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LogSelector {
    /// 服务 ID（manifest 中声明的 service 名，如 "api"、"web"）
    pub service_id: String,
    /// 日志源 ID 列表（空 = 该服务全部源）
    #[serde(default)]
    pub source_ids: Vec<String>,
}

/// 日志源信息（logs/sources/query 响应 data 元素）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LogSourceInfo {
    /// 服务 ID
    pub service_id: String,
    /// 日志源 ID
    pub source_id: String,
    /// 日志格式（text / json）
    pub format: String,
    /// 该源匹配到的日志文件绝对路径列表（容器内路径视角）
    pub matched_files: Vec<String>,
}

/// 单条日志记录（logs/query 响应 data.logs 元素；SSE `log` 事件同构）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LogRecord {
    /// 服务 ID
    pub service_id: String,
    /// 日志源 ID
    pub source_id: String,
    /// 来源日志文件路径
    pub file: String,
    /// 行偏移量（续拉起点）
    pub offset: u64,
    /// 时间戳（RFC3339；纯文本格式日志无时间戳解析时为 null）
    pub timestamp: Option<String>,
    /// 日志级别（INFO/WARN/...；可空）
    pub level: Option<String>,
    /// 日志正文（单行）
    pub message: String,
}

/// 日志源读取错误（logs/query 响应 data.source_errors 元素；SSE `source_error` 同构）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SourceError {
    /// 服务 ID
    pub service_id: String,
    /// 日志源 ID
    pub source_id: String,
    /// 错误码
    pub code: String,
    /// 错误描述
    pub message: String,
}

/// 多服务日志快照（logs/query 响应 data）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LogQueryResponse {
    /// 本次命中的日志行
    pub logs: Vec<LogRecord>,
    /// 读取失败的日志源列表（同源失败 SSE 只报一次，这里每次快照都带）
    pub source_errors: Vec<SourceError>,
    /// 新游标（base64；回填下次请求的 cursor 可从断点续拉）
    pub cursor: String,
    /// 游标已失效（跨部署代/损坏）：客户端应丢弃本地 cursor 从 tail 重读
    pub cursor_reset: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 契约锁定：rcoder 原样转发到 app-cli /v1/logs/*——请求必须接受
    /// snake 键（caef1f5 曾因 camelCase+deny_unknown_fields 断链，带 selectors
    /// 的请求 400）。deny_unknown_fields 同时兜底 camel 漂移。
    #[test]
    fn log_query_request_accepts_snake_wire() {
        let req: LogQueryRequest = serde_json::from_str(
            r#"{"selectors":[{"service_id":"backend-go","source_ids":["application"]}],"tail":200}"#,
        )
        .expect("snake wire from rcoder must deserialize");
        assert_eq!(req.selectors.len(), 1);
        assert_eq!(req.selectors[0].service_id, "backend-go");
        assert_eq!(req.selectors[0].source_ids, vec!["application".to_string()]);

        // camel 键必须被拒（deny_unknown_fields 防 wire 回潮）
        assert!(
            serde_json::from_str::<LogQueryRequest>(r#"{"selectors":[{"serviceId":"x"}]}"#)
                .is_err()
        );
    }

    /// 响应 wire 锁定：service_id/source_errors/cursor_reset 全 snake（Java 契约）。
    #[test]
    fn log_query_response_serializes_snake_wire() {
        let resp = LogQueryResponse {
            logs: vec![LogRecord {
                service_id: "backend".into(),
                source_id: "application".into(),
                file: "app.log".into(),
                offset: 1,
                timestamp: None,
                level: Some("info".into()),
                message: "hi".into(),
            }],
            source_errors: vec![SourceError {
                service_id: "backend".into(),
                source_id: "application".into(),
                code: "E".into(),
                message: "m".into(),
            }],
            cursor: "c".into(),
            cursor_reset: true,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""service_id""#), "{json}");
        assert!(json.contains(r#""source_errors""#), "{json}");
        assert!(json.contains(r#""cursor_reset""#), "{json}");
        assert!(!json.contains("serviceId"), "camel residue: {json}");
    }
}
