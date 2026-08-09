use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const MAX_SERVICES: usize = 64;
pub const MAX_SOURCES: usize = 128;
pub const MAX_TAIL_PER_SOURCE: usize = 10_000;
pub const MAX_KEYWORD_BYTES: usize = 256;
pub const MAX_CURSOR_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogQueryRequest {
    #[serde(default)]
    pub selectors: Vec<LogSelector>,
    #[serde(default)]
    pub levels: Vec<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub tail: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogSelector {
    pub service_id: String,
    #[serde(default)]
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogSourceInfo {
    pub service_id: String,
    pub source_id: String,
    pub format: String,
    pub matched_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogRecord {
    pub service_id: String,
    pub source_id: String,
    pub file: String,
    pub offset: u64,
    pub timestamp: Option<String>,
    pub level: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceError {
    pub service_id: String,
    pub source_id: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogQueryResponse {
    pub logs: Vec<LogRecord>,
    pub source_errors: Vec<SourceError>,
    pub cursor: String,
    pub cursor_reset: bool,
}

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
