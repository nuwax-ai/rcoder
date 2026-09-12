//! upload-file / upload-files handlers: multipart 单文件与批量上传。

use axum::extract::State;
use garde::Validate;
use serde_json::Value;

use crate::ops::files::{upload_file_impl, upload_files_impl};
use crate::ops::multipart::{file_field, text_field};

use super::super::resolve_computer_target;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::models::{UploadFileForm, UploadFilesForm};
use crate::service::temp_file::TemporaryFile;

/// upload-file 必填字段 (multipart 提取后构造 + garde 校验; 文件字段用内置 required)。
#[derive(garde::Validate)]
struct UploadFileFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    user_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    cid: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    file_path: Option<String>,
    #[garde(required)]
    data: Option<TemporaryFile>,
}

/// [`UploadFileFields`] 校验后的全必填形态 (parse, don't validate):
/// garde 校验通过后由 [`UploadFileFields::into_validated`] 消费转换,
/// 后续代码直接用非 Option 类型, 无需反复 ok_or_else。
struct ValidatedUploadFile {
    user_id: String,
    cid: String,
    file_path: String,
    data: TemporaryFile,
}

impl UploadFileFields {
    /// garde 校验 + 消费转换: 校验通过则返回全必填 [`ValidatedUploadFile`],
    /// 失败返回 garde 校验错误。调用方拿到的值全部非 Option, 后续代码无需再处理 Option。
    fn into_validated(self) -> Result<ValidatedUploadFile, AppError> {
        self.validate().map_err(crate::error::from_garde)?;
        // garde 已校验非空, 此处 ok_or_else 是防御性兜底 (不用 unwrap: 避免 panic)
        Ok(ValidatedUploadFile {
            user_id: self
                .user_id
                .ok_or_else(|| AppError::system("user_id missing after garde validation"))?,
            cid: self
                .cid
                .ok_or_else(|| AppError::system("c_id missing after garde validation"))?,
            file_path: self
                .file_path
                .ok_or_else(|| AppError::system("file_path missing after garde validation"))?,
            data: self
                .data
                .ok_or_else(|| AppError::system("file missing after garde validation"))?,
        })
    }
}

/// upload-files 必填字段 (filePaths/files 数组允许为空, 仅 userId/cId 必填)。
#[derive(garde::Validate)]
struct UploadFilesFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    user_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    cid: Option<String>,
}

/// [`UploadFilesFields`] 校验后的全必填形态。
struct ValidatedUploadFiles {
    user_id: String,
    cid: String,
}

impl UploadFilesFields {
    fn into_validated(self) -> Result<ValidatedUploadFiles, AppError> {
        self.validate().map_err(crate::error::from_garde)?;
        Ok(ValidatedUploadFiles {
            user_id: self
                .user_id
                .ok_or_else(|| AppError::system("user_id missing after garde validation"))?,
            cid: self
                .cid
                .ok_or_else(|| AppError::system("c_id missing after garde validation"))?,
        })
    }
}

/// 上传单文件
///
/// 对齐 nuwax computer uploadFile; multipart。
/// 返回 {success, message, fileSize} (不返回 filePath/originalname)。
#[utoipa::path(post, path = "/upload-file", request_body(content = UploadFileForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut file_path = None;
    let mut custom_target_dir = None;
    let mut workspace_path = None; // 项目绑定目录 (对齐 TS f979df7, 可选 multipart 字段)
    let mut data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "filePath" => file_path = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "workspacePath" => workspace_path = Some(text_field(field).await?),
            "file" => {
                data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            _ => {}
        }
    }
    let fields = UploadFileFields {
        user_id,
        cid,
        file_path,
        data,
    };
    let v = fields.into_validated()?;
    let ws = resolve_computer_target(
        &state,
        &v.user_id,
        &v.cid,
        custom_target_dir.as_deref(),
        workspace_path.as_deref(),
    )
    .await?;
    upload_file_impl(&ws, &v.file_path, v.data).await
}

/// 批量上传文件
///
/// 对齐 nuwax computer uploadFiles; 多文件 multipart。
/// 返回 {success, message, totalCount, successCount, failCount, results:[{success,filePath,originalname?,message?,fileSize?,error?}]}。
#[utoipa::path(post, path = "/upload-files", request_body(content = UploadFilesForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn upload_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut workspace_path = None; // 项目绑定目录 (对齐 TS f979df7, 可选 multipart 字段)
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "workspacePath" => workspace_path = Some(text_field(field).await?),
            "filePaths" => file_paths.push(text_field(field).await?),
            "files" => {
                let original = field.file_name().map(|s| s.to_string());
                files_vec.push((
                    original,
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                ));
            }
            _ => {}
        }
    }
    let fields = UploadFilesFields { user_id, cid };
    let v = fields.into_validated()?;
    // 跨字段一致性 (文件路径与文件一一对应), 非纯字段校验, 保留手写
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let ws = resolve_computer_target(
        &state,
        &v.user_id,
        &v.cid,
        custom_target_dir.as_deref(),
        workspace_path.as_deref(),
    )
    .await?;
    upload_files_impl(&ws, &file_paths, &files_vec).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::{
        AppState, BuildManager, Config, DevServerManager, LocalWorkspaceResolver, LogCacheManager,
        SkillDownloader, WorkspaceResolver,
    };

    fn make_state(tmp: &std::path::Path) -> AppState {
        // 上传临时目录指到测试 tmp (默认 /app/... 在 macOS 不存在)
        let config = Arc::new(Config {
            upload_project_dir: tmp.join("upload-tmp"),
            ..Config::default()
        });
        let resolver: Arc<dyn WorkspaceResolver> = Arc::new(LocalWorkspaceResolver::new(
            config.project_source_dir.clone(),
            tmp.join("c"),
        ));
        AppState {
            resolver,
            dev_server: Arc::new(DevServerManager::new(config.clone())),
            build_manager: Arc::new(BuildManager::new(config.max_build_concurrency)),
            log_cache: Arc::new(LogCacheManager::new(&config)),
            skill_downloader: Arc::new(
                SkillDownloader::new(&config).expect("construct skill downloader"),
            ),
            config,
            started_at: std::time::Instant::now(),
        }
    }

    /// 手拼 multipart/form-data body (无第三方依赖; 文本字段 + 单文件)。
    fn multipart_body(
        boundary: &str,
        fields: &[(&str, &str)],
        file_name: &str,
        file_bytes: &[u8],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(file_bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    /// multipart 文本字段 "workspacePath" 通道: 上传文件落绑定目录 (对齐 TS f979df7,
    /// 字段名与 TS multer 同名——拼错大小写在此测试报红)。
    #[tokio::test]
    async fn upload_file_accepts_workspace_path_multipart_field() {
        use tower::ServiceExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        tokio::fs::create_dir_all(tmp.path().join("upload-tmp"))
            .await
            .unwrap();
        let bound = tmp.path().join("multipart-bound");
        let state = make_state(tmp.path());

        let app = axum::Router::new()
            .route("/upload-file", axum::routing::post(upload_file))
            .with_state(state);

        let boundary = "test-boundary-12345";
        let body = multipart_body(
            boundary,
            &[
                ("userId", "u"),
                ("cId", "c"),
                ("filePath", "nested/uploaded.txt"),
                ("workspacePath", &bound.to_string_lossy()),
            ],
            "uploaded.txt",
            b"payload",
        );
        let resp = app
            .oneshot(
                axum::http::Request::post("/upload-file")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(axum::body::Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let content = tokio::fs::read(bound.join("nested").join("uploaded.txt"))
            .await
            .expect("file lands in bound dir");
        assert_eq!(content, b"payload");
    }
}
