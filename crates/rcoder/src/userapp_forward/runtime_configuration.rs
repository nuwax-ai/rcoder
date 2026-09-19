//! Save-only configuration API. This module deliberately has no runtime, wake,
//! exec or app-service calls: success confirms a committed configuration version.
use crate::{AppError, HttpResult, router::AppState};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use shared_types::{
    RuntimeConfigurationStatus, SaveRuntimeConfigurationRequest, SavedRuntimeConfiguration,
    UserAppOperationScope, UserAppStoreError,
};
use std::sync::Arc;

#[derive(serde::Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct ConfigurationQuery {
    /// Exact current lifecycle. A recreated application cannot inherit this query.
    lifecycle_id: String,
}
fn validate(app_id: &str, lifecycle_id: &str) -> Result<(), AppError> {
    shared_types::validate_identifier(app_id, "app_id").map_err(|e| AppError::bad_request(&e))?;
    shared_types::validate_identifier(lifecycle_id, "lifecycle_id")
        .map_err(|e| AppError::bad_request(&e))
}
pub(super) fn error(error: UserAppStoreError) -> AppError {
    match error {
        UserAppStoreError::NotFound => {
            AppError::not_found("Application configuration target was not found")
        }
        UserAppStoreError::LifecycleConflict | UserAppStoreError::OwnershipConflict => {
            AppError::conflict(
                "Application lifecycle changed; refresh its identity before saving configuration",
            )
        }
        UserAppStoreError::VersionConflict => AppError::conflict(
            "Runtime configuration revision changed; reload the configuration status",
        ),
        UserAppStoreError::OperationInProgress(blocker) => {
            AppError::conflict("A conflicting application operation is in progress")
                .with_operation_id(blocker.operation_id.clone())
                .with_blocker(blocker)
        }
        UserAppStoreError::InvalidOperation(message) => AppError::bad_request(&message),
        // Driver diagnostics can include a failed row. Never return or log private
        // credential-table values; original request identity permits safe replay.
        UserAppStoreError::Storage(_) => AppError::with_message(
            shared_types::error_codes::ERR_BACKEND_ERROR,
            "Runtime configuration storage is unavailable; retry using the original request identity",
        ),
    }
}

#[utoipa::path(
    summary = "保存生产运行配置",
    put, path = "/api/v1/userapp/{app_id}/prod/runtime-configuration",
    operation_id = "userapp_save_prod_runtime_configuration", tag = "Userapp · 双态 · 数据库",
    params(("app_id" = String, Path, description = "Application ID; user_id is not used")),
    request_body = SaveRuntimeConfigurationRequest,
    responses(
        (status = 200, description = "HttpResult：成功仅表示配置保存，待下次显式启动生效；错误检查 code/message。ERR_VALIDATION 输入错误，ERR_NOT_FOUND 应用不存在，ERR_CONFLICT 生命周期/版本冲突，ERR_BACKEND_ERROR 存储结果未知", body = HttpResult<SavedRuntimeConfiguration>)
    )
)]
pub(crate) async fn save(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(request): Json<SaveRuntimeConfigurationRequest>,
) -> Result<HttpResult<SavedRuntimeConfiguration>, AppError> {
    validate(&app_id, &request.lifecycle_id)?;
    let result = state
        .userapp_runtime_configuration
        .save_runtime_configuration(&app_id, UserAppOperationScope::Prod, &request)
        .await
        .map_err(error)?;
    Ok(HttpResult::success(result))
}

#[utoipa::path(
    summary = "查询生产运行配置版本",
    get, path = "/api/v1/userapp/{app_id}/prod/runtime-configuration",
    operation_id = "userapp_get_prod_runtime_configuration", tag = "Userapp · 双态 · 数据库",
    params(("app_id" = String, Path, description = "Application ID"), ConfigurationQuery),
    responses(
        (status = 200, description = "HttpResult：成功返回无密码的版本状态，data=null 表示未保存；错误检查 ERR_VALIDATION、ERR_NOT_FOUND、ERR_CONFLICT、ERR_BACKEND_ERROR", body = HttpResult<Option<RuntimeConfigurationStatus>>)
    )
)]
pub(crate) async fn status(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Query(query): Query<ConfigurationQuery>,
) -> Result<HttpResult<Option<RuntimeConfigurationStatus>>, AppError> {
    validate(&app_id, &query.lifecycle_id)?;
    let result = state
        .userapp_runtime_configuration
        .runtime_configuration_status(&app_id, &query.lifecycle_id, UserAppOperationScope::Prod)
        .await
        .map_err(error)?;
    Ok(HttpResult::success(result))
}
