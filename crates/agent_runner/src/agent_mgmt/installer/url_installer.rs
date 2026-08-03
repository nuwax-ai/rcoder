//! URL installer (P0-1)
//!
//! HTTP/HTTPS 下载,然后委托给 [`super::binary_installer::install_from_bytes`]
//! 进行落盘、文件类型检测、解压、注册。
//!
//! 使用 `download_utils` crate 提供的下载功能，支持：
//! - 重试、断点续传、SHA-256 校验、取消

use shared_types::InstallType;
use shared_types_grpc::InstallAgentResponse;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use download_utils::{DownloadConfig, Downloader};

use super::binary_installer;
use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::install_lock::InstallLockManager;
use crate::agent_mgmt::path_manager::PathManager;
use crate::agent_mgmt::registry::AgentRegistry;

/// 版本检查安装参数
///
/// 封装了版本检查安装所需的所有参数，
/// 避免函数参数过多。
pub struct VersionCheckInstallParams<'a> {
    /// 安装锁管理器
    pub lock_manager: &'a InstallLockManager,
    /// Agent 注册表
    pub registry: &'a AgentRegistry,
    /// 路径管理器
    pub path_manager: &'a PathManager,
    /// Agent ID
    pub agent_id: &'a str,
    /// 命令
    pub command: &'a str,
    /// 参数
    pub args: &'a [String],
    /// 版本
    pub version: &'a str,
    /// 平台配置
    pub platforms: &'a std::collections::HashMap<String, shared_types::PlatformEntry>,
    /// 是否强制安装
    pub force: bool,
}

/// 实际安装参数
///
/// 封装了实际安装所需的所有参数，
/// 避免函数参数过多。
struct DoInstallParams<'a> {
    /// 取消令牌
    cancel_token: CancellationToken,
    /// Agent 注册表
    registry: &'a AgentRegistry,
    /// 路径管理器
    path_manager: &'a PathManager,
    /// Agent ID
    agent_id: &'a str,
    /// 命令
    command: &'a str,
    /// 参数
    args: &'a [String],
    /// 版本
    version: &'a str,
    /// 平台配置
    platforms: &'a std::collections::HashMap<String, shared_types::PlatformEntry>,
}

/// 删除安装临时文件。
///
/// 安装成功时文件通常已被原子 rename，因此 `NotFound` 是正常结果；其他错误需要保留日志，
/// 避免权限或磁盘故障导致 staging 文件长期堆积而无法察觉。
async fn cleanup_staging_file(path: &std::path::Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "failed to remove agent install staging file"
            );
        }
    }
}

/// 解析并校验安装源 URL。仅限制协议，不限制 host，兼容内网 IP、集群域名和 localhost。
fn parse_http_url(url: &str) -> AgentMgmtResult<reqwest::Url> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| AgentMgmtError::InvalidChunk(format!("invalid URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AgentMgmtError::InvalidChunk(format!(
            "URL must use http or https, got scheme {:?}",
            parsed.scheme()
        )));
    }
    Ok(parsed)
}

/// 从 URL 下载并安装(下载完成后,二进制/解压逻辑复用 `binary_installer`)
pub async fn install_from_url(
    registry: &AgentRegistry,
    path_manager: &PathManager,
    agent_id: &str,
    url: &str,
    command: &str,
    args: &[String],
    expected_sha256: Option<&str>,
) -> AgentMgmtResult<InstallAgentResponse> {
    crate::agent_mgmt::path_manager::validate_agent_id(agent_id)
        .map_err(AgentMgmtError::InvalidManifest)?;

    let parsed_url = parse_http_url(url)?;

    // 私有化部署需要从内网 IP、集群域名和 localhost 镜像源下载；不限制 host 的公网属性。
    // 仍只接受 HTTP(S)，拒绝 file/gopher 等本地文件或非 HTTP 协议。
    if command.is_empty() {
        return Err(AgentMgmtError::InvalidChunk("empty command".into()));
    }
    crate::agent_mgmt::path_manager::validate_command(command)
        .map_err(AgentMgmtError::InvalidManifest)?;

    info!(
        "[agent_mgmt] url install: agent_id={}, origin={}",
        agent_id,
        parsed_url.origin().ascii_serialization()
    );

    // 下载到临时文件(支持重试 + 断点续传)
    let staging_path = path_manager
        .install_dir()
        .join(format!(".download-staging-{}", uuid::Uuid::new_v4()));
    let cancel = CancellationToken::new(); // install_from_url 不支持取消
    download_to_file(
        url,
        &staging_path,
        shared_types::MAX_BINARY_SIZE,
        expected_sha256,
        &cancel,
    )
    .await?;

    // 文件大小（download_to_file 已校验，传递给 install_from_file）
    let file_size = match tokio::fs::metadata(&staging_path).await {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            cleanup_staging_file(&staging_path).await;
            return Err(AgentMgmtError::Io(error));
        }
    };

    let response = binary_installer::install_from_file(
        registry,
        path_manager,
        binary_installer::InstallFileParams {
            agent_id,
            command,
            args,
            install_type: InstallType::Url,
            download_path: &staging_path,
            version: None,
            source: Some(url),
            file_size,
        },
    )
    .await;

    // install_from_file 成功时已 rename 走了 staging，失败时需清理
    cleanup_staging_file(&staging_path).await;

    let mut response = response?;
    response.source_url = Some(url.to_string());
    Ok(response)
}

/// 多平台版本管理安装(幂等)
///
/// 1. 获取 per-agent-id 安装锁（防并发）
/// 2. 查注册表判断 action (installed/updated/skipped)
/// 3. skipped → 直接返回现有信息
/// 4. 匹配当前系统平台 → 查 platforms map → 下载 → 安装
pub async fn install_with_version_check(
    params: VersionCheckInstallParams<'_>,
) -> AgentMgmtResult<InstallAgentResponse> {
    crate::agent_mgmt::path_manager::validate_agent_id(params.agent_id)
        .map_err(AgentMgmtError::InvalidManifest)?;
    crate::agent_mgmt::path_manager::validate_command(params.command)
        .map_err(AgentMgmtError::InvalidManifest)?;

    // 0. 版本格式校验(至少包含一个数字)
    validate_version_format(params.version)?;

    // 0.5 获取 per-agent-version 安装锁
    let state = params
        .lock_manager
        .get_or_create(params.agent_id, params.version)
        .ok_or_else(|| {
            AgentMgmtError::InvalidVersion(format!("invalid semver version: {}", params.version))
        })?;
    if params.force {
        // 强制模式：取消当前安装，等待锁
        state.cancel();
    }
    let _guard = if params.force {
        state.lock().await
    } else {
        match state.try_lock() {
            Some(guard) => guard,
            None => {
                // 正在安装中，返回当前状态
                let current_version = state.installing_version();
                info!(
                    "[agent_mgmt] install in progress: agent_id={}, version={:?}, requested={}",
                    params.agent_id, current_version, params.version
                );
                let mut resp =
                    make_in_progress_response(params.agent_id, current_version.as_deref());
                resp.previous_version = params.version.to_string();
                return Ok(resp);
            }
        }
    };

    // 标记安装开始，替换取消令牌（确保 force-cancel 后新安装使用干净的 token）
    state.set_installing(params.version);
    let cancel_token = CancellationToken::new();
    state.replace_cancel_token(cancel_token.clone());

    // 执行安装（内部检查 cancel token）
    let do_params = DoInstallParams {
        cancel_token,
        registry: params.registry,
        path_manager: params.path_manager,
        agent_id: params.agent_id,
        command: params.command,
        args: params.args,
        version: params.version,
        platforms: params.platforms,
    };
    let result = do_install_with_version_check(do_params).await;

    // 安装完成（成功或失败），清除状态
    state.clear_installing();

    result
}

/// 实际安装逻辑（从 install_with_version_check 中提取，便于锁管理）
async fn do_install_with_version_check(
    params: DoInstallParams<'_>,
) -> AgentMgmtResult<InstallAgentResponse> {
    use crate::agent_mgmt::registry::normalize_platform_key;

    // 1. 版本检查：检查特定版本是否已安装（精确匹配）
    // 使用单次 lookup 避免 contains + get 之间的 TOCTOU 竞争
    if let Some(manifest) = params.registry.get_version(params.agent_id, params.version) {
        let mut resp = make_skip_response(&manifest);
        resp.previous_version = params.version.to_string();
        return Ok(resp);
    }

    // 多版本并存模式：精确版本不存在即为新安装（不是更新）
    // Updated 仅用于单版本替换场景（旧版本被新版本覆盖）
    let action = shared_types::InstallAction::Installed;

    info!(
        "[agent_mgmt] version check: agent_id={}, action={}, requested_version={}",
        params.agent_id,
        action.as_str(),
        params.version
    );

    // 2. 匹配当前系统平台
    let sys_info = shared_types::SystemInfo::current();
    let platform_key = normalize_platform_key(&sys_info.os, &sys_info.arch);

    let entry = params.platforms.get(&platform_key).ok_or_else(|| {
        AgentMgmtError::PlatformNotFound(format!(
            "{platform_key} (available: {:?})",
            params.platforms.keys().collect::<Vec<_>>()
        ))
    })?;

    // 3. 验证 URL
    let parsed_url = parse_http_url(&entry.url)?;

    info!(
        "[agent_mgmt] platform install: agent_id={}, platform={}, origin={}",
        params.agent_id,
        platform_key,
        parsed_url.origin().ascii_serialization()
    );

    // 4. 下载到临时文件(支持重试 + 断点续传)
    let expected_sha256 = entry.sha256.as_deref().filter(|s| !s.is_empty());
    let staging_path = params
        .path_manager
        .install_dir()
        .join(format!(".download-staging-{}", uuid::Uuid::new_v4()));
    download_to_file(
        &entry.url,
        &staging_path,
        shared_types::MAX_BINARY_SIZE,
        expected_sha256,
        &params.cancel_token,
    )
    .await?;

    // 文件大小（download_to_file 已校验，传递给 install_from_file）
    let file_size = match tokio::fs::metadata(&staging_path).await {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            cleanup_staging_file(&staging_path).await;
            return Err(AgentMgmtError::Io(error));
        }
    };

    // 5. 安装(复用 binary_installer::install_from_file，避免全量读入内存)
    let t_install = std::time::Instant::now();
    debug!(
        "[agent_mgmt] starting install_from_file: agent_id={}, file_size={}",
        params.agent_id, file_size
    );
    let response = binary_installer::install_from_file(
        params.registry,
        params.path_manager,
        binary_installer::InstallFileParams {
            agent_id: params.agent_id,
            command: params.command,
            args: params.args,
            install_type: InstallType::Url,
            download_path: &staging_path,
            version: Some(params.version),
            source: Some(&entry.url),
            file_size,
        },
    )
    .await;
    debug!(
        "[agent_mgmt] install_from_file completed: took {:?}",
        t_install.elapsed()
    );

    // install_from_file 成功时已 rename 走了 staging，失败时需清理
    cleanup_staging_file(&staging_path).await;

    let mut response = response?;

    // 6. 覆盖 response 字段
    response.action = action.as_str().to_string();
    response.installed = true;
    response.previous_version = String::new();
    response.platform = platform_key;
    response.source_url = Some(entry.url.clone());

    Ok(response)
}

/// 校验版本格式:至少包含一个数字(如 "1.0.0", "v2", "1.2.3-beta")
/// 校验版本格式：必须是合法的 semver（如 "1.0.0"、"v2.1.3-beta"）
fn validate_version_format(version: &str) -> AgentMgmtResult<()> {
    shared_types::version_util::parse_semver(version)
        .ok_or_else(|| AgentMgmtError::InvalidVersion(format!("invalid semver: {version:?}")))?;
    Ok(())
}

/// 构造 in-progress response(正在安装中,force=false 时返回)
fn make_in_progress_response(agent_id: &str, version: Option<&str>) -> InstallAgentResponse {
    InstallAgentResponse {
        agent_id: agent_id.to_string(),
        status: shared_types_grpc::AgentInstallStatus::Available as i32,
        binary_path: String::new(),
        file_type: String::new(),
        file_count: None,
        file_size: 0,
        version: version.map(String::from),
        source_url: None,
        action: "in_progress".to_string(),
        installed: false,
        previous_version: String::new(),
        platform: String::new(),
    }
}

/// 构造 skip response(版本已是最新,不需要安装)
fn make_skip_response(
    manifest: &crate::agent_mgmt::installer::AgentManifest,
) -> InstallAgentResponse {
    InstallAgentResponse {
        agent_id: manifest.agent_id.clone(),
        status: shared_types_grpc::AgentInstallStatus::Available as i32,
        binary_path: manifest.binary_path.clone(),
        file_type: manifest.file_type.clone(),
        file_count: None,
        file_size: 0,
        version: manifest.version.clone(),
        source_url: None,
        action: shared_types::InstallAction::Skipped.as_str().to_string(),
        installed: false,
        previous_version: String::new(),
        platform: String::new(),
    }
}

/// 下载 URL 内容到文件（使用 download_utils）
///
/// 委托给 `download_utils::Downloader`，支持重试、断点续传、SHA-256 校验、取消。
pub(crate) async fn download_to_file(
    url: &str,
    dest_path: &std::path::Path,
    max_bytes: u64,
    expected_sha256: Option<&str>,
    cancel_token: &CancellationToken,
) -> AgentMgmtResult<u64> {
    let config = DownloadConfig {
        max_bytes,
        timeout_secs: shared_types::URL_DOWNLOAD_TIMEOUT_SECS,
        max_retries: 3,
        retry_backoff_base_secs: 1,
    };
    let downloader = Downloader::new(config);

    downloader
        .download_to_file(url, dest_path, expected_sha256, cancel_token)
        .await
        .map_err(|e| match e {
            download_utils::DownloadError::Cancelled => AgentMgmtError::InstallCancelled,
            download_utils::DownloadError::BinaryTooLarge { size, max } => {
                AgentMgmtError::BinaryTooLarge { size, max }
            }
            download_utils::DownloadError::ChecksumMismatch { expected, actual } => {
                AgentMgmtError::ChecksumMismatch { expected, actual }
            }
            other => AgentMgmtError::InstallFailed(format!("download failed: {}", other)),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use std::net::SocketAddr;
    use std::time::Duration;

    #[test]
    fn rejects_non_http_scheme() {
        assert!(parse_http_url("file:///etc/passwd").is_err());
        assert!(parse_http_url("gopher://evil/").is_err());
        assert!(parse_http_url("not a URL").is_err());
        assert!(parse_http_url("http://127.0.0.1/agent").is_ok());
        assert!(parse_http_url("https://service.cluster.local/agent").is_ok());
    }

    // ========== 本地 HTTP 测试服务器 ==========

    /// 测试数据：100KB 的伪随机数据（可预测，便于验证完整性）
    fn test_data() -> Vec<u8> {
        let size = 100 * 1024; // 100KB
        (0..size).map(|i| (i % 251) as u8).collect() // 251 是质数，避免周期性
    }

    /// 启动本地 HTTP 服务器，返回绑定地址
    async fn start_server(app: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // 等待服务器就绪
        tokio::time::sleep(Duration::from_millis(10)).await;
        addr
    }

    /// 支持 Range 请求的处理器（模拟 S3/OSS）
    async fn handler_with_range(req: Request<Body>) -> Response {
        let data = test_data();
        let total_size = data.len() as u64;
        let headers = req.headers();

        if let Some(range_header) = headers.get(header::RANGE) {
            let range_str = range_header.to_str().unwrap_or("");
            if let Some(range) = parse_range(range_str, total_size) {
                let content = &data[range.0 as usize..range.1 as usize];
                let mut resp = Response::new(Body::from(content.to_vec()));
                *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
                resp.headers_mut().insert(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", range.0, range.1 - 1, total_size)
                        .parse()
                        .unwrap(),
                );
                resp.headers_mut().insert(
                    header::CONTENT_LENGTH,
                    content.len().to_string().parse().unwrap(),
                );
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    "application/octet-stream".parse().unwrap(),
                );
                return resp;
            }
        }

        // 无 Range 或 Range 无效，返回完整内容
        let mut resp = Response::new(Body::from(data));
        resp.headers_mut().insert(
            header::CONTENT_LENGTH,
            total_size.to_string().parse().unwrap(),
        );
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        resp
    }

    /// 不支持 Range 的处理器（模拟 MinIO 默认行为）
    async fn handler_no_range(_req: Request<Body>) -> Response {
        let data = test_data();
        let len = data.len();
        let mut resp = Response::new(Body::from(data));
        resp.headers_mut()
            .insert(header::CONTENT_LENGTH, len.to_string().parse().unwrap());
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        resp
    }

    /// 404 处理器
    async fn handler_404() -> impl IntoResponse {
        StatusCode::NOT_FOUND
    }

    async fn handler_slow() -> impl IntoResponse {
        tokio::time::sleep(Duration::from_secs(5)).await;
        StatusCode::OK
    }

    /// 解析 Range: bytes=start-end
    fn parse_range(range_str: &str, total_size: u64) -> Option<(u64, u64)> {
        let range_str = range_str.strip_prefix("bytes=")?;
        if let Some((start_str, end_str)) = range_str.split_once('-') {
            let start: u64 = start_str.parse().ok()?;
            let end: u64 = if end_str.is_empty() {
                total_size - 1
            } else {
                end_str.parse().ok()?
            };
            if start <= end && end < total_size {
                return Some((start, end + 1)); // [start, end) 半开区间
            }
        }
        None
    }

    // ========== 本地 HTTP 服务器测试（替代 #[ignore] 测试） ==========

    /// 测试基本下载（本地服务器）
    #[tokio::test]
    async fn test_download_basic() {
        let app = Router::new().route("/file", get(handler_with_range));
        let addr = start_server(app).await;
        let url = format!("http://{}/file", addr);

        let dest = std::env::temp_dir().join("test-download-basic.bin");
        let _ = std::fs::remove_file(&dest);

        let max_bytes = test_data().len() as u64 * 2; // 足够大
        let result =
            download_to_file(&url, &dest, max_bytes, None, &CancellationToken::new()).await;

        match result {
            Ok(size) => {
                assert_eq!(size, test_data().len() as u64);
                let file_content = std::fs::read(&dest).unwrap();
                assert_eq!(file_content, test_data());
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                panic!("download failed: {}", e);
            }
        }

        let _ = std::fs::remove_file(&dest);
    }

    /// 测试 max_bytes 限制（下载过程中超限）
    #[tokio::test]
    async fn test_download_max_bytes() {
        let app = Router::new().route("/file", get(handler_with_range));
        let addr = start_server(app).await;
        let url = format!("http://{}/file", addr);

        let dest = std::env::temp_dir().join("test-download-maxbytes.bin");
        let _ = std::fs::remove_file(&dest);

        // max_bytes 小于文件大小（100KB），下载过程中触发 BinaryTooLarge
        let max_bytes = 30 * 1024u64; // 30KB
        let result =
            download_to_file(&url, &dest, max_bytes, None, &CancellationToken::new()).await;

        match result {
            Err(AgentMgmtError::BinaryTooLarge { size, max }) => {
                assert!(size > max, "should report actual > max");
                assert_eq!(max, max_bytes);
                // size 是下载到超限时的实际字节数（略大于 max_bytes）
                assert!(size > max_bytes, "size should exceed max_bytes");
            }
            other => {
                let _ = std::fs::remove_file(&dest);
                panic!("expected BinaryTooLarge, got: {:?}", other);
            }
        }

        let _ = std::fs::remove_file(&dest);
    }

    /// 测试 404 错误不重试（本地服务器）
    #[tokio::test]
    async fn test_download_404_no_retry() {
        let app = Router::new().route("/notfound", get(handler_404));
        let addr = start_server(app).await;
        let url = format!("http://{}/notfound", addr);

        let dest = std::env::temp_dir().join("test-download-404.bin");
        let _ = std::fs::remove_file(&dest);

        let result = download_to_file(&url, &dest, 1024, None, &CancellationToken::new()).await;

        match result {
            Err(AgentMgmtError::InstallFailed(msg)) => {
                assert!(
                    msg.contains("404") || msg.contains("HTTP"),
                    "should be HTTP 404 error: {}",
                    msg
                );
            }
            other => {
                let _ = std::fs::remove_file(&dest);
                panic!("expected InstallFailed with HTTP 404, got: {:?}", other);
            }
        }

        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn test_download_cancellation_is_prompt_and_cleans_partial_file() {
        let app = Router::new().route("/slow", get(handler_slow));
        let addr = start_server(app).await;
        let url = format!("http://{addr}/slow");
        let dest = std::env::temp_dir().join("test-download-cancel.bin");
        std::fs::write(&dest, b"partial").expect("partial download");
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_task.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            download_to_file(&url, &dest, 1024, None, &cancel),
        )
        .await
        .expect("cancellation must not wait for HTTP timeout");

        assert!(matches!(result, Err(AgentMgmtError::InstallCancelled)));
        assert!(!dest.exists(), "cancelled partial download must be removed");
    }

    /// 测试断点续传（本地服务器，支持 Range）
    ///
    /// 1. 手动下载前半部分到文件（模拟中断）
    /// 2. 调用 download_to_file 续传（max_bytes 足够大）
    /// 3. 验证文件完整性
    #[tokio::test]
    async fn test_download_resume() {
        let app = Router::new().route("/file", get(handler_with_range));
        let addr = start_server(app).await;
        let url = format!("http://{}/file", addr);

        let dest = std::env::temp_dir().join("test-download-resume.bin");
        let _ = std::fs::remove_file(&dest);

        // Step 1: 手动下载前 30KB（模拟中断）
        let partial_size = 30 * 1024usize;
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("Range", format!("bytes=0-{}", partial_size - 1))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206, "server should support Range");
        let partial_bytes = resp.bytes().await.unwrap();
        assert_eq!(partial_bytes.len(), partial_size);
        std::fs::write(&dest, &partial_bytes).unwrap();

        let first_50: Vec<u8> = partial_bytes[..50].to_vec();

        // Step 2: 调用 download_to_file 续传
        let max_bytes = test_data().len() as u64 * 2; // 足够大
        let result =
            download_to_file(&url, &dest, max_bytes, None, &CancellationToken::new()).await;

        match result {
            Ok(total) => {
                let file_size = std::fs::metadata(&dest).unwrap().len();
                assert_eq!(file_size, total);
                assert!(
                    total > partial_size as u64,
                    "should have grown: {} > {}",
                    total,
                    partial_size
                );
                // 验证续传后文件内容正确
                let final_content = std::fs::read(&dest).unwrap();
                assert_eq!(
                    final_content,
                    test_data(),
                    "final content should match test data"
                );
                // 验证前 50 字节保持不变（Range 续传不覆盖已有数据）
                assert_eq!(
                    &final_content[..50],
                    &first_50[..],
                    "first 50 bytes should be preserved after resume"
                );
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                panic!("resume download failed: {}", e);
            }
        }

        let _ = std::fs::remove_file(&dest);
    }

    /// 测试不支持 Range 的服务器（模拟 MinIO）
    ///
    /// 服务器忽略 Range 头，返回完整文件。
    /// 已有部分文件会被重新下载覆盖。
    #[tokio::test]
    async fn test_download_no_resume() {
        let app = Router::new().route("/file", get(handler_no_range));
        let addr = start_server(app).await;
        let url = format!("http://{}/file", addr);

        let dest = std::env::temp_dir().join("test-download-noresume.bin");
        let _ = std::fs::remove_file(&dest);

        // 先写入部分数据模拟中断
        let partial = vec![0xABu8; 30 * 1024];
        std::fs::write(&dest, &partial).unwrap();

        // 尝试续传，但服务器不支持 Range → 从头下载 → 成功（max_bytes 足够大）
        let max_bytes = test_data().len() as u64 * 2;
        let result =
            download_to_file(&url, &dest, max_bytes, None, &CancellationToken::new()).await;

        match result {
            Ok(size) => {
                // 服务器不支持 Range，从头下载成功
                assert_eq!(size, test_data().len() as u64);
                let file_content = std::fs::read(&dest).unwrap();
                assert_eq!(file_content, test_data());
                // 文件被重新下载，不是续传
                assert_ne!(
                    &file_content[..30],
                    &partial[..],
                    "should be重新下载，不是续传"
                );
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                panic!("unexpected error: {}", e);
            }
        }

        let _ = std::fs::remove_file(&dest);
    }
}
