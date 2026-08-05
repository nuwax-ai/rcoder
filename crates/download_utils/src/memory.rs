//! 小体积资源的内存下载: 直接读到 `Bytes`/`String`, 不落盘。
//!
//! 与 [`crate::downloader::Downloader`] (大文件落盘 + 重试 + 断点续传 + SHA-256)
//! 互补, 适用于附件/配置等小内容, 统一解决裸 `reqwest::Client::new()` 的三大问题:
//! - **无超时**: 慢/挂起的远端会无限占用任务; 连接超时 + 整体超时兜底;
//! - **无状态校验**: 404/500 错误页会被当成内容; `error_for_status()` 拦截;
//! - **无响应体上限**: 超大响应可撑爆内存; 流式累积并按字节数上限提前中止。
//!
//! 注: 本模块**不做** SSRF/内网地址限制 —— 产品支持内网私有化部署,
//! 附件/skill 常托管在内网地址, IP 黑名单会误伤正常业务。

use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;

use crate::error::DownloadError;

/// 连接建立超时 (秒)。
pub const CONNECT_TIMEOUT_SECS: u64 = 10;
/// 单次下载整体超时 (秒; reqwest 语义为从请求发出到响应体读完)。
pub const TIMEOUT_SECS: u64 = 60;
/// 响应体默认上限 (50 MiB, 与 file-server FileUtils 默认 max_file_size 对齐)。
pub const DEFAULT_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// 进程级共享内存下载客户端 (带超时; 首次调用时惰性构建)。
///
/// 与 [`crate::downloader`] 的文件下载 client 分开: 后者为断点续传禁用了
/// 自动重定向, 且按请求设置超时; 本 client 使用 reqwest 默认重定向策略。
pub fn shared_client() -> Result<&'static reqwest::Client, DownloadError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| DownloadError::Http(format!("build memory download client: {e}")))?;
    // 并发首调时可能重复构建, get_or_init 只保留其一, 多余实例丢弃即可。
    Ok(CLIENT.get_or_init(|| client))
}

/// GET 下载 `url`, 校验 HTTP 状态并按 `max_bytes` 流式限制响应体大小。
pub async fn download_bytes_limited(url: &str, max_bytes: u64) -> Result<Bytes, DownloadError> {
    let client = shared_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| DownloadError::Http(format!("download from {url}: {e}")))?
        // 4xx/5xx 直接报错, 避免错误页被当作内容
        .error_for_status()
        .map_err(|e| DownloadError::Http(format!("error status from {url}: {e}")))?;

    // Content-Length 已知时快速拒绝
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(DownloadError::BinaryTooLarge {
            size: len,
            max: max_bytes,
        });
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DownloadError::Http(format!("read body from {url}: {e}")))?;
        buf.extend_from_slice(&chunk);
        if buf.len() as u64 > max_bytes {
            return Err(DownloadError::BinaryTooLarge {
                size: buf.len() as u64,
                max: max_bytes,
            });
        }
    }
    Ok(buf.into())
}

/// GET 下载 `url` 并解码为 UTF-8 文本 (同 [`download_bytes_limited`] 的安全约束)。
pub async fn download_text_limited(url: &str, max_bytes: u64) -> Result<String, DownloadError> {
    let bytes = download_bytes_limited(url, max_bytes).await?;
    String::from_utf8(bytes.to_vec())
        .map_err(|e| DownloadError::Http(format!("content is not valid UTF-8 ({url}): {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use std::net::SocketAddr;

    async fn start_server(app: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // 轮询等待服务器就绪
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "test server did not become ready within 2s"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        addr
    }

    /// 带 Content-Length 的固定内容 handler
    async fn handler_ok() -> impl IntoResponse {
        let body = vec![b'a'; 1024];
        let mut resp = axum::response::Response::new(axum::body::Body::from(body.clone()));
        resp.headers_mut().insert(
            header::CONTENT_LENGTH,
            body.len().to_string().parse().unwrap(),
        );
        resp
    }

    async fn handler_404() -> impl IntoResponse {
        StatusCode::NOT_FOUND
    }

    /// 不带 Content-Length、分块输出超大数据的 handler (模拟 chunked 流)
    async fn handler_streaming_large() -> impl IntoResponse {
        use futures_util::stream;
        // 10 个 chunk × 4KB = 40KB，无 Content-Length
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            (0..10).map(|_| Ok(vec![b'x'; 4 * 1024])).collect();
        let stream = stream::iter(chunks);
        let body = axum::body::Body::from_stream(stream);
        axum::response::Response::new(body)
    }

    #[tokio::test]
    async fn downloads_body_within_limit() {
        let app = Router::new().route("/file", get(handler_ok));
        let addr = start_server(app).await;
        let url = format!("http://{addr}/file");

        let bytes = download_bytes_limited(&url, 1024 * 1024).await.unwrap();
        assert_eq!(bytes.len(), 1024);
        assert!(bytes.iter().all(|b| *b == b'a'));

        let text = download_text_limited(&url, 1024 * 1024).await.unwrap();
        assert_eq!(text.len(), 1024);
    }

    /// 回归: 404 错误页不能被当作附件内容 (修复前会返回错误页 HTML)。
    #[tokio::test]
    async fn rejects_error_status_instead_of_returning_error_page() {
        let app = Router::new().route("/missing", get(handler_404));
        let addr = start_server(app).await;
        let url = format!("http://{addr}/missing");

        let err = download_bytes_limited(&url, 1024 * 1024)
            .await
            .expect_err("404 must be rejected");
        assert!(
            matches!(&err, DownloadError::Http(msg) if msg.contains("404")),
            "error should mention status: {err}"
        );
    }

    /// 回归: Content-Length 已知且超限时快速拒绝。
    #[tokio::test]
    async fn rejects_known_content_length_over_limit() {
        let app = Router::new().route("/file", get(handler_ok));
        let addr = start_server(app).await;
        let url = format!("http://{addr}/file");

        // 服务端 Content-Length=1024，限制 512 → 快速拒绝
        let err = download_bytes_limited(&url, 512)
            .await
            .expect_err("must reject oversized body");
        assert!(
            matches!(err, DownloadError::BinaryTooLarge { .. }),
            "error: {err}"
        );
    }

    /// 回归: 无 Content-Length 的流式响应也要按字节数中止 (修复前会全量读入内存)。
    #[tokio::test]
    async fn rejects_streaming_body_over_limit_without_content_length() {
        let app = Router::new().route("/stream", get(handler_streaming_large));
        let addr = start_server(app).await;
        let url = format!("http://{addr}/stream");

        let err = download_bytes_limited(&url, 8 * 1024)
            .await
            .expect_err("streaming body must be aborted over limit");
        assert!(
            matches!(err, DownloadError::BinaryTooLarge { .. }),
            "error: {err}"
        );
    }
}
