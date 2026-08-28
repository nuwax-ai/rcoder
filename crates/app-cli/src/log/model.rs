//! 日志域 wire 模型。
//!
//! 请求/响应 DTO 已下沉 `shared_types::app_cli_logs`（跨进程契约单一事实源，
//! rcoder 的 OpenAPI 文档 schema 同源派生）；本文件 re-export 保持既有
//! 引用点稳定，游标内部持久态（checkpoint base64 载荷）仍是 app-cli 私有。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use shared_types::{
    LogQueryRequest, LogQueryResponse, LogRecord, LogSelector, LogSourceInfo, MAX_CURSOR_BYTES,
    MAX_KEYWORD_BYTES, MAX_SERVICES, MAX_SOURCES, MAX_TAIL_PER_SOURCE, SourceError,
};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CursorState {
    pub boot_id: String,
    pub sources: BTreeMap<String, SourceCursor>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SourceCursor {
    pub files: BTreeMap<String, FileCursor>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileCursor {
    pub file: String,
    pub offset: u64,
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// wire 契约锁定：rcoder（AppLogQueryRequest，snake）原样转发到本端点——
    /// 请求必须接受 snake 键（caef1f5 曾因 camelCase+deny_unknown_fields 断链，
    /// 带 selectors 的请求 400）。deny_unknown_fields 同时兜底 camel 漂移。
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
