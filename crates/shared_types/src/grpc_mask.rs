// gRPC URL 脱敏工具模块

/// 对域名进行脱敏处理
///
/// # 规则
/// - 保留前 3 个字符和后 6 个字符（包括顶级域名）
/// - 中间部分用 `***` 替代
///
/// # 示例
/// - `anthropic-code-api.nuwax.com` -> `ant***ax.com`
/// - `api.openai.com` -> `api***ai.com`
/// - `localhost` -> `localhost` (短域名不脱敏)
fn mask_domain(domain: &str) -> String {
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

/// 对 URL 进行脱敏处理，对域名进行脱敏但保留完整路径
///
/// # 示例
/// - `https://test-code-api.xspaceagi.com/api/anthropic/session-xxx` -> `https://tes***gi.com/api/anthropic/session-xxx`
/// - `https://api.openai.com/v1/chat/completions` -> `https://api***ai.com/v1/chat/completions`
pub fn mask_url(url: &str) -> String {
    // 尝试解析 URL
    if let Ok(parsed_url) = url::Url::parse(url)
        && let Some(host) = parsed_url.host_str() {
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

    // 如果解析失败，返回脱敏提示
    "***".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_domain_long() {
        let result = mask_domain("test-code-api.xspaceagi.com");
        assert_eq!(result, "tes***gi.com");
    }

    #[test]
    fn test_mask_domain_short() {
        let result = mask_domain("api.openai.com");
        assert_eq!(result, "api***ai.com");
    }

    #[test]
    fn test_mask_domain_very_short() {
        let result = mask_domain("localhost");
        assert_eq!(result, "localhost");
    }

    #[test]
    fn test_mask_url_with_path() {
        let result = mask_url("https://test-code-api.xspaceagi.com/api/anthropic/session-xxx");
        assert!(result.contains("tes***gi.com"));
        assert!(result.contains("/api/anthropic/session-xxx"));
    }

    #[test]
    fn test_mask_url_simple() {
        let result = mask_url("https://api.openai.com/v1/chat/completions");
        assert!(result.contains("api***ai.com"));
        assert!(result.contains("/v1/chat/completions"));
    }
}
