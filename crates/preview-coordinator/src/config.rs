//! 协调器运行配置（rcoder config.yml `preview_coordinator:` 段数据模型）。
use serde::{Deserialize, Serialize};

fn default_heartbeat_interval() -> u64 {
    30
}
fn default_heartbeat_ttl() -> u64 {
    90
}
fn default_route_positive_ttl() -> u64 {
    10
}
fn default_route_negative_ttl() -> u64 {
    5
}
fn default_activity_flush_interval() -> u64 {
    30
}
fn default_start_budget() -> u64 {
    600
}
fn default_remote_verify_timeout() -> u64 {
    5
}
fn default_remote_dispatch_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    /// 总开关（false=不装配协调器，60000 不改路，全部现状行为）。
    #[serde(default)]
    pub enabled: bool,
    /// 内部令牌的 env 名（令牌值不落配置文件；缺失/空且 enabled → 装配 fail-fast）。
    #[serde(default = "default_token_env")]
    pub internal_token_env: String,
    /// 对等副本主 API 端口（跨 Pod 派发 stop/verify/log 用；rcoder 装配时注入实际端口）。
    pub peer_api_port: u16,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    /// 心跳新鲜窗口：窗口内 keep-alive 不回环探测，直接信任宿主心跳。
    #[serde(default = "default_heartbeat_ttl")]
    pub heartbeat_ttl_secs: u64,
    /// 空闲回收（仅看 last_activity_at；0=关闭，默认关闭保持现状语义）。
    /// 判定阈值含安全余量 = idle_secs + 2×activity_flush_interval（消除刷盘窗口竞态）。
    #[serde(default)]
    pub idle_recycle_secs: u64,
    #[serde(default = "default_route_positive_ttl")]
    pub route_cache_positive_secs: u64,
    #[serde(default = "default_route_negative_ttl")]
    pub route_cache_negative_secs: u64,
    #[serde(default = "default_activity_flush_interval")]
    pub activity_flush_interval_secs: u64,
    /// 启动执行预算（对齐 TS start-dev 10 分钟超时）。
    #[serde(default = "default_start_budget")]
    pub start_budget_secs: u64,
    #[serde(default = "default_remote_verify_timeout")]
    pub remote_verify_timeout_secs: u64,
    #[serde(default = "default_remote_dispatch_timeout")]
    pub remote_dispatch_timeout_secs: u64,
}

fn default_token_env() -> String {
    "RCODER_PREVIEW_INTERNAL_TOKEN".to_string()
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            internal_token_env: default_token_env(),
            peer_api_port: 8086,
            heartbeat_interval_secs: default_heartbeat_interval(),
            heartbeat_ttl_secs: default_heartbeat_ttl(),
            idle_recycle_secs: 0,
            route_cache_positive_secs: default_route_positive_ttl(),
            route_cache_negative_secs: default_route_negative_ttl(),
            activity_flush_interval_secs: default_activity_flush_interval(),
            start_budget_secs: default_start_budget(),
            remote_verify_timeout_secs: default_remote_verify_timeout(),
            remote_dispatch_timeout_secs: default_remote_dispatch_timeout(),
        }
    }
}
