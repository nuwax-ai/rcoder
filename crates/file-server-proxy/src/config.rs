//! 分流代理配置与路由策略（纯函数域，无 IO）。

use serde::{Deserialize, Serialize};

pub use shared_types::{SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP};

/// userApp 业务路由前缀（header 未接入期的兜底判据）。
pub const USERAPP_PATH_PREFIX: &str = "/api/v1/userapp";

/// 60000（对外入口）与 60001（TS 内部端口）的单一事实源见 shared_types。
pub use shared_types::{AGENT_FILE_SERVER_PORT, NUWAX_FILE_SERVER_INTERNAL_PORT};

/// 分流代理配置（config.yml 顶层 `file_server_proxy:` 段 / agent_runner env 构造）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileServerProxyConfig {
    /// 对外监听端口（Java/外部入口；K8s NodePort 30779 → 此端口）
    pub listen_port: u16,
    /// rcoder 主服务端口（userApp 业务上游；容器形态=内嵌 Rust file-server 端口）
    pub rust_upstream_port: u16,
    /// TS nuwax-file-server 内部端口（存量域上游；容器形态为复用面预留，未使用）
    pub ts_upstream_port: u16,
    /// 路由策略（两种部署形态）
    #[serde(default)]
    pub policy: RoutePolicy,
}

impl Default for FileServerProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: AGENT_FILE_SERVER_PORT,
            rust_upstream_port: 8086,
            ts_upstream_port: NUWAX_FILE_SERVER_INTERNAL_PORT,
            policy: RoutePolicy::default(),
        }
    }
}

/// 路由策略——同一 crate 服务多种部署形态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePolicy {
    /// 分流模式（现状兼容，生产在跑）：userApp 判据（path 前缀或
    /// `x-service-type` header）→ rust 上游，其余 → ts 上游
    /// （存量域继续 TS nuwax-file-server；TS 尚未支持 service_type 入参时
    /// 存量路径上的 userApp 业务必须由 Rust 承载）
    #[default]
    UserappSplit,
    /// 全 Rust 模式（切流终态）：一律 rust 上游，全部流量由 Rust 重写的
    /// file-server 承载（TS 热备于 [`NUWAX_FILE_SERVER_INTERNAL_PORT`]，不接流量）
    AllRust,
    /// 全 TS 模式（npm 独立形态的回退/AB 对照档）：一律 ts 上游。
    /// 无路径白名单（TS 本就是全量老路由面，白名单语义不适用）；
    /// TS 没有的 userApp 新接口（/api/v1/userapp/*）在此模式下由 TS 返回 404。
    AllTs,
    /// TS 优先模式（过渡切流档）：**仅** Rust 独有接口（`/api/v1/userapp*`，TS 无
    /// 此路由）→ rust 上游；存量同名接口**全走 TS**（含带 `x-service-type`
    /// 标记的请求——header 判据在此模式下失效，由 TS 以 service_type 入参
    /// 内部消费 userApp 业务）。验证 TS 侧 userApp 能力就绪后的整体切流形态。
    TsFirst,
}

impl RoutePolicy {
    /// 策略的 wire 值（serde/CLI/env/helm 共用词汇表）。
    pub const fn as_str(self) -> &'static str {
        match self {
            RoutePolicy::UserappSplit => "userapp_split",
            RoutePolicy::AllRust => "all_rust",
            RoutePolicy::AllTs => "all_ts",
            RoutePolicy::TsFirst => "ts_first",
        }
    }
}

/// 解析策略值（env/CLI 入口共用；serde 之外的运行时入口）。
///
/// 受认可值与 serde wire 契约一致：`userapp_split|all_rust|all_ts|ts_first`。
/// 非法值返回 Err（带受认可值清单，调用方 exit 前可直接展示）。
pub fn parse_route_policy(value: &str) -> Result<RoutePolicy, String> {
    match value.trim() {
        "userapp_split" => Ok(RoutePolicy::UserappSplit),
        "all_rust" => Ok(RoutePolicy::AllRust),
        "all_ts" => Ok(RoutePolicy::AllTs),
        "ts_first" => Ok(RoutePolicy::TsFirst),
        other => Err(format!(
            "invalid route policy {other:?}: expected one of \
             userapp_split | all_rust | all_ts | ts_first"
        )),
    }
}

/// 业务域分流的选中上游。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// rcoder 主服务（userApp 业务）
    Rust(u16),
    /// TS nuwax-file-server（存量域）
    Ts(u16),
}

/// userApp 路径前缀的段边界形式（`/api/v1/userapplication` 不误命中）。
const USERAPP_PATH_PREFIX_SLASH: &str = "/api/v1/userapp/";

/// userApp 业务判定的 path 判据：`/api/v1/userapp` 精确或 `/api/v1/userapp/*`。
/// TS 无此路由（走 TS 也 404），按前缀分流零歧义——Java 同事加
/// `x-service-type` header 前的兜底判据，header 是未来的正名路径。
fn is_userapp_path(path: &str) -> bool {
    path == USERAPP_PATH_PREFIX || path.starts_with(USERAPP_PATH_PREFIX_SLASH)
}

/// userApp 业务判定的 header 判据：`x-service-type` 值为 `userapp`
/// （大小写不敏感 + 前后空白容忍，单一事实源
/// [`shared_types::is_userapp_service_type_value`]）。
fn is_userapp_service_type(header_value: Option<&str>) -> bool {
    header_value.is_some_and(shared_types::is_userapp_service_type_value)
}

impl FileServerProxyConfig {
    /// 分流规则纯函数（按 [`RoutePolicy`] 分派）：
    /// - [`RoutePolicy::UserappSplit`]：`/api/v1/userapp*` 前缀或
    ///   `x-service-type: userapp` header（任一命中）→ Rust 上游，其余 → TS 上游
    /// - [`RoutePolicy::AllRust`]：一律 Rust 上游
    /// - [`RoutePolicy::AllTs`]：一律 TS 上游
    /// - [`RoutePolicy::TsFirst`]：仅 `/api/v1/userapp*` → Rust 上游（header 判据
    ///   失效，存量同名接口含 userApp 标记一律 TS）
    pub fn upstream_port_for(&self, path: &str, service_type_header: Option<&str>) -> Upstream {
        let to_rust = match self.policy {
            RoutePolicy::UserappSplit => {
                is_userapp_path(path) || is_userapp_service_type(service_type_header)
            }
            RoutePolicy::AllRust => true,
            RoutePolicy::AllTs => false,
            RoutePolicy::TsFirst => is_userapp_path(path),
        };
        if to_rust {
            Upstream::Rust(self.rust_upstream_port)
        } else {
            Upstream::Ts(self.ts_upstream_port)
        }
    }
}
