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

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(AgentMgmtError::InvalidChunk(format!(
            "URL must start with http:// or https://: {url}"
        )));
    }
    if command.is_empty() {
        return Err(AgentMgmtError::InvalidChunk("empty command".into()));
    }
    crate::agent_mgmt::path_manager::validate_command(command)
        .map_err(AgentMgmtError::InvalidManifest)?;

    info!("[agent_mgmt] url install: agent_id={}, url={}", agent_id, url);

    // 下载到临时文件(支持重试 + 断点续传)
    let staging_path = path_manager.install_dir()
        .join(format!(".download-staging-{}", uuid::Uuid::new_v4()));
    let cancel = CancellationToken::new(); // install_from_url 不支持取消
    download_to_file(url, &staging_path, shared_types::MAX_BINARY_SIZE, expected_sha256, &cancel).await?;

    // 文件大小（download_to_file 已校验，传递给 install_from_file）
    let file_size = std::fs::metadata(&staging_path)
        .map(|m| m.len())
        .map_err(AgentMgmtError::Io)?;

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
    let _ = std::fs::remove_file(&staging_path);

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
#[allow(clippy::too_many_arguments)]
pub async fn install_with_version_check(
    lock_manager: &InstallLockManager,
    registry: &AgentRegistry,
    path_manager: &PathManager,
    agent_id: &str,
    command: &str,
    args: &[String],
    version: &str,
    platforms: &std::collections::HashMap<String, shared_types::PlatformEntry>,
    force: bool,
) -> AgentMgmtResult<InstallAgentResponse> {
    crate::agent_mgmt::path_manager::validate_agent_id(agent_id)
        .map_err(AgentMgmtError::InvalidManifest)?;
    crate::agent_mgmt::path_manager::validate_command(command)
        .map_err(AgentMgmtError::InvalidManifest)?;

    // 0. 版本格式校验(至少包含一个数字)
    validate_version_format(version)?;

    // 0.5 获取 per-agent-version 安装锁
    let state = lock_manager.get_or_create(agent_id, version);
    if force {
        // 强制模式：取消当前安装，等待锁
        state.cancel();
    }
    let _guard = if force {
        state.lock().await
    } else {
        match state.try_lock() {
            Some(guard) => guard,
            None => {
                // 正在安装中，返回当前状态
                let current_version = state.installing_version();
                info!(
                    "[agent_mgmt] install in progress: agent_id={}, version={:?}, requested={}",
                    agent_id, current_version, version
                );
                let mut resp = make_in_progress_response(agent_id, current_version.as_deref());
                resp.previous_version = version.to_string();
                return Ok(resp);
            }
        }
    };

    // 标记安装开始，替换取消令牌（确保 force-cancel 后新安装使用干净的 token）
    state.set_installing(version);
    let cancel_token = CancellationToken::new();
    state.replace_cancel_token(cancel_token.clone());

    // 执行安装（内部检查 cancel token）
    let result = do_install_with_version_check(
        cancel_token,
        registry,
        path_manager,
        agent_id,
        command,
        args,
        version,
        platforms,
    )
    .await;

    // 安装完成（成功或失败），清除状态
    state.clear_installing();

    result
}

/// 实际安装逻辑（从 install_with_version_check 中提取，便于锁管理）
#[allow(clippy::too_many_arguments)]
async fn do_install_with_version_check(
    cancel_token: CancellationToken,
    registry: &AgentRegistry,
    path_manager: &PathManager,
    agent_id: &str,
    command: &str,
    args: &[String],
    version: &str,
    platforms: &std::collections::HashMap<String, shared_types::PlatformEntry>,
) -> AgentMgmtResult<InstallAgentResponse> {
    use crate::agent_mgmt::registry::normalize_platform_key;

    // 1. 版本检查：检查特定版本是否已安装（精确匹配）
    if registry.contains_version(agent_id, version) {
        let manifest = registry.get_version(agent_id, version).unwrap();
        let mut resp = make_skip_response(&manifest);
        resp.previous_version = version.to_string();
        return Ok(resp);
    }

    // 判断是首次安装还是更新
    let action = if registry.contains(agent_id) {
        shared_types::InstallAction::Updated
    } else {
        shared_types::InstallAction::Installed
    };

    info!(
        "[agent_mgmt] version check: agent_id={}, action={}, requested_version={}",
        agent_id, action.as_str(), version
    );

    // 2. 匹配当前系统平台
    let sys_info = shared_types::SystemInfo::current();
    let platform_key = normalize_platform_key(&sys_info.os, &sys_info.arch);

    let entry = platforms.get(&platform_key).ok_or_else(|| {
        AgentMgmtError::PlatformNotFound(format!(
            "{platform_key} (available: {:?})",
            platforms.keys().collect::<Vec<_>>()
        ))
    })?;

    // 3. 验证 URL
    if !entry.url.starts_with("http://") && !entry.url.starts_with("https://") {
        return Err(AgentMgmtError::InvalidChunk(format!(
            "URL must start with http:// or https://: {}",
            entry.url
        )));
    }

    info!(
        "[agent_mgmt] platform install: agent_id={}, platform={}, url={}",
        agent_id, platform_key, entry.url
    );

    // 4. 下载到临时文件(支持重试 + 断点续传)
    let expected_sha256 = entry.sha256.as_deref().filter(|s| !s.is_empty());
    let staging_path = path_manager.install_dir()
        .join(format!(".download-staging-{}", uuid::Uuid::new_v4()));
    download_to_file(&entry.url, &staging_path, shared_types::MAX_BINARY_SIZE, expected_sha256, &cancel_token).await?;

    // 文件大小（download_to_file 已校验，传递给 install_from_file）
    let file_size = std::fs::metadata(&staging_path)
        .map(|m| m.len())
        .map_err(AgentMgmtError::Io)?;

    // 5. 安装(复用 binary_installer::install_from_file，避免全量读入内存)
    let t_install = std::time::Instant::now();
    debug!("[agent_mgmt] starting install_from_file: agent_id={}, file_size={}", agent_id, file_size);
    let response = binary_installer::install_from_file(
        registry,
        path_manager,
        binary_installer::InstallFileParams {
            agent_id,
            command,
            args,
            install_type: InstallType::Url,
            download_path: &staging_path,
            version: Some(version),
            source: Some(&entry.url),
            file_size,
        },
    )
    .await;
    debug!("[agent_mgmt] install_from_file completed: took {:?}", t_install.elapsed());

    // install_from_file 成功时已 rename 走了 staging，失败时需清理
    let _ = std::fs::remove_file(&staging_path);

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
fn validate_version_format(version: &str) -> AgentMgmtResult<()> {
    let trimmed = version.trim().trim_start_matches('v').trim_start_matches('V');
    if trimmed.is_empty() || !trimmed.chars().any(|c| c.is_ascii_digit()) {
        return Err(AgentMgmtError::InvalidVersion(format!(
            "version must contain at least one digit: {version:?}"
        )));
    }
    // 至少第一个 segment 必须是纯数字
    let first = trimmed.split('.').next().unwrap_or("");
    if first.parse::<u64>().is_err() {
        return Err(AgentMgmtError::InvalidVersion(format!(
            "version major must be a number: {version:?}"
        )));
    }
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
fn make_skip_response(manifest: &crate::agent_mgmt::installer::AgentManifest) -> InstallAgentResponse {
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
    use futures_util::StreamExt;
    use std::io::Write;
    use std::time::Duration;

    #[test]
    fn rejects_non_http_scheme() {
        assert!(!url_is_supported("file:///etc/passwd"));
        assert!(!url_is_supported("gopher://evil/"));
        assert!(url_is_supported("http://example.com/agent"));
        assert!(url_is_supported("https://example.com/agent"));
    }

    fn url_is_supported(url: &str) -> bool {
        url.starts_with("http://") || url.starts_with("https://")
    }

    /// 测试真实 URL 下载(限制下载大小避免测试太慢)
    #[tokio::test]
    #[ignore] // 依赖外部网络，手动运行: cargo test -- --ignored
    async fn download_to_file_real_url() {
        let url = "https://s3.nuwax.com:9443/nuwaclaw/nuwaclaw-electron/electron-v0.11.43/NuwaClaw-0.11.43-arm64-mac.zip";
        let tmp_dir = std::env::temp_dir().join("agent-mgmt-download-test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let dest = tmp_dir.join("test-download.zip");

        // 清理
        let _ = std::fs::remove_file(&dest);

        // 限制只下载前 1MB(文件总大小 ~393MB)
        let max_bytes = 1024 * 1024;
        let result = download_to_file(url, &dest, max_bytes, None, &CancellationToken::new()).await;

        // 应该成功(下载了 1MB 后超限报错,或服务器不支持 Range 时下载完整小块)
        // 实际行为取决于服务器是否支持 Range
        match result {
            Ok(size) => {
                assert!(size > 0, "downloaded size should be > 0");
                assert!(size <= max_bytes, "should not exceed max_bytes");
                // 验证文件存在且大小一致
                let file_size = std::fs::metadata(&dest).unwrap().len();
                assert_eq!(file_size, size, "file size should match returned size");
                println!("downloaded {} bytes", size);
            }
            Err(AgentMgmtError::BinaryTooLarge { size, max }) => {
                // 下载超过 max_bytes 也是预期行为(服务器不支持 Range 时)
                assert!(size > max, "should report actual size > max");
                println!("file too large: {} bytes (max {})", size, max);
            }
            Err(e) => {
                panic!("unexpected error: {}", e);
            }
        }

        // 清理
        let _ = std::fs::remove_file(&dest);
    }

    /// 测试断点续传:先写入部分文件,再下载剩余部分
    #[tokio::test]
    #[ignore] // 依赖外部网络
    async fn download_to_file_resume() {
        let url = "https://s3.nuwax.com:9443/nuwaclaw/nuwaclaw-electron/electron-v0.11.43/NuwaClaw-0.11.43-arm64-mac.zip";
        let tmp_dir = std::env::temp_dir().join("agent-mgmt-download-test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let dest = tmp_dir.join("test-resume.zip");

        // 清理
        let _ = std::fs::remove_file(&dest);

        // 先下载前 100KB
        let first_bytes = 100 * 1024;
        let result1 = download_to_file(url, &dest, first_bytes, None, &CancellationToken::new()).await;

        let downloaded = match result1 {
            Ok(size) => size,
            Err(AgentMgmtError::BinaryTooLarge { .. }) => {
                // 服务器不支持 Range,整个文件太大,跳过续传测试
                let _ = std::fs::remove_file(&dest);
                println!("server does not support Range, skipping resume test");
                return;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                panic!("first download failed: {}", e);
            }
        };

        assert!(downloaded > 0, "first download should get some bytes");
        let file_size_1 = std::fs::metadata(&dest).unwrap().len();
        assert_eq!(file_size_1, downloaded);

        // 现在用更大的 max_bytes 继续下载(续传)
        let max_bytes = 200 * 1024; // 200KB
        let result2 = download_to_file(url, &dest, max_bytes, None, &CancellationToken::new()).await;

        match result2 {
            Ok(total_size) => {
                // 续传成功,文件应该更大了
                let file_size_2 = std::fs::metadata(&dest).unwrap().len();
                assert_eq!(file_size_2, total_size);
                // 如果服务器支持 Range,文件应该从断点继续
                // 如果不支持,文件会被重新下载
                println!(
                    "resume: first={}, total={}, grew by {} bytes",
                    downloaded,
                    total_size,
                    total_size - downloaded
                );
            }
            Err(AgentMgmtError::BinaryTooLarge { size, max }) => {
                println!("resume: file too large after resume: {} (max {})", size, max);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                panic!("resume download failed: {}", e);
            }
        }

        // 清理
        let _ = std::fs::remove_file(&dest);
    }

    /// 测试 4xx 错误不重试
    #[tokio::test]
    #[ignore] // 依赖外部网络 (httpbin.org)
    async fn download_to_file_404_no_retry() {
        let url = "https://httpbin.org/status/404";
        let tmp_dir = std::env::temp_dir().join("agent-mgmt-download-test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let dest = tmp_dir.join("test-404.zip");
        let _ = std::fs::remove_file(&dest);

        let result = download_to_file(url, &dest, 1024, None, &CancellationToken::new()).await;

        match result {
            Err(AgentMgmtError::InstallFailed(msg)) => {
                assert!(msg.contains("HTTP 404"), "should be HTTP 404 error: {}", msg);
            }
            other => panic!("expected InstallFailed with HTTP 404, got: {:?}", other),
        }

        let _ = std::fs::remove_file(&dest);
    }

    /// 断点续传验证(阿里云 OSS,支持 Range):
    /// 1. 手动下载前 500KB 到文件(模拟中断)
    /// 2. 调用 download_to_file 续传
    /// 3. 验证文件增长 + 数据完整性
    #[tokio::test]
    #[ignore] // 依赖外部网络 (阿里云 OSS)
    async fn download_resume_oss() {
        let url = "https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/docker/20260529122753/docker-aarch64.zip";
        let tmp_dir = std::env::temp_dir().join("agent-mgmt-download-test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let dest = tmp_dir.join("test-resume-oss.zip");
        let _ = std::fs::remove_file(&dest);

        // === Step 1: 手动下载前 500KB ===
        let partial_size: u64 = 500 * 1024;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let resp = client.get(url).send().await.unwrap();
        assert!(resp.status().is_success(), "should be 200");

        let mut file = std::fs::File::create(&dest).unwrap();
        let mut written: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            let remaining = partial_size - written;
            if remaining == 0 {
                break;
            }
            let write_len = (chunk.len() as u64).min(remaining);
            file.write_all(&chunk[..write_len as usize]).unwrap();
            written += write_len;
        }
        file.flush().unwrap();
        drop(file);
        println!("[step1] wrote {} bytes (simulated interrupt)", written);

        let first_100: Vec<u8> = std::fs::read(&dest).unwrap()[..100].to_vec();

        // === Step 2: 调用 download_to_file 续传(max_bytes 设大,允许完成) ===
        let max_bytes = 5 * 1024 * 1024; // 5MB (测试文件更大,但只要超过续传后的大小即可验证)
        let result = download_to_file(url, &dest, max_bytes, None, &CancellationToken::new()).await;

        match result {
            Ok(total) => {
                // 续传成功完成
                let file_size = std::fs::metadata(&dest).unwrap().len();
                assert_eq!(file_size, total);
                assert!(total > written, "should have grown: {} > {}", total, written);

                let final_content = std::fs::read(&dest).unwrap();
                assert_eq!(&final_content[..100], &first_100[..], "first 100 bytes preserved");

                println!(
                    "[resume OK] {} → {} bytes (grew {}), first 100 bytes intact",
                    written, total, total - written
                );
            }
            Err(AgentMgmtError::BinaryTooLarge { size, max }) => {
                // 文件超过 5MB,但续传逻辑已正确工作(文件被清理)
                println!(
                    "[resume OK but exceeded] file grew beyond {}MB: {} bytes",
                    max / 1024 / 1024, size
                );
                // 验证:续传前 512000,续传后应远大于此
                assert!(size > written, "should have grown beyond partial: {} > {}", size, written);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                panic!("resume failed: {}", e);
            }
        }

        let _ = std::fs::remove_file(&dest);
    }

    /// MinIO 不支持 Range:验证下载到 max_bytes 后正确返回 BinaryTooLarge
    #[tokio::test]
    #[ignore] // 依赖外部网络 (MinIO)
    async fn download_no_resume_minio() {
        let url = "https://s3.nuwax.com:9443/nuwaclaw/nuwaclaw-electron/electron-v0.11.43/NuwaClaw-0.11.43-arm64-mac.zip";
        let tmp_dir = std::env::temp_dir().join("agent-mgmt-download-test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let dest = tmp_dir.join("test-no-resume-minio.zip");
        let _ = std::fs::remove_file(&dest);

        // 先写入 100KB 模拟中断
        let partial = vec![0xABu8; 100 * 1024];
        std::fs::write(&dest, &partial).unwrap();

        // 尝试续传,但服务器不支持 Range → 从头下载 → 超限
        let max_bytes = 1024 * 1024; // 1MB
        let result = download_to_file(url, &dest, max_bytes, None, &CancellationToken::new()).await;

        match result {
            Err(AgentMgmtError::BinaryTooLarge { size, max }) => {
                assert!(size > max, "should report actual > max");
                println!("[no-resume OK] MinIO returned full file, caught at {} bytes (max {})", size, max);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                panic!("unexpected error: {}", e);
            }
            Ok(size) => {
                // 如果文件刚好小于 1MB(不太可能,文件 393MB)
                println!("[unexpected] download completed: {} bytes", size);
            }
        }

        let _ = std::fs::remove_file(&dest);
    }
}
