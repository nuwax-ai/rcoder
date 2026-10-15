//! 探活编排:缓存查询 + genai 探活请求 + 结果分级。
//!
//! 对外暴露 [`check_model_available`]（缓存感知主入口）。

use std::sync::LazyLock;
use std::time::Duration;

use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest};
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ServiceTarget};
use moka::sync::Cache;
use shared_types::{ModelApiProtocol, ModelProviderConfig};
use tracing::{debug, warn};

use super::classify::classify;
use super::endpoint::normalize_endpoint;
use crate::ProbeResult;

/// 探活整体超时(5s;超过即 fail-open 放行)
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 探活缓存 TTL(10 分钟;过期后下次新建会话再探一次)
const PROBE_CACHE_TTL: Duration = Duration::from_secs(600);

/// 全局探活缓存
///
/// key = `{normalized_endpoint}|{model}|{adapter}`,value = `()`(仅标记"已验证")。
/// moka 自动按 TTL 过期;只有 `Available` 才写缓存(不可用不写,保证下次还能立刻看到错误)。
static MODEL_PROBE_CACHE: LazyLock<Cache<String, ()>> = LazyLock::new(|| {
    Cache::builder()
        .time_to_live(PROBE_CACHE_TTL)
        .max_capacity(500) // 防御性上限:不同 endpoint+model+adapter 组合极少超 500
        .build()
});

/// 缓存感知的模型可用性检查(对外主入口)。
///
/// 流程:查缓存(TTL 命中→跳过)→ 未命中调 `probe_once` → Available 写缓存。
///
/// 以下情况直接 fail-open(不探活):
/// - base_url / default_model 为空(无端点可探)
/// - `api_protocol` 未显式指定(协议不确定 —— `get_api_protocol()` 默认 Anthropic,
///   但 proxy 可能按 `requires_openai_auth` 走 OpenAI;探测错误协议可能 5xx → 误拦截)
pub async fn check_model_available(provider: &ModelProviderConfig) -> ProbeResult {
    // 无端点可探 → 跳过(fail-open)
    if provider.base_url.trim().is_empty() || provider.default_model.trim().is_empty() {
        debug!("[MODEL_PROBE] empty base_url or model, skipping probe (fail-open)");
        return ProbeResult::Inconclusive;
    }

    // api_protocol 未显式指定 → 协议不确定,不猜测,直接 fail-open
    if provider.api_protocol.is_none() {
        debug!("[MODEL_PROBE] api_protocol not specified, skipping probe (fail-open)");
        return ProbeResult::Inconclusive;
    }

    let adapter = build_adapter(provider);
    let protocol = provider.get_api_protocol();
    let endpoint = normalize_endpoint(&provider.base_url, protocol);
    let cache_key = format!("{endpoint}|{}|{adapter}", provider.default_model);

    // 查缓存:moka 按 TTL 自动过期,contains_key 返回 false 表示已过期或从未写入
    if MODEL_PROBE_CACHE.contains_key(&cache_key) {
        debug!(
            "[MODEL_PROBE] cache hit (TTL alive): endpoint={}, model={}",
            endpoint, provider.default_model
        );
        return ProbeResult::Available;
    }

    // 未命中 / 过期 → 实际探活
    let result = probe_once(&endpoint, provider, adapter).await;

    // 只有"可用"才写缓存
    if matches!(result, ProbeResult::Available) {
        MODEL_PROBE_CACHE.insert(cache_key, ());
        debug!(
            "[MODEL_PROBE] probe passed, cached for {:?}: endpoint={}",
            PROBE_CACHE_TTL, endpoint
        );
    }

    result
}

/// 发一次 max_tokens:1 的探活请求,用配置指定的 adapter(strict,不做 fallback)。
async fn probe_once(
    endpoint: &str,
    provider: &ModelProviderConfig,
    adapter: AdapterKind,
) -> ProbeResult {
    // 构建带短超时的 reqwest client(fail-open 阈值)
    let reqwest_client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            warn!("[MODEL_PROBE] failed to build reqwest client: {e}");
            return ProbeResult::Inconclusive; // 拿不准 → fail-open
        }
    };
    let client = Client::builder().with_reqwest(reqwest_client).build();

    let target = ServiceTarget {
        endpoint: Endpoint::from_owned(endpoint.to_string()),
        auth: AuthData::from_single(provider.api_key.clone()),
        model: ModelIden::new(adapter, provider.default_model.as_str()),
    };

    let req = ChatRequest::new(vec![ChatMessage::user("hi")]);
    let options = ChatOptions::default().with_max_tokens(1);

    let result = client.exec_chat(target, req, Some(&options)).await;

    classify(result)
}

/// 根据 api_protocol + wire_api 选择 genai adapter。
///
/// - `Anthropic` → Anthropic adapter(POST /v1/messages)
/// - `OpenAI` + `wire_api=chat` → OpenAI adapter(POST /chat/completions)
/// - `OpenAI` + `wire_api=response`(或 None,默认)→ OpenAIResp adapter(POST /responses)
fn build_adapter(provider: &ModelProviderConfig) -> AdapterKind {
    match provider.get_api_protocol() {
        ModelApiProtocol::Anthropic => AdapterKind::Anthropic,
        ModelApiProtocol::OpenAI => match provider.wire_api.as_deref() {
            Some("chat") => AdapterKind::OpenAI,
            _ => AdapterKind::OpenAIResp, // "response"/"responses" 或 None → Responses API(默认)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试用 ModelProviderConfig(非空 base_url/model)。
    fn make_provider(api_protocol: Option<&str>, wire_api: Option<&str>) -> ModelProviderConfig {
        ModelProviderConfig {
            id: "test".to_string(),
            name: "test".to_string(),
            base_url: "https://example.com".to_string(),
            api_key: "key".to_string(),
            requires_openai_auth: false,
            default_model: "model".to_string(),
            api_protocol: api_protocol.map(String::from),
            wire_api: wire_api.map(String::from),
        }
    }

    // --- build_adapter 单测(协议 + wire_api → adapter 映射) ---

    #[test]
    fn test_build_adapter_anthropic() {
        let provider = make_provider(Some("anthropic"), None);
        assert_eq!(build_adapter(&provider), AdapterKind::Anthropic);
    }

    #[test]
    fn test_build_adapter_default_protocol_is_anthropic() {
        let provider = make_provider(None, None);
        assert_eq!(build_adapter(&provider), AdapterKind::Anthropic);
    }

    #[test]
    fn test_build_adapter_openai_default_is_responses() {
        let provider = make_provider(Some("openai"), None);
        assert_eq!(build_adapter(&provider), AdapterKind::OpenAIResp);
    }

    #[test]
    fn test_build_adapter_openai_explicit_chat() {
        let provider = make_provider(Some("openai"), Some("chat"));
        assert_eq!(build_adapter(&provider), AdapterKind::OpenAI);
    }

    #[test]
    fn test_build_adapter_openai_explicit_response() {
        let provider = make_provider(Some("openai"), Some("response"));
        assert_eq!(build_adapter(&provider), AdapterKind::OpenAIResp);
    }

    // --- check_model_available 边界 ---

    #[tokio::test]
    async fn test_empty_config_skip_probe() {
        let provider = ModelProviderConfig {
            id: "test".to_string(),
            name: "test".to_string(),
            base_url: String::new(),
            api_key: "key".to_string(),
            requires_openai_auth: false,
            default_model: String::new(),
            api_protocol: None,
            wire_api: None,
        };
        assert_eq!(
            check_model_available(&provider).await,
            ProbeResult::Inconclusive
        );
    }

    #[tokio::test]
    async fn test_no_api_protocol_skip_probe() {
        // api_protocol=None(未显式指定)→ 协议不确定,直接 fail-open
        // 即使 base_url 和 model 都有效,也不猜测协议
        let provider = make_provider(None, None);
        assert_eq!(
            check_model_available(&provider).await,
            ProbeResult::Inconclusive
        );
    }
}
