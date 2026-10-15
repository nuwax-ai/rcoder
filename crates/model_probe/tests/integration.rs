//! 模型探活集成测试
//!
//! 默认 `#[ignore]` —— 不影响 CI / 正常 `cargo test`。
//! 手动运行(需要 .env.local 配置真实模型):
//!
//! ```bash
//! cargo test -p model_probe --test integration -- --ignored --nocapture
//! ```
//!
//! 配置方法:复制 `.env.local.example` 为 `.env.local`,填入真实 API key。

use std::collections::HashMap;
use std::path::PathBuf;

use model_probe::{ProbeResult, check_model_available};
use shared_types::ModelProviderConfig;

// ---------------------------------------------------------------------------
// .env.local 解析
// ---------------------------------------------------------------------------

/// 读取 crate 根目录下的 `.env.local`,解析为 KEY=VALUE HashMap。
fn load_env_local() -> HashMap<String, String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env.local");
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            ".env.local not found at {}. \
             Copy .env.local.example to .env.local and fill in real config.",
            path.display()
        )
    });

    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

/// 从 .env.local 构造 ModelProviderConfig。
///
/// prefix: "ANTHROPIC" 或 "OPENAI",对应 TEST_{prefix}_BASE_URL / _API_KEY / _MODEL。
fn load_provider(prefix: &str) -> ModelProviderConfig {
    let env = load_env_local();
    let get = |suffix: &str| -> String {
        let key = format!("TEST_{prefix}_{suffix}");
        env.get(&key)
            .unwrap_or_else(|| panic!("{key} not set in .env.local"))
            .clone()
    };

    ModelProviderConfig {
        id: format!("{prefix}-test"),
        name: prefix.to_lowercase(),
        base_url: get("BASE_URL"),
        api_key: get("API_KEY"),
        default_model: get("MODEL"),
        requires_openai_auth: prefix == "OPENAI",
        api_protocol: Some(
            if prefix == "OPENAI" {
                "openai"
            } else {
                "anthropic"
            }
            .to_string(),
        ),
        wire_api: env.get(&format!("TEST_{prefix}_WIRE_API")).cloned(),
    }
}

// ---------------------------------------------------------------------------
// 集成测试(#[ignore],手动触发)
// ---------------------------------------------------------------------------

/// Anthropic 协议真实模型 → 探活应返回 Available。
#[tokio::test]
#[ignore = "需要 .env.local 配置真实 Anthropic 模型"]
async fn probe_anthropic_model_available() {
    let provider = load_provider("ANTHROPIC");
    let result = check_model_available(&provider).await;
    println!("Anthropic probe result: {result:?}");
    assert_eq!(result, ProbeResult::Available);
}

/// 缓存验证:连续两次探活,第二次应命中缓存(无网络请求)。
#[tokio::test]
#[ignore = "需要 .env.local 配置真实 Anthropic 模型"]
async fn probe_cache_hit_on_second_call() {
    let provider = load_provider("ANTHROPIC");

    // 第一次:实际网络探活
    let start = std::time::Instant::now();
    let result1 = check_model_available(&provider).await;
    let elapsed1 = start.elapsed();

    // 模型临时不可用/限流时跳过缓存验证(集成测试可能并发触发限流)
    if result1 != ProbeResult::Available {
        eprintln!("First probe was {result1:?}, skipping cache test (model may be rate-limited)");
        return;
    }

    // 第二次:应命中 moka 缓存(几乎 0ms)
    let start = std::time::Instant::now();
    let result2 = check_model_available(&provider).await;
    let elapsed2 = start.elapsed();
    assert_eq!(result2, ProbeResult::Available);

    println!("First probe:  {elapsed1:?} (network call)");
    println!("Second probe: {elapsed2:?} (cache hit)");
    // 缓存命中应比首次快至少一个数量级
    assert!(
        elapsed2 < elapsed1 / 5,
        "cache hit should be much faster: first={elapsed1:?}, second={elapsed2:?}"
    );
}

/// OpenAI 协议模型探活。
///
/// 探活固定走 Anthropic adapter。如果端点也兼容 Anthropic 协议 → Available;
/// 如果只支持 OpenAI → 4xx → Inconclusive (fail-open,正确行为)。
#[tokio::test]
#[ignore = "需要 .env.local 配置真实 OpenAI 模型"]
async fn probe_openai_model() {
    let provider = load_provider("OPENAI");
    let result = check_model_available(&provider).await;
    println!("OpenAI probe result: {result:?}");
    // 不 assert 具体值 —— 结果取决于端点是否兼容 Anthropic 协议
    // 只验证不 panic、不 hang、在合理时间内返回
    assert!(
        matches!(result, ProbeResult::Available | ProbeResult::Inconclusive),
        "OpenAI endpoint via Anthropic adapter should be Available or Inconclusive, got: {result:?}"
    );
}

/// 不可达端点 → 应返回 Unavailable(连接拒绝,非超时)。
#[tokio::test]
#[ignore = "验证不可达端点的拦截行为(连接到端口 1)"]
async fn probe_unreachable_endpoint_unavailable() {
    let provider = ModelProviderConfig {
        id: "unreachable".to_string(),
        name: "test".to_string(),
        base_url: "http://127.0.0.1:1".to_string(), // 端口 1 — 保证不可达
        api_key: "test".to_string(),
        requires_openai_auth: false,
        default_model: "test".to_string(),
        api_protocol: None,
        wire_api: None,
    };
    let result = check_model_available(&provider).await;
    println!("Unreachable probe result: {result:?}");
    assert!(
        matches!(result, ProbeResult::Unavailable(_)),
        "unreachable endpoint should be Unavailable, got: {result:?}"
    );
}

/// 空配置 → 跳过探活(fail-open),返回 Inconclusive。
/// 不需要 .env.local,不需要网络。
#[tokio::test]
async fn probe_empty_config_skip() {
    let provider = ModelProviderConfig {
        id: "empty".to_string(),
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
