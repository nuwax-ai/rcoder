//! 工具函数模块
//!
//! 包含代理服务使用的通用工具函数。

use pingora_core::Result as PingoraResult;
use pingora_http::RequestHeader;
use tracing::error;

/// 规范化路径（去除尾部斜杠）
///
/// # 规则
/// - `"/"` → `"/"`（保留根路径）
/// - `"/foo/"` → `"/foo"`
/// - `"/foo/bar/"` → `"/foo/bar"`
pub fn normalize_path(raw: &str) -> &str {
    if raw.len() > 1 {
        raw.trim_end_matches('/')
    } else {
        raw
    }
}

/// 统一的 URI 重写方法，消除重复代码
pub fn rewrite_uri(original_uri: &http::Uri, target_path: String) -> PingoraResult<http::Uri> {
    let new_uri_str = if let Some(query) = original_uri.query() {
        format!("{}?{}", target_path, query)
    } else {
        target_path
    };

    new_uri_str.parse().map_err(|e| {
        error!("URI rewrite failed: {} - {}", new_uri_str, e);
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })
}

/// 设置通用请求头
pub fn set_common_headers(upstream_request: &mut RequestHeader) -> PingoraResult<()> {
    upstream_request.insert_header("X-Forwarded-Proto", "http")?;
    Ok(())
}

/// 对 header value 进行脱敏，保留前 4 和后 4 个字符
pub fn mask_header_value(value: &str) -> String {
    if value.len() <= 10 {
        return "***".to_string();
    }
    // 使用 char-based 切片避免 UTF-8 边界 panic
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 10 {
        return "***".to_string();
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{}***{}", prefix, suffix)
}

/// 对 URL 进行脱敏处理，隐藏域名中间部分
///
/// # 示例
/// - `https://anthropic-code-api.nuwax.com/api/...` -> `https://ant***ax.com/api/...`
/// - `https://api.openai.com/v1/chat` -> `https://api***ai.com/v1/chat`
pub fn mask_url(url: &str) -> String {
    // 尝试解析 URL
    if let Ok(parsed_url) = url::Url::parse(url)
        && let Some(host) = parsed_url.host_str()
    {
        let masked_host = mask_domain(host);
        // 重新构建 URL，保留协议、端口、路径等
        let scheme = parsed_url.scheme();
        let port = parsed_url
            .port()
            .map(|p| format!(":{}", p))
            .unwrap_or_default();
        let path = parsed_url.path();
        let query = parsed_url
            .query()
            .map(|q| format!("?{}", q))
            .unwrap_or_default();
        return format!("{}://{}{}{}{}", scheme, masked_host, port, path, query);
    }

    // 如果解析失败，直接返回原始 URL（不应该发生）
    url.to_string()
}

/// 对域名进行脱敏处理
///
/// # 规则
/// - 保留前 3 个字符和后 6 个字符（包括顶级域名）
/// - 中间部分用 `***` 替代
/// - `localhost` 和短域名（<=6 字符）不做脱敏处理
///
/// # 示例
/// - `anthropic-code-api.nuwax.com` -> `ant***ax.com`
/// - `api.openai.com` -> `api***ai.com`
/// - `localhost` -> `localhost` (特殊域名不脱敏)
pub fn mask_domain(domain: &str) -> String {
    // 使用字符而非字节处理，避免 Unicode 边界问题
    let chars: Vec<char> = domain.chars().collect();
    let len = chars.len();

    // 如果域名太短（小于等于 6 个字符），或者特定域名（localhost），不脱敏
    if len <= 6 || domain == "localhost" {
        return domain.to_string();
    }

    // 如果域名较短（小于等于 10 个字符），保留首尾各 3 个字符
    if len <= 10 {
        let prefix: String = chars[..3].iter().collect();
        let suffix: String = chars[len - 3..].iter().collect();
        return format!("{}***{}", prefix, suffix);
    }

    // 正常情况：保留前 3 个字符和后 6 个字符
    let prefix: String = chars[..3].iter().collect();
    let suffix: String = chars[len - 6..].iter().collect();
    format!("{}***{}", prefix, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path() {
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("/foo"), "/foo");
        assert_eq!(normalize_path("/foo/"), "/foo");
        assert_eq!(normalize_path("/foo/bar/"), "/foo/bar");
        assert_eq!(normalize_path("/foo/bar"), "/foo/bar");
    }

    #[test]
    fn test_mask_header_value() {
        assert_eq!(mask_header_value("short"), "***");
        assert_eq!(mask_header_value("1234567890"), "***");
        assert_eq!(mask_header_value("12345678901"), "1234***8901");
        assert_eq!(mask_header_value("abcdefghijklmnop"), "abcd***mnop");
    }

    #[test]
    fn test_mask_domain() {
        assert_eq!(mask_domain("localhost"), "localhost");
        assert_eq!(mask_domain("short"), "short");
        assert_eq!(mask_domain("api.openai.com"), "api***ai.com");
        assert_eq!(mask_domain("anthropic-code-api.nuwax.com"), "ant***ax.com");
    }

    #[test]
    fn test_mask_url() {
        assert_eq!(
            mask_url("https://api.openai.com/v1/chat"),
            "https://api***ai.com/v1/chat"
        );
        assert_eq!(
            mask_url("https://anthropic-code-api.nuwax.com/api/v1/messages"),
            "https://ant***ax.com/api/v1/messages"
        );
        assert_eq!(
            mask_url("http://localhost:8080/api/test"),
            "http://localhost:8080/api/test"
        );
    }

    #[test]
    fn test_rewrite_uri() {
        let uri: http::Uri = "/original/path?query=1".parse().unwrap();
        let new_uri = rewrite_uri(&uri, "/new/path".to_string()).unwrap();
        assert_eq!(new_uri.path(), "/new/path");
        assert_eq!(new_uri.query(), Some("query=1"));

        let uri_no_query: http::Uri = "/original/path".parse().unwrap();
        let new_uri = rewrite_uri(&uri_no_query, "/new/path".to_string()).unwrap();
        assert_eq!(new_uri.path(), "/new/path");
        assert_eq!(new_uri.query(), None);
    }
}
