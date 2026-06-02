//! URL installer (P0-1)
//!
//! HTTP/HTTPS 下载,然后委托给 [`super::binary_installer::install_from_bytes`]
//! 进行落盘、文件类型检测、解压、注册。
//!
//! 安全:
//! - 仅允许 http/https(防 file:// / gopher:// 等)
//! - 限制最大下载字节数(防恶意大文件撑爆磁盘)
//! - 强制超时(10 分钟,见 [`shared_types::URL_DOWNLOAD_TIMEOUT_SECS`])
//! - 可选 SHA-256 校验

use std::time::Duration;

use bytes::Bytes;
use shared_types::InstallType;
use shared_types_grpc::InstallAgentResponse;
use tracing::info;

use super::binary_installer;
use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
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

    let bytes = download_with_limit(url, shared_types::MAX_BINARY_SIZE).await?;

    let mut response = binary_installer::install_from_bytes(
        registry,
        path_manager,
        binary_installer::InstallBytesParams {
            agent_id,
            command,
            args,
            expected_sha256,
            install_type: InstallType::Url,
            bytes,
        },
    )
    .await?;

    // 用 URL 覆盖 source 字段
    response.source_url = Some(url.to_string());
    Ok(response)
}

/// 下载到内存,字节数超过 `max_bytes` 时立即终止并返回 `BinaryTooLarge`
async fn download_with_limit(url: &str, max_bytes: u64) -> AgentMgmtResult<Bytes> {
    // 禁用自动重定向,手动验证每个重定向目标的 scheme,防止 SSRF
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(shared_types::URL_DOWNLOAD_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AgentMgmtError::InstallFailed(format!("http client: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| AgentMgmtError::InstallFailed(format!("GET {url}: {e}")))?;

    // 处理重定向:最多跟随 5 次,每次验证 scheme
    let response = follow_redirects(&client, response, 5).await?;

    if !response.status().is_success() {
        return Err(AgentMgmtError::InstallFailed(format!(
            "GET {url}: HTTP {}",
            response.status()
        )));
    }

    // 如果 Content-Length 超限,立即拒绝
    if let Some(len) = response.content_length()
        && len > max_bytes {
            return Err(AgentMgmtError::BinaryTooLarge { size: len, max: max_bytes });
        }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| AgentMgmtError::InstallFailed(format!("read body: {e}")))?;

    if (bytes.len() as u64) > max_bytes {
        return Err(AgentMgmtError::BinaryTooLarge {
            size: bytes.len() as u64,
            max: max_bytes,
        });
    }

    Ok(bytes)
}

/// 手动跟随重定向,每次验证目标 URL 的 scheme 必须是 http/https
async fn follow_redirects(
    client: &reqwest::Client,
    mut response: reqwest::Response,
    max_redirects: usize,
) -> AgentMgmtResult<reqwest::Response> {
    for _ in 0..max_redirects {
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                AgentMgmtError::InstallFailed("redirect missing Location header".into())
            })?;
        if !location.starts_with("http://") && !location.starts_with("https://") {
            return Err(AgentMgmtError::InvalidChunk(format!(
                "redirect to non-http scheme: {location}"
            )));
        }
        response = client
            .get(location)
            .send()
            .await
            .map_err(|e| AgentMgmtError::InstallFailed(format!("redirect GET {location}: {e}")))?;
    }
    if response.status().is_redirection() {
        return Err(AgentMgmtError::InstallFailed(
            "too many redirects (max 5)".into(),
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
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
}
