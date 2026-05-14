//! 协议转换模块: Responses API <-> Chat Completions API
//!
//! 使用 codex-convert-proxy 库在 OpenAI Responses API (Codex 使用) 和
//! Chat Completions API (国内大模型提供商使用) 之间进行透明的双向协议转换。

use bytes::Bytes;
use dashmap::DashMap;
use codex_convert_proxy::{
    chat_chunk_to_response_events, chat_to_response_with_context, create_provider,
    event_to_sse, response_to_chat,
    types::chat_api::ChatStreamChunk,
    types::response_api::ResponseRequest,
    util::parse_sse,
    ResponseRequestContext, StreamState,
};
use codex_convert_proxy::convert::request::ToolPriority;
use pingora_core::Result as PingoraResult;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use shared_types::ModelProviderConfig;
use tracing::{debug, error, info, warn};
use std::sync::Arc;

/// Provider 缓存（避免重复调用 create_provider）
static PROVIDER_CACHE: std::sync::LazyLock<DashMap<String, Arc<dyn codex_convert_proxy::Provider + Send + Sync>>> =
    std::sync::LazyLock::new(|| DashMap::new());

/// 判断请求路径是否需要协议转换
/// Codex 发送请求到 `/v1/responses` 或 `/responses` 等路径
pub fn needs_conversion(api_path: &str) -> bool {
    let path = api_path.trim_end_matches('/');
    matches!(path, "responses" | "v1/responses" | "/responses" | "/v1/responses")
}

/// 判断 wire_api 是否为 "chat" (需要转换)
/// 当 wire_api 为 None 时，默认启用转换（国内模型提供商通常使用 Chat API）
pub fn is_chat_wire_api(config: &ModelProviderConfig) -> bool {
    config
        .wire_api
        .as_deref()
        .map(|v| v == "chat")
        .unwrap_or(true) // wire_api 为 None 时默认启用转换
}

/// 处理需要协议转换的请求
///
/// 返回 Ok(true) 表示请求已处理（调用者应返回 Ok(true) 跳过 Pingora 代理流程）
/// 返回 Ok(false) 表示此请求不需要转换（调用者应继续正常流程）
pub async fn handle_converted_request(
    session: &mut Session,
    config: &ModelProviderConfig,
    api_path: &str,
    http_client: &reqwest::Client,
) -> PingoraResult<bool> {
    debug!("[PROTOCOL_CONVERT] handle_converted_request called: path={}, config.name={}", api_path, config.name);

    // 只在 wire_api == "chat" 且路径包含 /response 时转换
    let wire_api_is_chat = is_chat_wire_api(config);
    let needs_conv = needs_conversion(api_path);
    debug!("[PROTOCOL_CONVERT] is_chat_wire_api={}, needs_conversion={}", wire_api_is_chat, needs_conv);

    if !wire_api_is_chat || !needs_conv {
        debug!("[PROTOCOL_CONVERT] Skipping conversion - is_chat={}, needs_conv={}", wire_api_is_chat, needs_conv);
        return Ok(false);
    }

    debug!("[PROTOCOL_CONVERT] Intercepting Responses API request: path={}, provider={}", api_path, config.name);
    info!(
        "[PROTOCOL_CONVERT] Intercepting Responses API request: path={}, provider={}",
        api_path, config.name
    );

    // 1. 读取完整的请求 body（带超时保护）
    debug!("[PROTOCOL_CONVERT] Starting to read request body...");
    let body_bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        read_full_request_body(session),
    )
    .await
    {
        Ok(Ok(bytes)) => {
            debug!("[PROTOCOL_CONVERT] Successfully read {} bytes from request body", bytes.len());
            bytes
        }
        Ok(Err(e)) => {
            error!("[PROTOCOL_CONVERT] Failed to read request body: {}", e);
            let status = match e.etype {
                pingora_core::ErrorType::HTTPStatus(code) => code,
                _ => 400,
            };
            write_error_response(session, status, "Failed to read request body").await?;
            return Ok(true);
        }
        Err(_) => {
            // 超时
            error!("[PROTOCOL_CONVERT] Timeout reading request body (30s)");
            write_error_response(session, 408, "Request body read timeout").await?;
            return Ok(true);
        }
    };

    // 2. 解析为 ResponseRequest
    let response_req: ResponseRequest = match serde_json::from_slice(&body_bytes) {
        Ok(req) => req,
        Err(e) => {
            error!("[PROTOCOL_CONVERT] Failed to parse ResponseRequest: {}", e);
            write_error_response(session, 400, &format!("Invalid Responses API request: {}", e))
                .await?;
            return Ok(true);
        }
    };

    let is_stream = response_req.stream;

    debug!("[PROTOCOL_CONVERT] ResponseRequest parsed, stream={}", is_stream);

    // 3. 获取或创建 Provider（使用缓存）
    // codex-convert-proxy 支持默认 fallback，未知 provider 会使用默认 provider
    let provider_name = &config.name;

    // 检查缓存
    let provider = if let Some(cached) = PROVIDER_CACHE.get(provider_name) {
        debug!("[PROTOCOL_CONVERT] Using cached provider for name={}", provider_name);
        cached.value().clone()
    } else {
        // 缓存未命中，创建 provider
        debug!("[PROTOCOL_CONVERT] Provider cache miss, creating provider for name={}", provider_name);

        let result = match create_provider(provider_name) {
            Ok(p) => {
                debug!("[PROTOCOL_CONVERT] Provider created: {}", provider_name);
                p
            }
            Err(e) => {
                error!(
                    "[PROTOCOL_CONVERT] Failed to create provider '{}': {}",
                    provider_name, e
                );
                write_error_response(session, 500, &format!("Unsupported provider: {}", provider_name))
                    .await?;
                return Ok(true);
            }
        };

        // 缓存 provider
        let cached = result.clone();
        PROVIDER_CACHE.insert(provider_name.to_string(), cached.clone());
        result
    };

    let request_context = ResponseRequestContext::from(&response_req);

    debug!("[PROTOCOL_CONVERT] Calling response_to_chat...");
    let chat_req = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        async {
            response_to_chat(
                response_req,
                provider.as_ref(),
                Some(&config.default_model),
                ToolPriority::Merge,
            )
        },
    )
    .await
    {
        Ok(Ok(req)) => {
            debug!("[PROTOCOL_CONVERT] response_to_chat succeeded");
            req
        }
        Ok(Err(e)) => {
            error!("[PROTOCOL_CONVERT] Request conversion failed: {}", e);
            write_error_response(
                session,
                400,
                &format!("Request conversion error: {}", e),
            )
            .await?;
            return Ok(true);
        }
        Err(_) => {
            error!("[PROTOCOL_CONVERT] response_to_chat timed out after 30s");
            write_error_response(session, 408, "Request conversion timeout").await?;
            return Ok(true);
        }
    };

    let chat_req_json = match serde_json::to_vec(&chat_req) {
        Ok(json) => json,
        Err(e) => {
            error!("[PROTOCOL_CONVERT] Failed to serialize ChatRequest: {}", e);
            write_error_response(session, 500, "Internal conversion error").await?;
            return Ok(true);
        }
    };

    debug!(
        "[PROTOCOL_CONVERT] Converted request: model={}, stream={}, messages={}",
        chat_req.model,
        is_stream,
        chat_req.messages.len()
    );

    // 4. 构建上游 URL (将 /v1/responses 替换为 /v1/chat/completions)
    let upstream_url = build_upstream_url(&config.base_url, api_path);
    debug!("[PROTOCOL_CONVERT] upstream_url={}", upstream_url);

    // 5. 构建认证 headers
    let mut req_builder = http_client
        .post(&upstream_url)
        .header("Content-Type", "application/json");

    let use_anthropic_auth = config
        .api_protocol
        .as_ref()
        .map(|p| p.to_lowercase() != "openai")
        .unwrap_or(!config.requires_openai_auth);

    if use_anthropic_auth {
        req_builder = req_builder.header("x-api-key", &config.api_key);
    } else {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", config.api_key));
    }

    req_builder = req_builder.body(chat_req_json);

    // 6. 发送请求并处理响应
    if is_stream {
        handle_streaming_response(session, req_builder, request_context, &config.name, &config.default_model).await?;
    } else {
        handle_non_streaming_response(session, req_builder, request_context, &config.name).await?;
    }

    Ok(true)
}

/// 最大请求体大小: 10MB
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// 从 Pingora session 读取完整的请求 body
async fn read_full_request_body(session: &mut Session) -> PingoraResult<Bytes> {
    let mut body_buf = Vec::new();
    while let Some(chunk) = session.downstream_session.read_request_body().await? {
        if body_buf.len() + chunk.len() > MAX_BODY_SIZE {
            error!(
                "[PROTOCOL_CONVERT] Request body exceeds max size ({} bytes)",
                MAX_BODY_SIZE
            );
            return Err(pingora_core::Error::new(
                pingora_core::ErrorType::HTTPStatus(413),
            ));
        }
        body_buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body_buf))
}

/// 构建上游 URL: 将 Responses API 路径转换为 Chat API 路径
///
/// api_path 来自路由 /api/{service_name}/{*path}，格式如 "v1/responses"
/// base_url 是模型提供商的完整 API 路径，如 "https://open.bigmodel.cn/api/coding/paas/v4"
///
/// 转换逻辑: 去掉 /responses 后缀和 version prefix (v1/)，直接追加 /chat/completions
fn build_upstream_url(base_url: &str, api_path: &str) -> String {
    let base = base_url.trim_end_matches('/');

    debug!(
        "[PROTOCOL_CONVERT] build_upstream_url: base_url={}, api_path={}",
        base, api_path
    );

    // 处理 /responses 或 responses 后缀
    let chat_path = if api_path.ends_with("/responses") || api_path.ends_with("responses") {
        // 去掉 /responses 或 responses 后缀
        let stripped = api_path
            .trim_end_matches("/responses")
            .trim_end_matches("responses");

        // 去掉 version prefix (v1/ 或 /v1/)，因为 base_url 已经包含完整 API 版本
        let cleaned = stripped
            .trim_start_matches("/v1/")
            .trim_start_matches("v1/")
            .trim_end_matches('/');

        format!("{}/chat/completions", cleaned)
    } else {
        // 不需要转换，直接返回原路径
        api_path.to_string()
    };

    let result = format!("{}{}", base, chat_path);
    debug!("[PROTOCOL_CONVERT] build_upstream_url result: {}", result);
    result
}

/// 处理非流式响应
async fn handle_non_streaming_response(
    session: &mut Session,
    req_builder: reqwest::RequestBuilder,
    request_context: ResponseRequestContext,
    provider_name: &str,
) -> PingoraResult<()> {
    // 发送请求到上游
    let resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            error!("[PROTOCOL_CONVERT] Upstream request failed: {}", e);
            write_error_response(session, 502, &format!("Upstream request failed: {}", e)).await?;
            return Ok(());
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let error_body = match resp.text().await {
            Ok(body) => body,
            Err(e) => {
                warn!("[PROTOCOL_CONVERT] Failed to read upstream error body: {}", e);
                String::new()
            }
        };
        warn!(
            "[PROTOCOL_CONVERT] Upstream error: status={}, body={}",
            status, error_body
        );
        // 透传上游错误
        let mut resp_header = ResponseHeader::build(status.as_u16(), None)?;
        resp_header.insert_header("Content-Type", "application/json")?;
        session
            .write_response_header(Box::new(resp_header), false)
            .await?;
        session
            .write_response_body(Some(Bytes::from(error_body)), true)
            .await?;
        return Ok(());
    }

    // 解析 Chat API 响应
    let chat_resp: codex_convert_proxy::types::chat_api::ChatResponse = match resp.json().await {
        Ok(r) => r,
        Err(e) => {
            error!("[PROTOCOL_CONVERT] Failed to parse upstream response: {}", e);
            write_error_response(session, 502, &format!("Invalid upstream response: {}", e))
                .await?;
            return Ok(());
        }
    };

    // 转换为 Responses API 格式
    let response_obj = match chat_to_response_with_context(chat_resp, Some(&request_context)) {
        Ok(r) => r,
        Err(e) => {
            error!("[PROTOCOL_CONVERT] Response conversion failed: {}", e);
            write_error_response(session, 500, &format!("Response conversion error: {}", e))
                .await?;
            return Ok(());
        }
    };

    // 序列化并返回
    let response_json = match serde_json::to_vec(&response_obj) {
        Ok(j) => j,
        Err(e) => {
            error!("[PROTOCOL_CONVERT] Failed to serialize response: {}", e);
            write_error_response(session, 500, "Internal serialization error").await?;
            return Ok(());
        }
    };

    let mut resp_header = ResponseHeader::build(200, None)?;
    resp_header.insert_header("Content-Type", "application/json")?;
    resp_header.insert_header(
        "Content-Length",
        response_json.len().to_string(),
    )?;

    session
        .write_response_header(Box::new(resp_header), false)
        .await?;
    session
        .write_response_body(Some(Bytes::from(response_json)), true)
        .await?;

    info!(
        "[PROTOCOL_CONVERT] Non-streaming response converted: provider={}",
        provider_name
    );
    Ok(())
}

/// 处理流式响应 (SSE)
async fn handle_streaming_response(
    session: &mut Session,
    req_builder: reqwest::RequestBuilder,
    request_context: ResponseRequestContext,
    provider_name: &str,
    model: &str,
) -> PingoraResult<()> {
    use futures_util::StreamExt;

    // 发送流式请求到上游
    let resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            error!("[PROTOCOL_CONVERT] Upstream streaming request failed: {}", e);
            write_error_response(session, 502, &format!("Upstream request failed: {}", e)).await?;
            return Ok(());
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let error_body = match resp.text().await {
            Ok(body) => body,
            Err(e) => {
                warn!("[PROTOCOL_CONVERT] Failed to read upstream streaming error body: {}", e);
                String::new()
            }
        };
        warn!(
            "[PROTOCOL_CONVERT] Upstream streaming error: status={}, body={}",
            status, error_body
        );
        let mut resp_header = ResponseHeader::build(status.as_u16(), None)?;
        resp_header.insert_header("Content-Type", "application/json")?;
        session
            .write_response_header(Box::new(resp_header), false)
            .await?;
        session
            .write_response_body(Some(Bytes::from(error_body)), true)
            .await?;
        return Ok(());
    }

    // 写入 SSE 响应头
    let mut resp_header = ResponseHeader::build(200, None)?;
    resp_header.insert_header("Content-Type", "text/event-stream")?;
    resp_header.insert_header("Cache-Control", "no-cache")?;
    resp_header.insert_header("Connection", "keep-alive")?;
    session
        .write_response_header(Box::new(resp_header), false)
        .await?;

    // 创建流式转换状态
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let mut stream_state = StreamState::new(response_id, model.to_string(), Some(request_context));

    // SSE 事件序列号
    let mut seq: u64 = 0;

    // 最大 SSE 缓冲区大小: 10MB
    const MAX_SSE_BUFFER_SIZE: usize = 10 * 1024 * 1024;

    // 逐 chunk 处理 SSE 流 (使用原始字节缓冲，避免 UTF-8 跨 chunk 边界损坏)
    let mut stream = resp.bytes_stream();
    let mut sse_raw_buf: Vec<u8> = Vec::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                warn!("[PROTOCOL_CONVERT] SSE stream error: {}", e);
                break;
            }
        };

        // 追加到原始字节缓冲区
        if sse_raw_buf.len() + chunk.len() > MAX_SSE_BUFFER_SIZE {
            error!(
                "[PROTOCOL_CONVERT] SSE buffer exceeded max size ({} bytes), aborting",
                MAX_SSE_BUFFER_SIZE
            );
            break;
        }
        sse_raw_buf.extend_from_slice(&chunk);

        // 归一化 SSE 行尾 (W3C spec §9.2.4):
        // 去掉所有 \r 字节，使得 \r\n、\r、\n 统一为 \n。
        // 确保 find_sse_boundary 和 parse_sse 不依赖上游的行尾约定。
        sse_raw_buf.retain(|&b| b != b'\r');

        // 查找最后一个完整的 SSE 事件边界 (\n\n)
        // 只解析到边界处，保留不完整的部分
        let parse_boundary = find_sse_boundary(&sse_raw_buf);
        if parse_boundary == 0 {
            continue; // 没有完整的事件，继续缓冲
        }

        let parseable = &sse_raw_buf[..parse_boundary];
        let sse_text = match std::str::from_utf8(parseable) {
            Ok(s) => s,
            Err(_) => {
                warn!("[PROTOCOL_CONVERT] Invalid UTF-8 in SSE data, skipping");
                sse_raw_buf.drain(..parse_boundary);
                continue;
            }
        };

        let (events, _) = parse_sse(sse_text);
        sse_raw_buf.drain(..parse_boundary);

        for event in &events {
            if event.data.is_empty() || event.data == "[DONE]" {
                continue;
            }

            let chat_chunk: ChatStreamChunk = match serde_json::from_str(&event.data) {
                Ok(c) => c,
                Err(e) => {
                    debug!(
                        "[PROTOCOL_CONVERT] Failed to parse SSE chunk (may be non-JSON event): {}",
                        e
                    );
                    continue;
                }
            };

            let response_events = match chat_chunk_to_response_events(&chat_chunk, &mut stream_state)
            {
                Ok(events) => events,
                Err(e) => {
                    warn!("[PROTOCOL_CONVERT] SSE event conversion failed: {}", e);
                    continue;
                }
            };

            for resp_event in &response_events {
                let sse_text = event_to_sse(resp_event, seq);
                seq += 1;
                session
                    .write_response_body(Some(Bytes::from(sse_text)), false)
                    .await?;
            }
        }
    }

    // 处理缓冲区中剩余的数据
    if !sse_raw_buf.is_empty() {
        // 归一化残余缓冲区（与主循环保持一致）
        sse_raw_buf.retain(|&b| b != b'\r');
        if let Ok(sse_text) = std::str::from_utf8(&sse_raw_buf) {
            let (events, _) = parse_sse(sse_text);
            for event in &events {
                if event.data.is_empty() || event.data == "[DONE]" {
                    continue;
                }
                if let Ok(chat_chunk) = serde_json::from_str::<ChatStreamChunk>(&event.data)
                    && let Ok(response_events) =
                        chat_chunk_to_response_events(&chat_chunk, &mut stream_state)
                    {
                        for resp_event in &response_events {
                            let sse_text = event_to_sse(resp_event, seq);
                            seq += 1;
                            session
                                .write_response_body(Some(Bytes::from(sse_text)), false)
                                .await?;
                        }
                    }
            }
        }
    }

    // 发送结束标记
    session.write_response_body(None, true).await?;

    info!(
        "[PROTOCOL_CONVERT] Streaming response completed: provider={}",
        provider_name
    );
    Ok(())
}

/// 写入错误响应到客户端
async fn write_error_response(
    session: &mut Session,
    status: u16,
    message: &str,
) -> PingoraResult<()> {
    let error_body = serde_json::json!({
        "error": {
            "message": message,
            "type": "proxy_error",
            "code": status,
        }
    });
    let body_bytes = match serde_json::to_vec(&error_body) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("[PROTOCOL_CONVERT] Failed to serialize error response: {}", e);
            // 使用最简单的 fallback 错误 JSON
            br#"{"error":{"message":"internal proxy error","type":"proxy_error","code":500}}"#.to_vec()
        }
    };

    let mut resp = ResponseHeader::build(status, None)?;
    resp.insert_header("Content-Type", "application/json")?;
    resp.insert_header("Content-Length", body_bytes.len().to_string())?;

    session.write_response_header(Box::new(resp), false).await?;
    session
        .write_response_body(Some(Bytes::from(body_bytes)), true)
        .await?;
    Ok(())
}

/// 查找 SSE 事件边界位置（\n\n 的起始位置）
/// 返回可以安全解析的字节长度（到边界处）
fn find_sse_boundary(data: &[u8]) -> usize {
    // 从后往前查找 \n\n，找到最后一个完整的事件边界
    let mut last_boundary = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == b'\n' && data[i + 1] == b'\n' {
            last_boundary = i + 2; // \n\n 之后的位置
        }
        i += 1;
    }
    last_boundary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_sse_boundary_lf_only() {
        let data = b"data: {\"x\":1}\n\ndata: {\"y\":2}\n\n";
        // find_sse_boundary 返回最后一个完整边界的位置 (30 = 第二个 \n\n 之后)
        assert_eq!(find_sse_boundary(data), 30);
    }

    #[test]
    fn test_find_sse_boundary_no_boundary() {
        let data = b"data: incomplete";
        assert_eq!(find_sse_boundary(data), 0);
    }

    #[test]
    fn test_normalize_crlf_produces_valid_boundary() {
        let mut buf = b"data: {\"x\":1}\r\n\r\ndata: {\"y\":2}\r\n\r\n".to_vec();
        buf.retain(|&b| b != b'\r');
        assert!(find_sse_boundary(&buf) > 0);
    }

    #[test]
    fn test_normalize_empty_buffer() {
        let mut buf: Vec<u8> = vec![];
        buf.retain(|&b| b != b'\r');
        assert_eq!(find_sse_boundary(&buf), 0);
    }
}
