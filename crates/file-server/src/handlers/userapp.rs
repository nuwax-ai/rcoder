//! UserApp workspace HTTP handlers（独立 `/api/userapp`，复用 `resolve_project`）。
//!
//! - `POST /api/userapp/build`：workspace 多项目打包，返整体包 `workspace-package.zip`。
//! - `GET  /api/userapp/static/{app_id}/{*rest}`：取整体包（在 [`static_files`] 内，复用
//!   `serve_from_root` + COMPUTER_CORS）。
//!
//! 详见 `docs/userapp-development-design.md` §5。

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson;
use crate::extract::deserialize_id_string;
use crate::extract::deserialize_optional_id_string;
use crate::service::userapp;

/// `POST /api/userapp/build` 请求体。
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildUserAppBody {
    /// UserApp 标识（= workspace app_id = file-server project_id）。
    #[serde(deserialize_with = "deserialize_id_string")]
    pub app_id: String,
    /// 多租户三级目录（可选，留空走单级；对齐 resolve_project）。
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub tenant_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub space_id: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportProjectBody {
    #[serde(deserialize_with = "deserialize_id_string")]
    pub app_id: String,
    pub project_dir: String,
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub tenant_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub space_id: Option<String>,
}

/// `POST /api/userapp/build` —— workspace 多项目打包，返整体包 `workspace-package.zip`。
///
/// 流程（service::userapp::build_workspace_package）：
/// resolve_project(app_id) → 读 workspace.manifest → 遍历子项目 build_generic →
/// 组装成 `workspace-package.zip`（各子产物 + workspace 根 start.sh/scripts）。
///
/// Java 串后续步骤：`GET /api/userapp/static/{app_id}/workspace-package.zip` 取包 → upload →
/// `app_manager upload_from_url` + `create_app`。
#[utoipa::path(
    post,
    path = "/build",
    request_body = BuildUserAppBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn build_workspace(
    State(state): State<AppState>,
    AppJson(body): AppJson<BuildUserAppBody>,
) -> Result<Json<Value>, AppError> {
    let artifact = userapp::build_workspace_package(
        state.resolver.as_ref(),
        state.build_manager.as_ref(),
        &body.app_id,
        body.tenant_id.as_deref(),
        body.space_id.as_deref(),
        state.config.dev_command_timeout_secs,
        None,
    )
    .await?;

    tracing::info!(
        app_id = %body.app_id,
        package = %artifact.path.display(),
        "userapp workspace package built"
    );

    Ok(Json(json!({
        "success": true,
        "releaseId": artifact.release_id,
        "schemaVersion": 1,
        "artifact": {
            "path": artifact.file_name,
            "fileName": artifact.file_name,
            "sha256": artifact.sha256,
            "sizeBytes": artifact.size_bytes,
        }
    })))
}

#[utoipa::path(
    post,
    path = "/projects/detect",
    request_body = ImportProjectBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn detect_project(
    State(state): State<AppState>,
    AppJson(body): AppJson<ImportProjectBody>,
) -> Result<Json<Value>, AppError> {
    let workspace = state
        .resolver
        .resolve_project(&crate::workspace::ProjectContext {
            project_id: body.app_id,
            tenant_id: body.tenant_id,
            space_id: body.space_id,
            isolation_type: None,
        })
        .await?;
    let result = userapp::import::detect_project(&workspace, &body.project_dir).await?;
    Ok(Json(json!({"success": true, "detection": result})))
}

#[utoipa::path(
    post,
    path = "/projects/confirm",
    request_body = ImportProjectBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn confirm_project(
    State(state): State<AppState>,
    AppJson(body): AppJson<ImportProjectBody>,
) -> Result<Json<Value>, AppError> {
    let workspace = state
        .resolver
        .resolve_project(&crate::workspace::ProjectContext {
            project_id: body.app_id,
            tenant_id: body.tenant_id,
            space_id: body.space_id,
            isolation_type: None,
        })
        .await?;
    let path = userapp::import::confirm_project(&workspace, &body.project_dir).await?;
    Ok(Json(json!({"success": true, "path": path})))
}
