//! `/api/computer` HTTP handlers (对齐 nuwax computerRoutes)。
//!
//! computer 工作区路径: `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}/`。
//!
//! 拆分: [`files`] (get-file-list / files-update / upload / import / delete-workspace) /
//! [`archive`] (zip-workspace / download-all-files) / [`workspace`] (create-workspace /
//! push-skills / init-project-template) / [`exec`] (execute-command / install-project /
//! get-logs / build-agent-package / cleanup-build-artifacts)。本 mod.rs 仅提供跨组共享 helper。

use std::path::PathBuf;

use crate::AppState;
use crate::error::AppError;
use crate::workspace::ComputerContext;
use serde::Deserialize;

use super::multipart::{file_field, text_field, validate_zip_ext};

/// 兼容整数 + 字符串 deserializer (Java 后端可能传 userId: 6 或 "6")。
/// 用法: #[serde(deserialize_with = "super::deserialize_id_string")]
pub(crate) fn deserialize_id_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(Error::custom("expected string or number")),
    }
}

pub(crate) mod archive;
pub(crate) mod exec;
pub(crate) mod files;
mod process_capture;
pub(crate) mod workspace;

// ── 跨组共享 helper (子模块经 super:: 访问) ──────────────────────────────────────

async fn ws_path(state: &AppState, user_id: &str, cid: &str) -> Result<PathBuf, AppError> {
    state
        .resolver
        .resolve_computer(&ComputerContext {
            user_id: user_id.to_string(),
            cid: cid.to_string(),
        })
        .await
}

/// computer 目标路径: `customTargetDir` trim 后非空则用之, 否则回退默认工作区 (对齐 nuwax)。
async fn resolve_computer_target(
    state: &AppState,
    user_id: &str,
    cid: &str,
    custom_target_dir: Option<&str>,
) -> Result<PathBuf, AppError> {
    let default_path = ws_path(state, user_id, cid).await?;
    Ok(
        match custom_target_dir.map(str::trim).filter(|s| !s.is_empty()) {
            Some(ct) => PathBuf::from(ct),
            None => default_path,
        },
    )
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserCidQuery {
    pub user_id: String,
    pub c_id: String,
    #[serde(default)]
    pub proxy_path: Option<String>,
    #[serde(default)]
    pub custom_target_dir: Option<String>,
}
