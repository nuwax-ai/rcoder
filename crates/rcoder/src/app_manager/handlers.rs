//! 应用管理处理器

use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, Query, State},
    Json,
};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::models::*;

/// 应用状态（用于处理器）
#[derive(Clone)]
pub struct AppManagerState {
    pub app_service: Arc<dyn super::AppServiceTrait>,
}

// ============================================================================
// 应用生命周期
// ============================================================================

/// 创建应用
#[utoipa::path(
    post,
    path = "/api/v1/apps",
    request_body = CreateAppRequest,
    responses(
        (status = 200, description = "创建成功", body = HttpResult<AppInfo>),
        (status = 400, description = "请求参数错误", body = HttpResult<String>),
        (status = 409, description = "应用已存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request), fields(app_name = %request.name))]
pub async fn create_app(
    State(state): State<Arc<AppManagerState>>,
    Json(request): Json<CreateAppRequest>,
) -> Result<Json<HttpResult<AppInfo>>, AppError> {
    info!("创建应用: {}", request.name);

    let app_info = state
        .app_service
        .create_app(request)
        .await
        .map_err(|e| AppError::internal_server_error(&e.to_string()))?;

    Ok(Json(HttpResult::success(app_info)))
}

/// 查询应用列表
#[utoipa::path(
    post,
    path = "/api/v1/apps/query",
    request_body = QueryAppsRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<PaginatedResponse<AppInfo>>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn query_apps(
    State(state): State<Arc<AppManagerState>>,
    Json(request): Json<QueryAppsRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<AppInfo>>>, AppError> {
    info!("查询应用列表");

    let response = state
        .app_service
        .query_apps(request)
        .await
        .map_err(|e| AppError::internal_server_error(&e.to_string()))?;

    Ok(Json(HttpResult::success(response)))
}

/// 获取应用详情
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<AppInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppInfo>>, AppError> {
    info!("获取应用详情: {}", app_id);

    let app_info = state
        .app_service
        .get_app(&app_id)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success(app_info)))
}

/// 更新应用配置
#[utoipa::path(
    put,
    path = "/api/v1/apps/{app_id}",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = UpdateAppRequest,
    responses(
        (status = 200, description = "更新成功", body = HttpResult<AppInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn update_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<UpdateAppRequest>,
) -> Result<Json<HttpResult<AppInfo>>, AppError> {
    info!("更新应用配置: {}", app_id);

    let app_info = state
        .app_service
        .update_app(&app_id, request)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success(app_info)))
}

/// 删除应用
#[utoipa::path(
    delete,
    path = "/api/v1/apps/{app_id}",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn delete_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("删除应用: {}", app_id);

    state
        .app_service
        .delete_app(&app_id)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success("删除成功".to_string())))
}

// ============================================================================
// 应用操作
// ============================================================================

/// 启动应用
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/start",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "启动成功", body = HttpResult<AppInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用已在运行", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn start_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppInfo>>, AppError> {
    info!("启动应用: {}", app_id);

    let app_info = state
        .app_service
        .start_app(&app_id)
        .await
        .map_err(|e| {
            if e.to_string().contains("已在运行") {
                AppError::conflict(&e.to_string())
            } else {
                AppError::not_found(&e.to_string())
            }
        })?;

    Ok(Json(HttpResult::success(app_info)))
}

/// 停止应用
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/stop",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "停止成功", body = HttpResult<AppInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用未在运行", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn stop_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppInfo>>, AppError> {
    info!("停止应用: {}", app_id);

    let app_info = state
        .app_service
        .stop_app(&app_id)
        .await
        .map_err(|e| {
            if e.to_string().contains("未在运行") {
                AppError::conflict(&e.to_string())
            } else {
                AppError::not_found(&e.to_string())
            }
        })?;

    Ok(Json(HttpResult::success(app_info)))
}

/// 重启应用
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/restart",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "重启成功", body = HttpResult<AppInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn restart_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppInfo>>, AppError> {
    info!("重启应用: {}", app_id);

    let app_info = state
        .app_service
        .restart_app(&app_id)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success(app_info)))
}

// ============================================================================
// 查询接口
// ============================================================================

/// 获取应用日志
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/logs",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("tail" = Option<u32>, Query, description = "返回最后 N 行"),
        ("follow" = Option<bool>, Query, description = "是否持续输出")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<LogEntry>>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_logs(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Query(params): Query<LogParams>,
) -> Result<Json<HttpResult<Vec<LogEntry>>>, AppError> {
    info!("获取应用日志: {}", app_id);

    let logs = state
        .app_service
        .get_app_logs(&app_id, params)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success(logs)))
}

/// 获取应用健康状态
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/health",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<HealthInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_health(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<HealthInfo>>, AppError> {
    info!("获取应用健康状态: {}", app_id);

    let app_info = state
        .app_service
        .get_app(&app_id)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success(app_info.health)))
}

/// 获取应用资源使用
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/stats",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<ResourceStats>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_stats(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<ResourceStats>>, AppError> {
    info!("获取应用资源使用: {}", app_id);

    let stats = state
        .app_service
        .get_app_stats(&app_id)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success(stats)))
}

/// 获取应用事件
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/events",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<String>>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_events(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<Vec<String>>>, AppError> {
    info!("获取应用事件: {}", app_id);

    let events = state
        .app_service
        .get_app_events(&app_id)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success(events)))
}

// ============================================================================
// 文件管理
// ============================================================================

/// 上传文件
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/upload",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body(content_type = "multipart/form-data", description = "上传文件"),
    responses(
        (status = 200, description = "上传成功", body = HttpResult<UploadResult>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, multipart))]
pub async fn upload_file(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<HttpResult<UploadResult>>, AppError> {
    info!("上传文件: {}", app_id);

    let mut file_data: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut target_path: Option<String> = None;

    // 解析 multipart 数据
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        AppError::bad_request(&format!("解析上传文件失败: {}", e))
    })? {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                let data = field.bytes().await.map_err(|e| {
                    AppError::bad_request(&format!("读取文件数据失败: {}", e))
                })?;
                file_data = Some(data.to_vec());
            }
            "target" => {
                let data = field.text().await.map_err(|e| {
                    AppError::bad_request(&format!("读取目标路径失败: {}", e))
                })?;
                target_path = Some(data);
            }
            _ => {
                // 忽略未知字段
            }
        }
    }

    // 验证必需字段
    let data = file_data.ok_or_else(|| AppError::bad_request("缺少 file 字段"))?;
    let name = file_name.unwrap_or_else(|| "uploaded_file".to_string());
    let target = target_path.unwrap_or_else(|| format!("code/{}", name));

    // 调用服务层上传文件
    let result = state
        .app_service
        .upload_file(&app_id, data, &target)
        .await
        .map_err(|e| AppError::internal_server_error(&e.to_string()))?;

    Ok(Json(HttpResult::success(result)))
}

/// 列出文件
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/files",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<FileInfo>>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn list_files(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<Vec<FileInfo>>>, AppError> {
    info!("列出文件: {}", app_id);

    let files = state
        .app_service
        .list_files(&app_id)
        .await
        .map_err(|e| AppError::not_found(&e.to_string()))?;

    Ok(Json(HttpResult::success(files)))
}

/// 删除文件
#[utoipa::path(
    delete,
    path = "/api/v1/apps/{app_id}/files/{path}",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("path" = String, Path, description = "文件路径")
    ),
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 404, description = "文件不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn delete_file(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, file_path)): Path<(String, String)>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("删除文件: {}/{}", app_id, file_path);

    state
        .app_service
        .delete_file(&app_id, &file_path)
        .await
        .map_err(|e| AppError::internal_server_error(&e.to_string()))?;

    Ok(Json(HttpResult::success("文件删除成功".to_string())))
}
