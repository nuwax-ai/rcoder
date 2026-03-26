//! 语言检测工具
//!
//! 从 HTTP 请求中提取语言偏好
//! 优先级：HTTP Header > 环境变量 > 默认值 (en-US)

use axum::http::HeaderMap;
use shared_types::{SUPPORTED_LOCALES, parse_accept_language};
use std::sync::OnceLock;

/// 默认语言（硬编码回退值）
const FALLBACK_LOCALE: &str = "en-US";

/// 缓存的环境变量默认语言
static DEFAULT_LOCALE_CACHED: OnceLock<&'static str> = OnceLock::new();

/// 从环境变量获取默认语言（带缓存）
///
/// 环境变量名：DEFAULT_LOCALE
/// 如果未设置或无效，返回硬编码默认值 "en-US"
///
/// 使用 OnceLock 缓存，只在首次调用时读取环境变量
fn get_default_locale_from_env() -> &'static str {
    *DEFAULT_LOCALE_CACHED.get_or_init(|| {
        if let Ok(locale) = std::env::var("DEFAULT_LOCALE") {
            // 验证是否为支持的语言
            if SUPPORTED_LOCALES.contains(&locale.as_str()) {
                // 匹配已知的静态字符串，避免内存泄漏
                match locale.as_str() {
                    "zh-CN" => return "zh-CN",
                    "zh-TW" => return "zh-TW",
                    "en-US" => return "en-US",
                    _ => {}
                }
            }
        }
        FALLBACK_LOCALE
    })
}

/// 从 HTTP 请求头获取语言
///
/// 语言检测优先级：
/// 1. HTTP Accept-Language Header
/// 2. 环境变量 DEFAULT_LOCALE
/// 3. 硬编码默认值 "en-US"
///
/// # Arguments
/// * `headers` - HTTP 请求头
///
/// # Returns
/// 语言代码，如 "zh-CN", "en-US"
pub fn get_locale_from_headers(headers: &HeaderMap) -> &'static str {
    // 1. 首先尝试从 HTTP Header 获取
    if let Some(header) = headers.get("Accept-Language").and_then(|v| v.to_str().ok()) {
        // parse_accept_language 返回的总是支持的语言或默认值
        return parse_accept_language(Some(header));
    }

    // 2. 尝试从环境变量获取（已缓存）
    get_default_locale_from_env()
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
        // 无 Header 且无环境变量时，应返回默认值 en-US
        assert_eq!(get_locale_from_headers(&headers), "en-US");
    }

    #[test]
    fn test_get_locale_from_headers_unsupported() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept-language"),
            "fr-FR".parse().unwrap(),
        );
        // 不支持的语言应返回默认值
        assert_eq!(get_locale_from_headers(&headers), "en-US");
    }
}
