//! URL 规范化:把 provider.base_url 转为 genai adapter 期望的 endpoint 格式。
//!
//! 不同协议的 genai adapter 追加路径不同:
//! - Anthropic adapter: `format!("{base_url}messages")` —— 需 endpoint 以 `/v1/` 结尾
//! - OpenAI adapter: `Url::parse(base).join("chat/completions")` —— 需 trailing `/`(否则替换末段)

use shared_types::ModelApiProtocol;

/// 规范化 endpoint,按协议选择不同策略。
///
/// **Anthropic**:base 通常不含 `/v1`(SDK 追加 `/v1/messages`)→ 补 `/v1/`;
/// 已含 `/v1` 则只补尾部 `/`。
///
/// **OpenAI**:base 可能含或不含 `/v1`(如 `https://api.deepseek.com` 或 `.../v1`)→ 只补尾部 `/`。
/// genai 用 `Url::join("chat/completions")` 或 `Url::join("responses")`,没有 trailing `/` 会替换掉末段。
pub(crate) fn normalize_endpoint(base_url: &str, protocol: ModelApiProtocol) -> String {
    let base = base_url.trim_end_matches('/');
    match protocol {
        ModelApiProtocol::Anthropic => {
            if base.ends_with("/v1") {
                format!("{base}/")
            } else {
                format!("{base}/v1/")
            }
        }
        ModelApiProtocol::OpenAI => format!("{base}/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Anthropic 协议 ---

    #[test]
    fn test_normalize_anthropic_standard() {
        assert_eq!(
            normalize_endpoint("https://api.anthropic.com", ModelApiProtocol::Anthropic),
            "https://api.anthropic.com/v1/"
        );
    }

    #[test]
    fn test_normalize_anthropic_zhipu_glm() {
        assert_eq!(
            normalize_endpoint(
                "https://open.bigmodel.cn/api/anthropic",
                ModelApiProtocol::Anthropic
            ),
            "https://open.bigmodel.cn/api/anthropic/v1/"
        );
    }

    #[test]
    fn test_normalize_anthropic_already_has_v1() {
        assert_eq!(
            normalize_endpoint("https://some-proxy.com/v1", ModelApiProtocol::Anthropic),
            "https://some-proxy.com/v1/"
        );
    }

    #[test]
    fn test_normalize_anthropic_trailing_slash() {
        assert_eq!(
            normalize_endpoint("https://api.anthropic.com/", ModelApiProtocol::Anthropic),
            "https://api.anthropic.com/v1/"
        );
    }

    // --- OpenAI 协议 ---

    #[test]
    fn test_normalize_openai_deepseek() {
        // base 已含 /v1 → 只补 trailing /
        assert_eq!(
            normalize_endpoint("https://api.deepseek.com/v1", ModelApiProtocol::OpenAI),
            "https://api.deepseek.com/v1/"
        );
    }

    #[test]
    fn test_normalize_openai_standard() {
        assert_eq!(
            normalize_endpoint("https://api.openai.com/v1", ModelApiProtocol::OpenAI),
            "https://api.openai.com/v1/"
        );
    }

    #[test]
    fn test_normalize_openai_no_v1() {
        // base 不含 /v1(某些自定义代理)→ 只补 trailing /
        assert_eq!(
            normalize_endpoint(
                "http://47.109.194.91:18086/api/proxy/model",
                ModelApiProtocol::OpenAI
            ),
            "http://47.109.194.91:18086/api/proxy/model/"
        );
    }

    #[test]
    fn test_normalize_openai_trailing_slash() {
        assert_eq!(
            normalize_endpoint("https://api.deepseek.com/v1/", ModelApiProtocol::OpenAI),
            "https://api.deepseek.com/v1/"
        );
    }
}
