//! 应用管理处理器

use std::sync::Arc;

use axum::{
    Json,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Multipart, Path, Query, State},
    response::Response,
};
use serde::Deserialize;
use tracing::{info, instrument};
use utoipa::ToSchema;

use shared_types::{AppError, HttpResult};

use super::models::*;

/// 应用状态（用于处理器）
#[derive(Clone)]
pub struct AppManagerState {
    pub app_service: Arc<dyn super::AppServiceTrait>,
}

/// 由运行时信息派生健康信息（AppRuntimeInfo → HealthInfo）
fn health_from_runtime(info: &AppRuntimeInfo) -> HealthInfo {
    HealthInfo {
        status: info.phase.clone(),
        instance: Some(InstanceInfo {
            name: format!(
                "{}-{}",
                shared_types::ServiceType::UserApp.container_prefix(),
                info.app_id
            ),
            phase: info.phase.clone(),
            ready: info.ready_replicas > 0,
            restart_count: info.restart_count,
            node: info.node.clone().unwrap_or_default(),
            ip: info.pod_ip.clone().unwrap_or_default(),
            started_at: info.started_at.clone(),
        }),
        probes: None,
    }
}

/// app 操作错误 → HTTP 响应错误（v2 §12）。
///
/// service 层返回强类型 [`AppOperationError`]（variant 携带错误码），handler 通过 From
/// 直接转换——错误码在 service 抛出点确定（Fail Fast），无需 downcast / 字符串匹配。
impl From<AppOperationError> for AppError {
    fn from(e: AppOperationError) -> Self {
        AppError::with_message(e.code(), e.message().to_string())
    }
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
    info!("[APP] creating app: {}", request.name);
    let app_info = state.app_service.create_app(request).await?;
    Ok(Json(HttpResult::success(app_info)))
}

/// 查询应用列表（实时查集群 + 过滤/分页；仅 status/app_ids 过滤生效）
#[utoipa::path(
    post,
    path = "/api/v1/apps/query",
    request_body = QueryAppsRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<PaginatedResponse<AppRuntimeInfo>>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn query_apps(
    State(state): State<Arc<AppManagerState>>,
    Json(request): Json<QueryAppsRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<AppRuntimeInfo>>>, AppError> {
    info!("[APP] querying apps");
    let response = state.app_service.query_apps(request).await?;
    Ok(Json(HttpResult::success(response)))
}

/// 对账接口：列出集群中所有 rcoder 托管的应用运行时状态
///
/// 供 Java 在 rcoder/自身重启后对账（rcoder 不持久化 app 元数据）。
#[utoipa::path(
    get,
    path = "/api/v1/apps/runtime",
    responses(
        (status = 200, description = "对账成功", body = HttpResult<Vec<AppRuntimeInfo>>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn list_app_runtimes(
    State(state): State<Arc<AppManagerState>>,
) -> Result<Json<HttpResult<Vec<AppRuntimeInfo>>>, AppError> {
    info!("[APP] reconcile: listing all app runtimes");
    let runtimes = state.app_service.list_app_runtimes().await?;
    Ok(Json(HttpResult::success(runtimes)))
}

/// 获取应用运行时详情（实时查集群）
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] getting app runtime: {}", app_id);
    let runtime = state.app_service.get_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 更新应用（全量替换 desired state）
///
/// rcoder 无状态：调用方需发送完整新状态（`image` 必填）。K8s SSA re-apply 幂等，
/// Docker 重建容器；工作空间目录保留。详见设计文档 §5.2。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/update",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = UpdateAppRequest,
    responses(
        (status = 200, description = "更新成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn update_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<UpdateAppRequest>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] updating app: {}", app_id);
    let runtime = state.app_service.update_app(&app_id, request).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 删除应用（默认保留持久存储；body `{"purge": true}` 一键连数据面一起清空）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = DeleteAppRequest,
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, body))]
pub async fn delete_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Option<Json<DeleteAppRequest>>,
) -> Result<Json<HttpResult<String>>, AppError> {
    let (purge, expected_rv) = body
        .map(|Json(r)| (r.purge.unwrap_or(false), r.expected_resource_version))
        .unwrap_or((false, None));
    info!("[APP] deleting app: {} (purge={})", app_id, purge);
    state
        .app_service
        .delete_app(&app_id, purge, expected_rv.as_deref())
        .await?;
    Ok(Json(HttpResult::success("删除成功".to_string())))
}

// ============================================================================
// 应用操作
// ============================================================================

/// 启动应用（scale replicas = 1）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/start",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "启动成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn start_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] starting app: {}", app_id);
    let runtime = state.app_service.start_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 停止应用（scale replicas = 0）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/stop",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "停止成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn stop_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] stopping app: {}", app_id);
    let runtime = state.app_service.stop_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 重启应用（rollout restart）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/restart",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "重启成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn restart_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] restarting app: {}", app_id);
    let runtime = state.app_service.restart_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime)))
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
    info!("[APP] getting app logs: {}", app_id);
    let logs = state.app_service.get_app_logs(&app_id, params).await?;
    Ok(Json(HttpResult::success(logs)))
}

/// 获取应用健康状态（由运行时状态派生）
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
    info!("[APP] getting app health: {}", app_id);
    let runtime = state.app_service.get_app(&app_id).await?;
    Ok(Json(HttpResult::success(health_from_runtime(&runtime))))
}

/// 获取应用资源使用（best-effort：restart_count 来自运行时；CPU/内存需 metrics-server）
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
    info!("[APP] getting app stats: {}", app_id);
    let stats = state.app_service.get_app_stats(&app_id).await?;
    Ok(Json(HttpResult::success(stats)))
}

/// 获取应用事件（best-effort：当前返回空，TODO 接 K8s events）
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
) -> Result<Json<HttpResult<Vec<container_runtime_api::AppEventInfo>>>, AppError> {
    info!("[APP] getting app events: {}", app_id);
    let events = state.app_service.get_app_events(&app_id).await?;
    Ok(Json(HttpResult::success(events)))
}

/// 文件日志查询参数
#[derive(Debug, Deserialize, ToSchema)]
pub struct FileLogQuery {
    /// 日志文件路径（app 根相对，如 "logs/app.log"）
    pub path: String,
    /// 返回最后 N 行（默认 100）
    pub tail: Option<u32>,
}

/// 读取应用文件日志（从 workspace PVC 读，适用不写 stdout 的应用）
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/logs/file",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("path" = String, Query, description = "日志文件路径（app 根相对，如 logs/app.log）"),
        ("tail" = Option<u32>, Query, description = "返回最后 N 行，默认 100")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<LogEntry>>),
        (status = 404, description = "文件/应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_file_logs(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Query(params): Query<FileLogQuery>,
) -> Result<Json<HttpResult<Vec<LogEntry>>>, AppError> {
    let tail = params.tail.unwrap_or(100);
    info!(
        "[APP] reading file logs: {} path={} tail={}",
        app_id, params.path, tail
    );
    let logs = state
        .app_service
        .get_app_file_logs(&app_id, &params.path, tail)
        .await?;
    Ok(Json(HttpResult::success(logs)))
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
    info!("[APP] uploading file: {}", app_id);

    let mut file_data: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut target_path: Option<String> = None;
    let mut flatten = false; // 压缩包上传：是否剥单层 wrapper 目录（默认 false 保留结构）

    // 解析 multipart 数据
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(&format!("failed to parse upload: {}", e)))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                let data = field.bytes().await.map_err(|e| {
                    AppError::bad_request(&format!("failed to read file data: {}", e))
                })?;
                file_data = Some(data.to_vec());
            }
            "target" => {
                let data = field.text().await.map_err(|e| {
                    AppError::bad_request(&format!("failed to read target path: {}", e))
                })?;
                target_path = Some(data);
            }
            "flatten" => {
                let data = field.text().await.map_err(|e| {
                    AppError::bad_request(&format!("failed to read flatten: {}", e))
                })?;
                flatten = data == "true" || data == "1";
            }
            _ => {}
        }
    }

    // 验证必需字段
    let data = file_data.ok_or_else(|| AppError::bad_request("missing file field"))?;
    let name = file_name.unwrap_or_else(|| "uploaded_file".to_string());
    let target = target_path.unwrap_or_else(|| format!("code/{}", name));

    let result = state
        .app_service
        .upload_file(&app_id, data, &target, flatten)
        .await?;

    Ok(Json(HttpResult::success(result)))
}

/// 列出文件查询参数
#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct ListFilesQuery {
    /// 子目录（相对 app 根，如 "code"/"data"/"logs"；默认列 app 根）
    pub path: Option<String>,
}

/// 列出文件
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/files",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("path" = Option<String>, Query, description = "子目录（相对 app 根，如 code/data/logs；默认列 app 根）")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<FileInfo>>),
        (status = 404, description = "应用/路径不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn list_files(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Query(q): Query<ListFilesQuery>,
) -> Result<Json<HttpResult<Vec<FileInfo>>>, AppError> {
    info!("[APP] listing files: {} (subpath={:?})", app_id, q.path);
    let files = state
        .app_service
        .list_files(&app_id, q.path.as_deref())
        .await?;
    Ok(Json(HttpResult::success(files)))
}

/// 删除文件请求
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeleteFileRequest {
    /// 文件路径（app 根相对，如 "code/app.jar"，可指向 code/data/logs 下任意文件）
    pub path: String,
}

/// 删除文件
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/files/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = DeleteFileRequest,
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 404, description = "文件/应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
pub async fn delete_file(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<DeleteFileRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] deleting file: {}/{}", app_id, request.path);
    state
        .app_service
        .delete_file(&app_id, &request.path)
        .await?;
    Ok(Json(HttpResult::success("文件删除成功".to_string())))
}

// ============================================================================
// 持久存储管理（v2 §5.4）
// ============================================================================

/// 查询应用持久存储状态
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/storage",
    params(("app_id" = String, Path, description = "应用 ID")),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<StorageInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<StorageInfo>>, AppError> {
    info!("[APP] getting app storage: {}", app_id);
    let info = state.app_service.get_app_storage(&app_id).await?;
    Ok(Json(HttpResult::success(info)))
}

/// 清空应用持久存储内容（留 PVC，可恢复；仅当 app 已 delete 时允许，否则 409 INVALID_STATE）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/storage/clear",
    params(("app_id" = String, Path, description = "应用 ID")),
    responses(
        (status = 200, description = "清空成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用仍存在，需先 delete", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn clear_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] clearing app storage: {}", app_id);
    state.app_service.clear_app_storage(&app_id).await?;
    Ok(Json(HttpResult::success("存储已清空".to_string())))
}

/// 销毁应用持久存储 PVC（高危·不可逆·释放配额；需 body `confirm=app_id`，仅 app 已 delete 后允许）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/storage/destroy",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = DestroyStorageRequest,
    responses(
        (status = 200, description = "PVC 已销毁", body = HttpResult<String>),
        (status = 400, description = "confirm 缺失/不匹配 app_id", body = HttpResult<String>),
        (status = 409, description = "应用仍存在，需先 delete", body = HttpResult<String>),
        (status = 500, description = "PVC 卡 Terminating，需运维介入（pvc-protection finalizer 未移除）", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, req))]
pub async fn destroy_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(req): Json<DestroyStorageRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] destroying app PVC: {}", app_id);
    state
        .app_service
        .destroy_app_storage(&app_id, &req.confirm)
        .await?;
    Ok(Json(HttpResult::success("PVC 已销毁，配额已释放".to_string())))
}

/// 重置 app 容器内 PG 密码（exec 容器内 psql ALTER USER，本地 trust 认证绕过当前密码）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/db/reset-password",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = ResetDbPasswordRequest,
    responses(
        (status = 200, description = "改密成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 400, description = "应用无 PG / 参数错误", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn reset_db_password(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<ResetDbPasswordRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] resetting PG password: {}", app_id);
    state
        .app_service
        .reset_db_password(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success("密码已重置".to_string())))
}

/// 新建 PG 库（exec 容器内 psql CREATE DATABASE）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/db/create-database",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = CreateDatabaseRequest,
    responses(
        (status = 200, description = "建库成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 400, description = "应用无 PG / 参数错误", body = HttpResult<String>),
        (status = 409, description = "库已存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn create_database(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<CreateDatabaseRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] creating database: {}", app_id);
    state
        .app_service
        .create_database(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success("数据库已创建".to_string())))
}

/// 分页查询持久存储（强制分页，无全量模式）
#[utoipa::path(
    post,
    path = "/api/v1/apps/storage/query",
    request_body = QueryStorageRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<PaginatedResponse<StorageInfo>>),
        (status = 400, description = "分页参数错误", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn query_storage(
    State(state): State<Arc<AppManagerState>>,
    Json(request): Json<QueryStorageRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<StorageInfo>>>, AppError> {
    info!(
        "[APP] querying storage list: page={} page_size={}",
        request.page, request.page_size
    );
    let resp = state.app_service.query_storage(request).await?;
    Ok(Json(HttpResult::success(resp)))
}

// ============================================================================
// 日志 WebSocket 流（v2 §11）
// ============================================================================

/// 日志流 query 参数
#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct LogStreamQuery {
    /// 起始历史行数（默认 0 = 仅 follow 新行）
    pub tail: Option<u32>,
}

/// 日志 WebSocket 流（follow）。WS 升级后服务端逐行推送 `LogEntry` JSON；
/// 客户端断开 → 服务端停止 follow（receiver drop → runtime 任务终止）。
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/logs/stream",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("tail" = Option<u32>, Query, description = "起始历史行数（0=仅 follow 新行）")
    ),
    responses(
        (status = 200, description = "WebSocket 升级成功；逐行推送 LogEntry JSON（见 get_app_logs 响应体）")
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, ws))]
pub async fn stream_app_logs(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    ws: WebSocketUpgrade,
    Query(params): Query<LogStreamQuery>,
) -> Result<Response, AppError> {
    let tail = params.tail.unwrap_or(0);
    info!("[APP] log WS stream: {} (tail={})", app_id, tail);
    let mut rx = state.app_service.stream_app_logs(&app_id, tail).await?;
    Ok(ws.on_upgrade(move |mut socket: WebSocket| async move {
        while let Some(entry) = rx.recv().await {
            let json = serde_json::to_string(&LogEntry {
                timestamp: entry.timestamp.unwrap_or_default(),
                stream: entry.stream,
                message: entry.message,
            })
            .unwrap_or_default();
            if socket.send(Message::Text(json.into())).await.is_err() {
                break; // 客户端断开
            }
        }
    }))
}
