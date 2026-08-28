//! 文件管理 handler（upload / list / delete）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Multipart, Path, Query, State},
};
use garde::Validate as _;
use serde::Deserialize;
use tracing::{info, instrument};
use utoipa::ToSchema;

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{FileInfo, UploadResult};

/// multipart 上传表单 schema（OpenAPI-only——handler 手解析字段，结构此单源声明）。
#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct AppUploadForm {
    /// 归属用户 ID（必填，白名单校验；dev 容器懒创建宿主树分区依据 + 审计）
    pub user_id: String,
    /// 目标路径（app 根相对；单文件=文件路径如 code/app.jar，压缩包=解压目录如 code/；缺省 code/{文件名}）
    pub target: Option<String>,
    /// 压缩包是否剥单层 wrapper 目录（"true"/"1" 生效；默认 false 保留结构）
    pub flatten: Option<String>,
    #[schema(format = Binary)]
    /// 上传文件（zip/tar.gz 自动解压；必填）
    pub file: String,
}

/// 上传文件
///
/// multipart 直传到目标环境数据卷（prod=运行容器 /app 根；dev=开发容器 workspace
/// 根）。字段：`file`（必填，二进制内容）、`target`（根相对落盘路径，缺省
/// `code/{文件名}`）、`flatten`（zip/tar.gz 时是否剥掉单层 wrapper 目录，
/// "true"/"1" 生效；默认 false 保留原结构）。
/// 压缩包自动识别并解压；单文件直写。typical：发版前的静态资源/配置文件补充。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/upload",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder，target 相对 workspace 根）/ `prod`=生产运行容器（target 相对 /app 根）")
    ),
    request_body(content = AppUploadForm, content_type = "multipart/form-data", description = "上传文件（multipart：user_id 必填）"),
    responses(
        (status = 200, description = "上传成功", body = HttpResult<UploadResult>),
        (status = 400, description = "multipart 解析失败 / 缺 file 或 user_id 字段 / app_stage 非法", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 502, description = "app_stage=dev 开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state, multipart))]
pub async fn upload_file(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<Json<HttpResult<UploadResult>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    info!(
        "[APP] uploading file: {} (app_stage={})",
        app_id,
        app_stage.as_str()
    );

    let mut file_data: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut target_path: Option<String> = None;
    let mut flatten = false; // 压缩包上传：是否剥单层 wrapper 目录（默认 false 保留结构）
    let mut user_id: Option<String> = None;

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
            "user_id" => {
                let data = field.text().await.map_err(|e| {
                    AppError::bad_request(&format!("failed to read user_id: {}", e))
                })?;
                user_id = Some(data);
            }
            _ => {}
        }
    }

    // 验证必需字段
    let data = file_data.ok_or_else(|| AppError::bad_request("missing file field"))?;
    let name = file_name.unwrap_or_else(|| "uploaded_file".to_string());
    let target = target_path.unwrap_or_else(|| format!("code/{}", name));
    let user_id = user_id.ok_or_else(|| AppError::bad_request("missing user_id field"))?;
    shared_types::identifier(&user_id, &()).map_err(|e| AppError::bad_request(&e.to_string()))?;

    let result = state
        .app_service
        .upload_file(app_stage, &app_id, &user_id, data, &target, flatten)
        .await?;

    Ok(Json(HttpResult::success(result)))
}

/// 从 URL 下载文件请求
#[derive(Debug, Deserialize, ToSchema, garde::Validate)]
pub struct UploadFromUrlRequest {
    /// 归属用户 ID（必填，白名单校验；dev 容器懒创建时宿主树
    /// `dev/{user_id}/{app_id}` 分区依据）
    #[garde(custom(shared_types::identifier))]
    pub user_id: String,
    /// 下载 URL（HTTP/HTTPS；允许内网 IP、localhost、集群域名和普通公网域名）
    #[garde(skip)]
    pub url: String,
    /// 目标路径（app 根相对；单文件=文件路径，压缩包=解压目录如 "code/"；默认 "code/"）
    #[garde(skip)]
    pub target: Option<String>,
    /// 压缩包是否剥单层 wrapper 目录（默认 false）
    #[garde(skip)]
    pub flatten: Option<bool>,
}

/// 从 URL 下载文件/压缩包并上传
///
/// 服务端代下载后落盘到应用数据卷——省去本地中转一步（制品库直连发布场景：
/// Java 把 build 产物 URL 直接喂进来）。允许内网 IP / localhost / 集群域名，
/// 仅要求 HTTP(S)；压缩包按魔数自动解压，语义同 [`upload_file`]（target/
/// flatten 可选，缺省 `code/`、不剥层）。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/upload-from-url",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器 / `prod`=生产运行容器（target 根基准同 upload）")
    ),
    request_body = UploadFromUrlRequest,
    responses(
        (status = 200, description = "下载并上传成功", body = HttpResult<UploadResult>),
        (status = 400, description = "URL 非法或不是 HTTP(S) / app_stage 非法", body = HttpResult<String>),
        (status = 502, description = "app_stage=dev 开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state))]
pub async fn upload_from_url(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Json(req): Json<UploadFromUrlRequest>,
) -> Result<Json<HttpResult<UploadResult>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    info!(
        "[APP] upload from url: {} (app_stage={}, url={})",
        app_id,
        app_stage.as_str(),
        req.url
    );
    req.validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    let target = req.target.clone().unwrap_or_else(|| "code/".to_string());
    let flatten = req.flatten.unwrap_or(false);
    let result = state
        .app_service
        .upload_from_url(app_stage, &app_id, &req.user_id, &req.url, &target, flatten)
        .await?;
    Ok(Json(HttpResult::success(result)))
}

/// 列出文件查询参数
#[derive(Debug, Deserialize, ToSchema, garde::Validate)]
pub struct ListFilesQuery {
    /// 归属用户 ID（必填，白名单校验；dev 容器懒创建时宿主树
    /// `dev/{user_id}/{app_id}` 分区依据）
    #[garde(custom(shared_types::identifier))]
    pub user_id: String,
    /// 子目录（相对 app 根，如 "code"/"data"/"logs"；默认列 app 根）
    #[garde(skip)]
    pub path: Option<String>,
}

/// 列出文件
///
/// 列应用数据卷内指定子目录的文件清单（名称/大小/mtime 等元信息，非内容）。
/// `path` 相对 app 根（如 "code"/"data"/"logs"），缺省列 app 根一层；
/// 递归遍历请逐层下钻或配合 file-server 文件镜像族接口使用。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/files",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（根=workspace）/ `prod`=生产运行容器（根=/app）"),
        ("user_id" = String, Query, description = "归属用户 ID（必填，白名单校验；dev 容器懒创建宿主树分区依据 + 审计）"),
        ("path" = Option<String>, Query, description = "子目录（相对环境根，如 code/data/logs；默认列根）")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<FileInfo>>),
        (status = 400, description = "app_stage 非法", body = HttpResult<String>),
        (status = 404, description = "应用/路径不存在", body = HttpResult<String>),
        (status = 502, description = "app_stage=dev 开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state))]
pub async fn list_files(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(q): Query<ListFilesQuery>,
) -> Result<Json<HttpResult<Vec<FileInfo>>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    q.validate().map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] listing files: {} (app_stage={}, user_id={}, subpath={:?})",
        app_id,
        app_stage.as_str(),
        q.user_id,
        q.path
    );
    let files = state
        .app_service
        .list_files(app_stage, &app_id, &q.user_id, q.path.as_deref())
        .await?;
    Ok(Json(HttpResult::success(files)))
}

/// 删除文件请求
#[derive(Debug, Deserialize, ToSchema, garde::Validate)]
pub struct DeleteFileRequest {
    /// 归属用户 ID（必填，白名单校验；dev 容器懒创建时宿主树
    /// `dev/{user_id}/{app_id}` 分区依据）
    #[garde(custom(shared_types::identifier))]
    pub user_id: String,
    /// 文件路径（app 根相对，如 "code/app.jar"，可指向 code/data/logs 下任意文件）
    #[garde(skip)]
    pub path: String,
}

/// 删除文件
///
/// 按路径删除应用数据卷内的单个文件（app 根相对，可指向 code/data/logs 下
/// 任意文件）；目录递归清理不在此面——危险操作走 storage/clear。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/files/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（workspace 根相对）/ `prod`=生产运行容器（/app 根相对）")
    ),
    request_body = DeleteFileRequest,
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 400, description = "app_stage 非法", body = HttpResult<String>),
        (status = 404, description = "文件/应用不存在", body = HttpResult<String>),
        (status = 502, description = "app_stage=dev 开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
pub async fn delete_file(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Json(request): Json<DeleteFileRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    request
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] deleting file: {}/{} (app_stage={}, user_id={})",
        app_id,
        request.path,
        app_stage.as_str(),
        request.user_id
    );
    state
        .app_service
        .delete_file(app_stage, &app_id, &request.user_id, &request.path)
        .await?;
    Ok(Json(HttpResult::success("文件删除成功".to_string())))
}
