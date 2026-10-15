//! 内部令牌获取（env 单一来源；装配 fail-fast 在 rcoder 侧）。

/// 从 env 读内部令牌（`internal_token_env` 配置名，缺省 RCODER_PREVIEW_INTERNAL_TOKEN）。
pub fn internal_token_from_env_named(env_name: &str) -> Option<String> {
    std::env::var(env_name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 缺省 env 名读取（内部端点 guard 用；与协调器配置同源约定）。
pub fn internal_token_from_env() -> Option<String> {
    internal_token_from_env_named("RCODER_PREVIEW_INTERNAL_TOKEN")
}
