//! 文件管理 handler（upload / list / delete）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Multipart, Path, Query, State},
};
use serde::Deserialize;
use tracing::{info, instrument};
use utoipa::ToSchema;

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{FileInfo, UploadResult};

/// 上传文件
///
/// multipart 直传到目标环境数据卷（prod=运行容器 /app 根；dev=开发容器 workspace
/// 根）。字段：`file`（必填，二进制内容）、`target`（根相对落盘路径，缺省
/// `code/{文件名}`）、`flatten`（zip/tar.gz 时是否剥掉单层 wrapper 目录，
/// "true"/"1" 生效；默认 false 保留原结构）。
/// 压缩包自动识别并解压；单文件直写。typical：发版前的静态资源/配置文件补充。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{env}/upload",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("env" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder，target 相对 workspace 根）/ `prod`=生产运行容器（target 相对 /app 根）")
    ),
    request_body(content_type = "multipart/form-data", description = "上传文件"),
    responses(
        (status = 200, description = "上传成功", body = HttpResult<UploadResult>),
        (status = 400, description = "multipart 解析失败 / 缺 file 字段 / env 非法", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 502, description = "env=dev 开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state, multipart))]
pub async fn upload_file(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, env)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<Json<HttpResult<UploadResult>>, AppError> {
    let env = shared_types::UserappEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_env_error(&env)))?;
    info!("[APP] uploading file: {} (env={})", app_id, env.as_str());

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
        .upload_file(env, &app_id, data, &target, flatten)
        .await?;

    Ok(Json(HttpResult::success(result)))
}

/// 从 URL 下载文件请求
#[derive(Debug, Deserialize, ToSchema)]
pub struct UploadFromUrlRequest {
    /// 下载 URL（HTTP/HTTPS；允许内网 IP、localhost、集群域名和普通公网域名）
    pub url: String,
    /// 目标路径（app 根相对；单文件=文件路径，压缩包=解压目录如 "code/"；默认 "code/"）
    pub target: Option<String>,
    /// 压缩包是否剥单层 wrapper 目录（默认 false）
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
    path = "/api/v1/userapp/{app_id}/{env}/upload-from-url",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("env" = String, Path, description = "目标环境：`dev`=开发容器 / `prod`=生产运行容器（target 根基准同 upload）")
    ),
    request_body = UploadFromUrlRequest,
    responses(
        (status = 200, description = "下载并上传成功", body = HttpResult<UploadResult>),
        (status = 400, description = "URL 非法或不是 HTTP(S) / env 非法", body = HttpResult<String>),
        (status = 502, description = "env=dev 开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state))]
pub async fn upload_from_url(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, env)): Path<(String, String)>,
    Json(req): Json<UploadFromUrlRequest>,
) -> Result<Json<HttpResult<UploadResult>>, AppError> {
    let env = shared_types::UserappEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_env_error(&env)))?;
    info!(
        "[APP] upload from url: {} (env={}, url={})",
        app_id,
        env.as_str(),
        req.url
    );
    let target = req.target.unwrap_or_else(|| "code/".to_string());
    let flatten = req.flatten.unwrap_or(false);
    let result = state
        .app_service
        .upload_from_url(env, &app_id, &req.url, &target, flatten)
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
///
/// 列应用数据卷内指定子目录的文件清单（名称/大小/mtime 等元信息，非内容）。
/// `path` 相对 app 根（如 "code"/"data"/"logs"），缺省列 app 根一层；
/// 递归遍历请逐层下钻或配合 file-server 文件镜像族接口使用。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{env}/files",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("env" = String, Path, description = "目标环境：`dev`=开发容器（根=workspace）/ `prod`=生产运行容器（根=/app）"),
        ("path" = Option<String>, Query, description = "子目录（相对环境根，如 code/data/logs；默认列根）")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<FileInfo>>),
        (status = 400, description = "env 非法", body = HttpResult<String>),
        (status = 404, description = "应用/路径不存在", body = HttpResult<String>),
        (status = 502, description = "env=dev 开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state))]
pub async fn list_files(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, env)): Path<(String, String)>,
    Query(q): Query<ListFilesQuery>,
) -> Result<Json<HttpResult<Vec<FileInfo>>>, AppError> {
    let env = shared_types::UserappEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_env_error(&env)))?;
    info!(
        "[APP] listing files: {} (env={}, subpath={:?})",
        app_id,
        env.as_str(),
        q.path
    );
    let files = state
        .app_service
        .list_files(env, &app_id, q.path.as_deref())
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
///
/// 按路径删除应用数据卷内的单个文件（app 根相对，可指向 code/data/logs 下
/// 任意文件）；目录递归清理不在此面——危险操作走 storage/clear。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{env}/files/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("env" = String, Path, description = "目标环境：`dev`=开发容器（workspace 根相对）/ `prod`=生产运行容器（/app 根相对）")
    ),
    request_body = DeleteFileRequest,
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 400, description = "env 非法", body = HttpResult<String>),
        (status = 404, description = "文件/应用不存在", body = HttpResult<String>),
        (status = 502, description = "env=dev 开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
pub async fn delete_file(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, env)): Path<(String, String)>,
    Json(request): Json<DeleteFileRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    let env = shared_types::UserappEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_env_error(&env)))?;
    info!(
        "[APP] deleting file: {}/{} (env={})",
        app_id,
        request.path,
        env.as_str()
    );
    state
        .app_service
        .delete_file(env, &app_id, &request.path)
        .await?;
    Ok(Json(HttpResult::success("文件删除成功".to_string())))
}
