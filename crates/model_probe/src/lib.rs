//! Agent 启动前模型可用性预检(model_probe)
//!
//! 在 agent 第一次启动(新建会话)之前,主动做一次 `max_tokens:1` 轻量探活。
//! 模型明确不可用(5xx / 连接拒绝 / DNS / TLS 失败)时立即返回友好错误,
//! 不启动 agent、不让用户空等。采取 **fail-open** 策略 —— 拿不准就放行。
//!
//! ## 核心设计
//!
//! - **库**:genai 0.6.5(Anthropic/OpenAI 协议抽象),不手写协议分支
//! - **协议**:按 `api_protocol` + `wire_api` 选择 adapter
//!   (Anthropic → `/v1/messages`,OpenAI Responses → `/responses`,OpenAI Chat → `/chat/completions`)。
//!   严格使用配置指定的协议,不做 fallback —— 探活结果必须与 Agent 实际使用的协议一致。
//! - **URL**:按协议规范化 endpoint(见 `endpoint` 模块)
//! - **缓存**:moka TTL 缓存(10min),只有"可用"才写
//! - **fail-open**:只拦 5xx + 连接/DNS/TLS 失败;超时 / 429 / 401 / 403 全放行

mod classify;
mod endpoint;
mod probe;

/// 探活结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// 模型可用 —— 放行 + 写缓存
    Available,
    /// 模型明确不可用 —— 拦截,返回友好错误。String 为原因(用于日志)
    Unavailable(String),
    /// 拿不准 —— fail-open 放行(只 warn 日志)
    Inconclusive,
}

pub use probe::check_model_available;
