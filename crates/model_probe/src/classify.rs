//! 错误分级:把 genai 的 `Result<ChatResponse>` 转为 `ProbeResult`。
//!
//! 纯函数,便于单测(不需要真实网络请求)。分级规则:
//! - 2xx(Ok)→ Available
//! - 5xx → Unavailable(拦截)
//! - 4xx(含 401/403/429)→ Inconclusive(fail-open,防怪网关 auth 误杀)
//! - `is_connect()`(连接拒绝/DNS/TLS)→ Unavailable(拦截)
//! - 其他 reqwest 错误(超时/body/redirect 等)→ Inconclusive(拿不准,放行)
//! - JSON 解析等 → Inconclusive

use genai::chat::ChatResponse;

use crate::ProbeResult;

/// 把 genai 结果分级为 ProbeResult
pub(crate) fn classify(result: genai::Result<ChatResponse>) -> ProbeResult {
    match result {
        Ok(_) => ProbeResult::Available,
        Err(e) => match e {
            genai::Error::WebModelCall { webc_error, .. } => classify_webc_error(webc_error),
            _ => ProbeResult::Inconclusive, // resolver / 内部错误 → fail-open
        },
    }
}

/// 把 webc::Error 分级
fn classify_webc_error(error: genai::webc::Error) -> ProbeResult {
    match error {
        genai::webc::Error::ResponseFailedStatus { status, .. } => classify_http_status(status),
        genai::webc::Error::Reqwest(r) => {
            // timeout 优先于 connect:连接阶段超时同时满足 is_connect + is_timeout,
            // 但语义上是"超时"(拿不准,放行),不是"连接拒绝"(明确不可达,拦截)
            if should_block_reqwest(r.is_timeout(), r.is_connect()) {
                ProbeResult::Unavailable(format!("connect: {r}"))
            } else {
                ProbeResult::Inconclusive
            }
        }
        _ => ProbeResult::Inconclusive, // JSON 解析等 → 放行
    }
}

/// reqwest 错误的拦截判定(纯函数,便于单测)。
///
/// 只有"非超时的连接错误"才拦截(connect refused / DNS / TLS)。
/// 超时(含连接超时)一律放行 —— 可能只是慢,不误杀。
fn should_block_reqwest(is_timeout: bool, is_connect: bool) -> bool {
    !is_timeout && is_connect
}

/// HTTP 状态码分级:5xx → 不可用(拦截),其余 → 拿不准(fail-open 放行)
///
/// 注:2xx 成功路径走 `Ok(_)`,不会到达这里。到达这里的只有 4xx/5xx。
fn classify_http_status(status: reqwest::StatusCode) -> ProbeResult {
    if status.is_server_error() {
        ProbeResult::Unavailable(format!("HTTP {status}"))
    } else {
        ProbeResult::Inconclusive // 401/403/429/4xx → 放行(防怪网关 auth 误杀)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- classify_http_status ---

    #[test]
    fn test_classify_5xx_unavailable() {
        assert_eq!(
            classify_http_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            ProbeResult::Unavailable("HTTP 500 Internal Server Error".to_string())
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::BAD_GATEWAY),
            ProbeResult::Unavailable("HTTP 502 Bad Gateway".to_string())
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            ProbeResult::Unavailable("HTTP 503 Service Unavailable".to_string())
        );
    }

    #[test]
    fn test_classify_4xx_inconclusive() {
        // 401/403/429 → fail-open(防怪网关 auth 误杀)
        assert_eq!(
            classify_http_status(reqwest::StatusCode::UNAUTHORIZED),
            ProbeResult::Inconclusive
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::FORBIDDEN),
            ProbeResult::Inconclusive
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            ProbeResult::Inconclusive
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::NOT_FOUND),
            ProbeResult::Inconclusive
        );
    }

    // --- classify_webc_error ---

    #[test]
    fn test_classify_webc_500() {
        let error = genai::webc::Error::ResponseFailedStatus {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: r#"{"error":{"message":"null"}}"#.to_string(), // 线上故障的实际响应体
            headers: Box::new(reqwest::header::HeaderMap::new()),
        };
        assert_eq!(
            classify_webc_error(error),
            ProbeResult::Unavailable("HTTP 500 Internal Server Error".to_string())
        );
    }

    #[test]
    fn test_classify_webc_401_inconclusive() {
        let error = genai::webc::Error::ResponseFailedStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: String::new(),
            headers: Box::new(reqwest::header::HeaderMap::new()),
        };
        assert_eq!(classify_webc_error(error), ProbeResult::Inconclusive);
    }

    #[test]
    fn test_classify_webc_not_json_inconclusive() {
        // JSON 解析类错误 → fail-open
        let error = genai::webc::Error::ResponseFailedNotJson {
            content_type: "text/html".to_string(),
            body: "<html>...</html>".to_string(),
        };
        assert_eq!(classify_webc_error(error), ProbeResult::Inconclusive);
    }

    // --- should_block_reqwest(timeout 优先于 connect) ---

    #[test]
    fn test_should_block_timeout_overrides_connect() {
        // 连接阶段超时:is_connect=true + is_timeout=true → 放行(timeout 优先)
        assert!(!should_block_reqwest(true, true));
    }

    #[test]
    fn test_should_block_timeout_only() {
        // 纯超时(非连接错误)→ 放行
        assert!(!should_block_reqwest(true, false));
    }

    #[test]
    fn test_should_block_connect_only() {
        // 纯连接错误(非超时)→ 拦截
        assert!(should_block_reqwest(false, true));
    }

    #[test]
    fn test_should_block_neither() {
        // 既非超时也非连接(body/redirect 等)→ 放行
        assert!(!should_block_reqwest(false, false));
    }
}
