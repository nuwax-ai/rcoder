//! 文件管理转发层（RBD 卷形态·容器中心化）。
//!
//! 四接口（upload / upload-from-url / files / files-delete）不再直读写卷——
//! rcoder 对生产 RBD 卷零挂载。改为：**唤醒**（闲置回收的 app 自动拉起）→
//! 解析运行容器地址（pod IP）→ 转发容器内 file-server-proxy (:60000) 的
//! `/api/v1/userapp/app-files/*` 内部契约。file-server 侧同语义实现（魔数识别
//! zip/tar.gz 解压 + flatten、app 根相对路径、防穿越），REST 契约（handbook）
//! 对 Java 保持不变。

use tracing::{info, instrument, warn};

use serde::Deserialize;
use shared_types::AppWakeControl;

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

/// 运行容器 file-server-proxy 端口（与 ttyd 7681 / pgweb 8081 同为固定端口）。
const APP_FILE_SERVER_PORT: u16 = shared_types::AGENT_FILE_SERVER_PORT;

/// file-server app-files 族响应 DTO（形状对齐 app_manager DTO，snake 键）。
///
/// 请求侧 app_id 双键发送（app_id+appId）：存量容器 digest 烙印不换镜像、
/// 旧 DTO 只认 camel 键（snake 键被静默忽略后 app_id 为空 → 错路由）；新容器
/// snake 主键 + camel alias 双收。存量容器全量换代后删 camel 键。
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
    /// 唤醒 + 解析运行容器文件服务基址（`http://{pod_ip}:60000`）。
    ///
    /// 幻报拦截：`ensure_running` 对不存在的 app 返回 AlreadyRunning（stopped-set
    /// 语义），后续 `get_app` NotFound 兜底 404。
    async fn app_files_base(&self, app_id: &str) -> AppResult<String> {
        match self.activity.ensure_running(app_id).await {
            shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
            shared_types::WakeOutcome::Timeout => {
                return Err(AppOperationError::InvalidState(format!(
                    "app {app_id} wake timed out; retry later"
                )));
            }
            shared_types::WakeOutcome::Failed(e) => {
                return Err(AppOperationError::InvalidState(format!(
                    "app {app_id} wake failed: {e}"
                )));
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

    /// 上传文件 / 压缩包（转发容器内 file-server，解压/flatten 语义同旧直读写实现）。
    ///
    /// 自动判断（魔数）：zip/tar.gz 压缩包 → 解压到 `target` 目录；其它 → 单文件存 `target`。
    /// 单文件：`target`=文件路径（如 `code/app.jar`）；压缩包：`target`=解压目录（如 `code/`）。
    #[instrument(skip(self, file_data))]
    pub async fn upload_file(
        &self,
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
        let base = self.app_files_base(app_id).await?;
        let file_name = std::path::Path::new(target)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "uploaded_file".to_string());
        let part = reqwest::multipart::Part::bytes(file_data).file_name(file_name);
        let form = reqwest::multipart::Form::new()
            .text("app_id", app_id.to_string())
            .text("appId", app_id.to_string())
            .text("target", target.to_string())
            .text("flatten", flatten.to_string())
            .part("file", part);
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/v1/userapp/app-files/upload"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| forward_error("upload", app_id, e))?;
        let resp = check_status(resp, "upload", app_id).await?;
        let parsed: UploadResp = resp.json().await.map_err(|e| {
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
    #[instrument(skip(self))]
    pub async fn upload_from_url(
        &self,
        app_id: &str,
        url: &str,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        validate_app_id(app_id)?;
        validate_upload_target(target)?;
        let base = self.app_files_base(app_id).await?;
        let body = serde_json::json!({
            "app_id": app_id,
            "appId": app_id,
            "url": url,
            "target": target,
            "flatten": flatten,
        });
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/v1/userapp/app-files/upload-from-url"))
            .json(&body)
            .send()
            .await
            .map_err(|e| forward_error("upload-from-url", app_id, e))?;
        let resp = check_status(resp, "upload-from-url", app_id).await?;
        let parsed: UploadResp = resp.json().await.map_err(|e| {
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
    /// 同旧直读写实现）。
    #[instrument(skip(self))]
    pub async fn list_files(
        &self,
        app_id: &str,
        subpath: Option<&str>,
    ) -> AppResult<Vec<FileInfo>> {
        validate_app_id(app_id)?;
        let base = self.app_files_base(app_id).await?;
        let mut url = format!(
            "{base}/api/v1/userapp/app-files/list?app_id={}&appId={}",
            urlencode(app_id),
            urlencode(app_id)
        );
        if let Some(p) = subpath.map(str::trim).filter(|p| !p.is_empty()) {
            url.push_str("&path=");
            url.push_str(&urlencode(p));
        }
        let resp = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .map_err(|e| forward_error("list-files", app_id, e))?;
        let resp = check_status(resp, "list-files", app_id).await?;
        let parsed: ListResp = resp.json().await.map_err(|e| {
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
    pub async fn delete_file(&self, app_id: &str, file_path: &str) -> AppResult<()> {
        validate_app_id(app_id)?;
        if file_path.trim().is_empty() {
            return Err(AppOperationError::Validation(
                "file path is empty".to_string(),
            ));
        }
        let base = self.app_files_base(app_id).await?;
        let body = serde_json::json!({"app_id": app_id, "appId": app_id, "path": file_path});
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/v1/userapp/app-files/delete"))
            .json(&body)
            .send()
            .await
            .map_err(|e| forward_error("delete-file", app_id, e))?;
        let checked = check_status(resp, "delete-file", app_id).await?;
        drop(checked);
        info!(
            "[APP] file deleted via container file-server: {}",
            file_path
        );
        Ok(())
    }
}

/// 非 2xx → 对应错误（携带容器侧错误信息，便于 Java/排障定位）。
async fn check_status(
    resp: reqwest::Response,
    op: &str,
    app_id: &str,
) -> AppResult<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    warn!("[APP] app-files forward {op} non-success (app {app_id}): {status} {body}");
    Err(match status {
        reqwest::StatusCode::NOT_FOUND => AppOperationError::NotFound(format!("{op}: {body}")),
        reqwest::StatusCode::BAD_REQUEST => AppOperationError::Validation(format!("{op}: {body}")),
        _ => AppOperationError::Backend(format!("{op} failed: {status} {body}")),
    })
}

fn forward_error(op: &str, app_id: &str, e: reqwest::Error) -> AppOperationError {
    AppOperationError::Backend(format!(
        "forward {op} to container file-server (app {app_id}) failed: {e}"
    ))
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
        assert_eq!(urlencode("app-1.2_x"), "app-1.2_x");
    }
}
