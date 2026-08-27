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

use crate::ops::multipart::validate_zip_ext;

// ID 字段反序列化 helper (deserialize_id_string / deserialize_optional_id_string) 已提升至
// `crate::extract`, 供 computer / project 等所有 handler 共用。

pub mod archive;
pub mod exec;
pub mod files;
pub mod files_read;
pub mod packages;
pub mod workspace;

// ── 跨组共享 helper (子模块经 super:: 访问) ──────────────────────────────────────

async fn ws_path(state: &AppState, user_id: &str, cid: &str) -> Result<PathBuf, AppError> {
    computer_root_for_request(state, user_id, cid).await
}

/// computer 域请求根目录（userApp 分流单头收口——ws_path 与静态文件共用，
/// 消除两处独立 if 的漂移面）。
///
/// userApp 分流（X-Service-Type=userapp，经反向代理/rcoder 拦截层透传）：
/// workspace 从 computer 定位 `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}` 切到
/// 开发卷 `{USERAPP_WORKSPACE_DIR}/{cId}`（cId=app_id；本容器即该 app 的开发容器）。
pub(crate) async fn computer_root_for_request(
    state: &AppState,
    user_id: &str,
    cid: &str,
) -> Result<PathBuf, AppError> {
    if crate::extract::is_userapp_request() {
        return crate::workspace::resolve_userapp_dev(cid, None, &state.config);
    }
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
