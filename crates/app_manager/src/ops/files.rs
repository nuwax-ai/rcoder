//! 文件管理转发层（容器中心化，`app_stage` 显式分派 dev/prod）。
//!
//! 四接口（upload / upload-from-url / files / files-delete）不直读写卷——
//! rcoder 对生产 RBD 卷零挂载。改为：按 `app_stage` 定位目标容器 → 转发其内
//! file-server-proxy (:60000) 的 `/api/v1/userapp/app-files/*` 内部契约。
//! file-server 侧同语义实现（魔数识别 zip/tar.gz 解压 + flatten、防穿越），
//! REST 契约（handbook）对 Java 保持不变。
//! - `app_stage=prod`：唤醒（闲置回收的 app 自动拉起）→ 运行容器；target 相对 /app 根
//! - `app_stage=dev`：幂等 ensure 开发容器（UserappBuilder）；target 相对 workspace 根

use std::sync::LazyLock;
use std::time::Duration;

use tracing::{info, instrument, warn};

use serde::Deserialize;
use shared_types::{AppWakeControl, UserappStage};

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

/// 运行容器 file-server-proxy 端口（与 ttyd 7681 / dbx 4224 同为固定端口）。
const APP_FILE_SERVER_PORT: u16 = shared_types::AGENT_FILE_SERVER_PORT;

/// 共享客户端连接建立超时（秒）——目标为集群内/本机容器，短连接超时足够。
const FILE_FORWARD_CONNECT_TIMEOUT_SECS: u64 = 5;

/// upload / upload-from-url 总超时（秒）。
/// upload：路由层放行至 1GiB 压缩包，含传输 + 容器侧解压/flatten；
/// upload-from-url：容器内流式下载外部 URL 再走上传核心，大制品耗时由下载侧
/// 决定，600s（≥300s 下限）覆盖两者。
const FILE_TRANSFER_TIMEOUT_SECS: u64 = 600;

/// list / delete 总超时（秒）——轻量元数据操作，短超时快速暴露失联容器。
const FILE_OPS_TIMEOUT_SECS: u64 = 30;

/// 文件转发共享客户端（对齐 rcoder-engine `http_client::shared_client` 风格：
/// 仅设连接超时 + 连接池复用，**不设全局总超时**——upload 是大文件长传，总超时
/// 会误杀；一次性请求按操作分级在 `RequestBuilder` 上单独 `.timeout(...)`）。
/// `reqwest::Client` 内部持有连接池且 Clone 廉价（Arc），全模块复用同一实例。
static FILE_FORWARD_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(FILE_FORWARD_CONNECT_TIMEOUT_SECS))
        .build()
        // builder 失败仅可能在 TLS 后端初始化异常时发生；退化为无超时 client 保持可用
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// file-server app-files 族响应 DTO（形状对齐 app_manager DTO，snake 键；
/// 请求侧同为 snake——本族为 userApp 专属新契约，未上线不做旧键兼容）。
#[derive(Debug, Deserialize)]
struct UploadResp {
    file_path: String,
    file_size: u64,
    uploaded_at: String,
    #[serde(default)]
    extracted_count: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct FileEntry {
    path: String,
    size: u64,
    is_dir: bool,
    modified_at: String,
}

#[derive(Debug, Deserialize)]
struct ListResp {
    #[serde(default)]
    files: Vec<FileEntry>,
}

impl AppService {
    /// 唤醒/定位 + 解析目标容器 file-server 基址（`http://{host}:60000`）。
    ///
    /// - `app_stage=prod`：`ensure_running` 唤醒（闲置回收的 app 自动拉起）→ `get_app`
    ///   拿运行容器 IP。幻报拦截：`ensure_running` 对不存在的 app 返回
    ///   AlreadyRunning（stopped-set 语义），后续 `get_app` NotFound 兜底 404。
    /// - `app_stage=dev`：经 `UserappDevLocator` 契约幂等 ensure UserappBuilder（探活
    ///   自愈，开发容器常驻无唤醒语义——app_manager 的 runtime 视图无 agent
    ///   能力，委托宿主 rcoder）。
    ///
    /// `user_id`：dev 懒创建容器的 owner 显式档——`None` = 调用方无显式入参，
    /// 由 ensure 侧取值链降级 metadata（**勿传空串哨兵**，空值用 None 表达）。
    pub(crate) async fn app_files_base(
        &self,
        app_stage: UserappStage,
        app_id: &str,
    ) -> AppResult<String> {
        if app_stage == UserappStage::Dev {
            let locator = self
                .dev_locator
                .read()
                .map_err(|_| {
                    AppOperationError::Diagnostic(shared_types::WakeFailure::new(
                        shared_types::ERR_RUNTIME_UNAVAILABLE,
                        "dev_locator",
                        "UserApp development container locator lock is poisoned",
                    ))
                })?
                .clone()
                .ok_or_else(|| {
                    AppOperationError::Diagnostic(shared_types::WakeFailure::new(
                        shared_types::ERR_RUNTIME_CONFIGURATION,
                        "dev_locator",
                        "UserApp development container locator is not configured",
                    ))
                })?;
            return locator.dev_file_server_addr(app_id).await.map_err(|e| {
                AppOperationError::Backend(format!(
                    "locate dev container file-server (app {app_id}): {e}"
                ))
            });
        }
        match self.activity.ensure_running(app_id).await {
            shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
            shared_types::WakeOutcome::Timeout(detail) => {
                return Err(AppOperationError::Diagnostic(detail));
            }
            shared_types::WakeOutcome::Blocked { message, blocker } => {
                return Err(AppOperationError::ConflictBlocked { message, blocker });
            }
            shared_types::WakeOutcome::Failed(detail) => {
                return Err(AppOperationError::Diagnostic(detail));
            }
        }
        let runtime = self.get_app(app_id).await?;
        let ip = runtime
            .health
            .instance
            .map(|instance| instance.ip)
            .filter(|ip| !ip.is_empty())
            .ok_or_else(|| {
                AppOperationError::InvalidState(format!(
                    "app {app_id} has no ready runtime IP for file access"
                ))
            })?;
        Ok(format!("http://{ip}:{APP_FILE_SERVER_PORT}"))
    }

    /// 上传文件 / 压缩包（转发目标容器 file-server，解压/flatten 语义同旧直读写实现）。
    ///
    /// 自动判断（魔数）：zip/tar.gz 压缩包 → 解压到 `target` 目录；其它 → 单文件存 `target`。
    /// 单文件：`target`=文件路径（如 `code/app.jar`）；压缩包：`target`=解压目录（如 `code/`）。
    /// `target` 根基准随 app_stage：prod=运行容器 app 根（/app）；dev=开发容器 workspace 根。
    #[instrument(skip(self, file_data))]
    pub async fn upload_file(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        validate_app_id(app_id)?;
        validate_upload_target(target)?;
        if file_data.is_empty() {
            return Err(AppOperationError::Validation(
                "file data is empty".to_string(),
            ));
        }
        let base = self.app_files_base(app_stage, app_id).await?;
        let file_name = std::path::Path::new(target)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "uploaded_file".to_string());
        let part = reqwest::multipart::Part::bytes(file_data).file_name(file_name);
        let form = reqwest::multipart::Form::new()
            .text("app_id", app_id.to_string())
            .text("target", target.to_string())
            .text("flatten", flatten.to_string())
            .part("file", part);
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(FILE_TRANSFER_TIMEOUT_SECS);
        let credentials = self
            .file_request_credentials(app_stage, app_id, deadline)
            .await?;
        let resp = credentials
            .apply(
                FILE_FORWARD_CLIENT
                    .post(format!("{base}/api/v1/userapp/app-files/upload"))
                    .timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
                    .multipart(form),
            )
            .send()
            .await
            .map_err(|e| forward_error("upload", app_id, e, FileForwardContext::Mutation))?;
        let body = read_file_response(resp, "upload", app_id, FileForwardContext::Mutation).await?;
        let parsed: UploadResp = serde_json::from_value(body).map_err(|e| {
            AppOperationError::Backend(format!("upload response decode (app {app_id}): {e}"))
        })?;
        info!(
            "[APP] file uploaded via container file-server: {} -> {} ({} bytes)",
            app_id, parsed.file_path, parsed.file_size
        );
        Ok(UploadResult {
            file_path: parsed.file_path,
            file_size: parsed.file_size,
            uploaded_at: parsed.uploaded_at,
            extracted_count: parsed.extracted_count,
        })
    }

    /// 从 URL 部署文件（容器内流式下载后走上传核心——大制品不进 rcoder 内存）。
    #[instrument(skip(self, url))]
    pub async fn upload_from_url(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        url: &str,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        validate_app_id(app_id)?;
        validate_upload_target(target)?;
        let base = self.app_files_base(app_stage, app_id).await?;
        let body = serde_json::json!({
            "app_id": app_id,
            "url": url,
            "target": target,
            "flatten": flatten,
        });
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(FILE_TRANSFER_TIMEOUT_SECS);
        let credentials = self
            .file_request_credentials(app_stage, app_id, deadline)
            .await?;
        let resp = credentials
            .apply(
                FILE_FORWARD_CLIENT
                    .post(format!("{base}/api/v1/userapp/app-files/upload-from-url"))
                    .timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
                    .json(&body),
            )
            .send()
            .await
            .map_err(|e| {
                forward_error("upload-from-url", app_id, e, FileForwardContext::Mutation)
            })?;
        let body = read_file_response(
            resp,
            "upload-from-url",
            app_id,
            FileForwardContext::Mutation,
        )
        .await?;
        let parsed: UploadResp = serde_json::from_value(body).map_err(|e| {
            AppOperationError::Backend(format!(
                "upload-from-url response decode (app {app_id}): {e}"
            ))
        })?;
        info!(
            "[APP] file deployed from url via container file-server: {} -> {}",
            app_id, parsed.file_path
        );
        Ok(UploadResult {
            file_path: parsed.file_path,
            file_size: parsed.file_size,
            uploaded_at: parsed.uploaded_at,
            extracted_count: parsed.extracted_count,
        })
    }

    /// 列出文件（app 根目录，或其子目录如 "code"/"data"/"logs"）。
    ///
    /// `subpath` 为 None/空 → 列 app 根；返回的 `path` 字段是 **app-root-relative**
    /// （如 "code/app.jar"），可直接作为 upload 的 target / delete 的 path（契约
    /// 同旧直读写实现）。app_stage=dev 时根基准为开发容器 workspace 根。
    #[instrument(skip(self))]
    pub async fn list_files(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        subpath: Option<&str>,
    ) -> AppResult<Vec<FileInfo>> {
        validate_app_id(app_id)?;
        let base = self.app_files_base(app_stage, app_id).await?;
        let mut url = format!(
            "{base}/api/v1/userapp/app-files/list?app_id={}",
            urlencode(app_id)
        );
        if let Some(p) = subpath.map(str::trim).filter(|p| !p.is_empty()) {
            url.push_str("&path=");
            url.push_str(&urlencode(p));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(FILE_OPS_TIMEOUT_SECS);
        let credentials = self
            .file_request_credentials(app_stage, app_id, deadline)
            .await?;
        let resp = credentials
            .apply(
                FILE_FORWARD_CLIENT
                    .get(url)
                    .timeout(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .send()
            .await
            .map_err(|e| forward_error("list-files", app_id, e, FileForwardContext::ReadOnly))?;
        let body =
            read_file_response(resp, "list-files", app_id, FileForwardContext::ReadOnly).await?;
        let parsed: ListResp = serde_json::from_value(body).map_err(|e| {
            AppOperationError::Backend(format!("list-files response decode (app {app_id}): {e}"))
        })?;
        Ok(parsed
            .files
            .into_iter()
            .map(|f| FileInfo {
                path: f.path,
                size: f.size,
                is_dir: f.is_dir,
                modified_at: f.modified_at,
            })
            .collect())
    }

    /// 删除文件（app 根相对路径，可指向 code/ data/ logs/）。
    #[instrument(skip(self))]
    pub async fn delete_file(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        file_path: &str,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        if file_path.trim().is_empty() {
            return Err(AppOperationError::Validation(
                "file path is empty".to_string(),
            ));
        }
        let base = self.app_files_base(app_stage, app_id).await?;
        let body = serde_json::json!({"app_id": app_id, "path": file_path});
        let deadline = tokio::time::Instant::now() + Duration::from_secs(FILE_OPS_TIMEOUT_SECS);
        let credentials = self
            .file_request_credentials(app_stage, app_id, deadline)
            .await?;
        let resp = credentials
            .apply(
                FILE_FORWARD_CLIENT
                    .post(format!("{base}/api/v1/userapp/app-files/delete"))
                    .timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
                    .json(&body),
            )
            .send()
            .await
            .map_err(|e| forward_error("delete-file", app_id, e, FileForwardContext::Mutation))?;
        read_file_response(resp, "delete-file", app_id, FileForwardContext::Mutation).await?;
        info!(
            "[APP] file deleted via container file-server: {}",
            file_path
        );
        Ok(())
    }
}

/// The caller declares whether dispatch can change files; operation names are diagnostic only.
#[derive(Clone, Copy)]
pub(crate) enum FileForwardContext {
    ReadOnly,
    Mutation,
}

/// Preserve the Response for storage protocols that decode their own physical identity.
pub(crate) async fn check_status(
    resp: reqwest::Response,
    op: &'static str,
    app_id: &str,
) -> AppResult<reqwest::Response> {
    check_status_with_context(resp, op, app_id, FileForwardContext::ReadOnly).await
}

pub(crate) async fn check_status_with_context(
    resp: reqwest::Response,
    op: &'static str,
    app_id: &str,
    context: FileForwardContext,
) -> AppResult<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp
        .text()
        .await
        .map_err(|error| forward_error(op, app_id, error, context))?;
    let body = serde_json::from_str::<serde_json::Value>(&text).ok();
    Err(file_peer_error(
        status,
        body.as_ref(),
        &text,
        op,
        app_id,
        context,
    ))
}

async fn read_file_response(
    resp: reqwest::Response,
    op: &'static str,
    app_id: &str,
    context: FileForwardContext,
) -> AppResult<serde_json::Value> {
    let response = check_status_with_context(resp, op, app_id, context).await?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| forward_error(op, app_id, error, context))?;
    // Every app-files success response has success:true. HTTP 2xx alone is not execution evidence.
    if body.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(file_peer_error(
            status,
            Some(&body),
            "File response did not confirm success",
            op,
            app_id,
            context,
        ));
    }
    Ok(body)
}

fn file_peer_error(
    status: reqwest::StatusCode,
    body: Option<&serde_json::Value>,
    raw: &str,
    op: &'static str,
    app_id: &str,
    context: FileForwardContext,
) -> AppOperationError {
    let source_code = body
        .and_then(|body| body.get("code"))
        .and_then(serde_json::Value::as_str)
        .filter(|code| {
            !code.is_empty() && *code != shared_types::ERR_UNKNOWN && *code != shared_types::SUCCESS
        });
    let source_type = body
        .and_then(|body| body.pointer("/error/type"))
        .and_then(serde_json::Value::as_str);
    let fallback = match source_type {
        Some("RESOURCE_ERROR") => shared_types::ERR_FILE_NOT_FOUND,
        Some("VALIDATION_ERROR" | "BUSINESS_ERROR") => shared_types::ERR_VALIDATION,
        Some("CONFLICT") => shared_types::ERR_CONFLICT,
        Some("NETWORK_ERROR") => shared_types::ERR_RUNTIME_UNAVAILABLE,
        _ => match status {
            reqwest::StatusCode::NOT_FOUND => shared_types::ERR_NOT_FOUND,
            reqwest::StatusCode::BAD_REQUEST => shared_types::ERR_VALIDATION,
            reqwest::StatusCode::CONFLICT => shared_types::ERR_CONFLICT,
            reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::GATEWAY_TIMEOUT => {
                shared_types::ERR_RUNTIME_TIMEOUT
            }
            reqwest::StatusCode::SERVICE_UNAVAILABLE | reqwest::StatusCode::BAD_GATEWAY => {
                shared_types::ERR_RUNTIME_UNAVAILABLE
            }
            _ => shared_types::ERR_BACKEND_ERROR,
        },
    };
    let message = body
        .and_then(|body| {
            body.get("message")
                .or_else(|| body.pointer("/error/message"))
        })
        .and_then(serde_json::Value::as_str)
        .unwrap_or(raw);
    let message = shared_types::sanitize_error_text(message);
    let message = format!("{op} failed: HTTP {status}: {message}");
    warn!(app_id, operation = op, status = status.as_u16(), detail = %message, "app-files peer rejected request");
    let definitive = status.is_client_error() && !matches!(status.as_u16(), 408 | 499);
    let protected = matches!(context, FileForwardContext::Mutation) && !definitive;
    let code = if protected {
        shared_types::ERR_OPERATION_OUTCOME_UNKNOWN
    } else {
        fallback
    };
    let mut failure = shared_types::WakeFailure::new(code, op, message.clone());
    failure.cause_code = fallback.into();
    failure.retryable = !protected
        && matches!(
            fallback,
            shared_types::ERR_RUNTIME_TIMEOUT | shared_types::ERR_RUNTIME_UNAVAILABLE
        );
    let mut diagnostic = body
        .and_then(|body| body.get("error_detail"))
        .cloned()
        .and_then(|detail| serde_json::from_value::<shared_types::ErrorDetail>(detail).ok())
        .unwrap_or_else(|| {
            shared_types::ErrorDetail::new(source_code.unwrap_or(fallback), op, message)
                .with_retryable(failure.retryable)
        });
    diagnostic = diagnostic.localized(shared_types::current_request_locale());
    diagnostic.retryable &= !protected;
    failure.command_diagnostic = Some(Box::new(shared_types::PgCommandDiagnostic {
        code: source_code.unwrap_or(fallback).to_owned(),
        operation_id: body
            .and_then(|body| body.get("operation_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        blocker: body
            .and_then(|body| body.get("blocker"))
            .cloned()
            .and_then(|blocker| serde_json::from_value(blocker).ok()),
        error_detail: Some(diagnostic),
    }));
    AppOperationError::Diagnostic(failure)
}

fn forward_error(
    op: &'static str,
    app_id: &str,
    error: reqwest::Error,
    context: FileForwardContext,
) -> AppOperationError {
    let cause = if error.is_builder() {
        shared_types::ERR_RUNTIME_CONFIGURATION
    } else if error.is_timeout() {
        shared_types::ERR_RUNTIME_TIMEOUT
    } else if error.is_decode() {
        shared_types::ERR_BACKEND_ERROR
    } else {
        shared_types::ERR_RUNTIME_UNAVAILABLE
    };
    let dispatched = !error.is_connect() && !error.is_builder();
    let protected = matches!(context, FileForwardContext::Mutation) && dispatched;
    let code = if protected {
        shared_types::ERR_OPERATION_OUTCOME_UNKNOWN
    } else {
        cause
    };
    let mut failure = shared_types::WakeFailure::new(
        code,
        op,
        shared_types::sanitize_error_text(&format!(
            "forward {op} to container file-server (app {app_id}) failed: {error}"
        )),
    );
    failure.cause_code = cause.into();
    failure.retryable = !protected
        && matches!(
            cause,
            shared_types::ERR_RUNTIME_TIMEOUT | shared_types::ERR_RUNTIME_UNAVAILABLE
        );
    AppOperationError::Diagnostic(failure)
}

/// query 参数百分号编码（防 `&`/空格 截断 query）。
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_encodes_reserved_chars() {
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlencode("app1.2_x"), "app1.2_x");
    }
}

#[cfg(test)]
#[path = "files/config_budget_tests.rs"]
mod config_budget_tests;

#[cfg(test)]
mod credential_tests;

#[cfg(test)]
mod response_contract_tests;
