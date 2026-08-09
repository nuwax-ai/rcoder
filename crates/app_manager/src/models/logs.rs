use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppLogQueryRequest {
    #[serde(default)]
    pub selectors: Vec<AppLogSelector>,
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

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppLogSelector {
    pub service_id: String,
    #[serde(default)]
    pub source_ids: Vec<String>,
}
