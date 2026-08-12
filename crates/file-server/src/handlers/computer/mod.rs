//! `/api/computer` HTTP handlers (对齐 nuwax computerRoutes)。
//!
//! computer 工作区路径: `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}/`。
//!
//! 拆分: [`files_read`] (get-file-list / resolve-file / search-files) /
//! [`files`] (files-update / upload / generate-file / import-project / delete-workspace) /
//! [`archive`] (zip-workspace / download-all-files) / [`workspace`] (create-workspace /
//! push-skills / init-project-template) / [`exec`] (execute-command / get-logs) /
//! [`packages`] (install-project / build-agent-package / cleanup-build-artifacts)。
//! 本 mod.rs 仅提供跨组共享 helper。

use std::path::PathBuf;

use crate::AppState;
use crate::error::AppError;
use crate::workspace::ComputerContext;
use garde::Validate;
use serde::Deserialize;

use super::multipart::{file_field, text_field, validate_zip_ext};

// ID 字段反序列化 helper (deserialize_id_string / deserialize_optional_id_string) 已提升至
// `crate::extract`, 供 computer / project 等所有 handler 共用。

pub(crate) mod archive;
pub(crate) mod exec;
pub(crate) mod files;
pub(crate) mod files_read;
pub(crate) mod packages;
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
///
/// 注: `customTargetDir` 完全信任调用方, **不做根目录白名单限制**:
/// 产品运行于容器内、内网私有化部署, 且用户客户端复用本 file-server 模块逻辑,
/// 每个用户电脑上的路径各不相同, 限制根路径会误伤正常业务;
/// 其内部相对路径仍由 [`crate::path_safety::ensure_within`] 防逃逸。
async fn resolve_computer_target(
    state: &AppState,
    user_id: &str,
    cid: &str,
    custom_target_dir: Option<&str>,
) -> Result<PathBuf, AppError> {
    let default_path = ws_path(state, user_id, cid).await?;
    match custom_target_dir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ct) => Ok(PathBuf::from(ct)),
        None => Ok(default_path),
    }
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserCidQuery {
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
}

/// `get-file-list` 查询参数: 在 `UserCidQuery` 基础上新增 `relativePath` / `recursive`
/// (对齐 TS commit ba08d0c)。缺省 `recursive=true` (原全量递归), 向后兼容。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileListQuery {
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    #[serde(default)]
    #[garde(skip)]
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
    /// 相对工作区根的子目录 (可多级), 空 → 列根目录。
    #[serde(default)]
    #[garde(skip)]
    pub relative_path: Option<String>,
    /// 是否递归扁平列出; 默认 true。显式传 "false" → 仅当前目录一层。
    /// 用 String 接收以对齐 TS `recursive === false || recursive === "false"` 语义。
    #[serde(default)]
    #[garde(skip)]
    pub recursive: Option<String>,
}

/// `resolve-file` 查询参数 (对齐 TS resolveExistingFile)。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolveFileQuery {
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    #[serde(default)]
    #[garde(skip)]
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub file_path: String,
}

/// `search-files` 查询参数 (对齐 TS searchFiles)。
/// `limit` / `max_visit` / `timeout_ms` 用 String 接收, 经 garde `positive_int`
/// 校验正整数, 对齐 TS `requirePositiveInt` (由 Java 网关传入, 不设默认值)。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchFilesQuery {
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    #[serde(default)]
    #[garde(skip)]
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub relative_path: Option<String>,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub kw: String,
    #[garde(custom(crate::validation_rules::positive_int))]
    pub limit: String,
    #[garde(custom(crate::validation_rules::positive_int))]
    pub max_visit: String,
    #[garde(custom(crate::validation_rules::positive_int))]
    pub timeout_ms: String,
}
