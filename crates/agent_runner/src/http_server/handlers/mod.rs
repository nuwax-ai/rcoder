//! HTTP 请求处理器
//!
//! 仅在 `http-server` feature 启用时编译

use axum::http::HeaderMap;

pub mod computer_cancel;
pub mod computer_chat;
pub mod computer_progress;
pub mod computer_status;
pub mod computer_stop;
pub mod rcoder_progress;  // RCoder 模式的 SSE 进度流

pub(super) fn locale_from_headers(headers: &HeaderMap) -> &'static str {
    let accept_language = headers.get("accept-language").and_then(|v| v.to_str().ok());
    shared_types::parse_accept_language(accept_language)
}
