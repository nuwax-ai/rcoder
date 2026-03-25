//! 语言检测工具
//!
//! 从 HTTP 请求中提取语言偏好

use axum::http::HeaderMap;
use shared_types::parse_accept_language;

/// 从 HTTP 请求头获取语言
///
/// # Arguments
/// * `headers` - HTTP 请求头
///
/// # Returns
/// 语言代码，如 "zh-CN", "en-US"
pub fn get_locale_from_headers(headers: &HeaderMap) -> &'static str {
    let accept_language = headers
        .get("Accept-Language")
        .and_then(|v| v.to_str().ok());

    parse_accept_language(accept_language)
}

/// 从 HTTP 请求头获取语言（返回 String）
///
/// # Arguments
/// * `headers` - HTTP 请求头
///
/// # Returns
/// 语言代码，如 "zh-CN", "en-US"
pub fn get_locale_string_from_headers(headers: &HeaderMap) -> String {
    get_locale_from_headers(headers).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderName;

    #[test]
    fn test_get_locale_from_headers_zh_cn() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept-language"),
            "zh-CN".parse().unwrap(),
        );
        assert_eq!(get_locale_from_headers(&headers), "zh-CN");
    }

    #[test]
    fn test_get_locale_from_headers_zh_tw() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept-language"),
            "zh-TW".parse().unwrap(),
        );
        assert_eq!(get_locale_from_headers(&headers), "zh-TW");
    }

    #[test]
    fn test_get_locale_from_headers_zh_hk() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept-language"),
            "zh-HK".parse().unwrap(),
        );
        assert_eq!(get_locale_from_headers(&headers), "zh-TW");
    }

    #[test]
    fn test_get_locale_from_headers_en_us() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept-language"),
            "en-US".parse().unwrap(),
        );
        assert_eq!(get_locale_from_headers(&headers), "en-US");
    }

    #[test]
    fn test_get_locale_from_headers_none() {
        let headers = HeaderMap::new();
        assert_eq!(get_locale_from_headers(&headers), "en-US"); // 默认值
    }
}
