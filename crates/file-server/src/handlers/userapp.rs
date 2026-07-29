//! UserApp workspace HTTP handlers（独立 `/api/userapp`，复用 `resolve_project`）。
//!
//! - `POST /api/userapp/build`：workspace 多项目打包，返整体包 `workspace-package.zip`。
//! - `GET  /api/userapp/static/{app_id}/{*rest}`：取整体包（在 [`static_files`] 内，复用
//!   `serve_from_root` + COMPUTER_CORS）。
//!
//! 详见 `docs/userapp-development-design.md` §5。

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::extract::deserialize_id_string;
use crate::extract::deserialize_optional_id_string;
use crate::extract::AppJson;
use crate::service::userapp::{self, WORKSPACE_PACKAGE_ZIP};
use crate::AppState;

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
    let pkg = userapp::build_workspace_package(
        state.resolver.as_ref(),
        state.build_manager.as_ref(),
        &body.app_id,
        body.tenant_id.as_deref(),
        body.space_id.as_deref(),
        state.config.dev_command_timeout_secs,
    )
    .await?;

    tracing::info!(
        app_id = %body.app_id,
        package = %pkg.display(),
        "userapp workspace package built"
    );

    Ok(Json(json!({
        "success": true,
        "artifact": {
            "path": WORKSPACE_PACKAGE_ZIP,
            "fileName": WORKSPACE_PACKAGE_ZIP,
        }
    })))
}
