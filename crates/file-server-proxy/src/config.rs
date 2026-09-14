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
    /// dev 生命周期路径（start/stop/restart/keep-alive 等 7 端点）在**所有策略**下
    /// 导向 Rust 上游（Custom Page 预览协调收口；与 rcoder `preview_coordinator.enabled`
    /// 同源配置渲染）。默认 false=分流行为与历史完全一致。
    #[serde(default)]
    pub coordinated_dev_lifecycle: bool,
}

impl Default for FileServerProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: AGENT_FILE_SERVER_PORT,
            rust_upstream_port: 8086,
            ts_upstream_port: NUWAX_FILE_SERVER_INTERNAL_PORT,
            policy: RoutePolicy::default(),
            coordinated_dev_lifecycle: false,
        }
    }
}

/// 路由策略——同一 crate 服务多种部署形态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePolicy {
    /// TS 优先模式（存量切流档，测试/生产在跑）：userApp 判据（`/api/v1/userapp*`
    /// 路径前缀 **或** `x-service-type: userapp` header）→ rust 上游（rcoder
    /// 拦截层转发 per-app 容器——per-app RBD 架构下只有容器读得到 app 工作区）；
    /// 其余（无 userApp 标记的存量流量）→ TS nuwax-file-server。
    ///
    /// 历史：曾为「仅路径判据、header 失效」的过渡档（假设 TS 以 service_type
    /// 入参自载 userApp 业务）——共享卷时代 TS 可达 app 卷，per-app RBD 后 TS
    /// 物理不可达，该假设失效，已并入 header 判据（原 userapp_split 档删除）。
    #[default]
    TsFirst,
    /// 全 Rust 模式（切流终态）：一律 rust 上游，全部流量由 Rust 重写的
    /// file-server 承载（TS 热备于 [`NUWAX_FILE_SERVER_INTERNAL_PORT`]，不接流量）
    AllRust,
    /// 全 TS 模式（npm 独立形态的回退/AB 对照档）：一律 ts 上游。
    /// 无路径白名单（TS 本就是全量老路由面，白名单语义不适用）；
    /// TS 没有的 userApp 新接口（/api/v1/userapp/*）在此模式下由 TS 返回 404。
    AllTs,
}

impl RoutePolicy {
    /// 策略的 wire 值（serde/CLI/env/helm 共用词汇表）。
    pub const fn as_str(self) -> &'static str {
        match self {
            RoutePolicy::TsFirst => "ts_first",
            RoutePolicy::AllRust => "all_rust",
            RoutePolicy::AllTs => "all_ts",
        }
    }
}

/// 解析策略值（env/CLI 入口共用；serde 之外的运行时入口）。
///
/// 受认可值与 serde wire 契约一致：`ts_first|all_rust|all_ts`。
/// 非法值返回 Err（带受认可值清单，调用方 exit 前可直接展示）。
pub fn parse_route_policy(value: &str) -> Result<RoutePolicy, String> {
    match value.trim() {
        "ts_first" => Ok(RoutePolicy::TsFirst),
        "all_rust" => Ok(RoutePolicy::AllRust),
        "all_ts" => Ok(RoutePolicy::AllTs),
        other => Err(format!(
            "invalid route policy {other:?}: expected one of ts_first | all_rust | all_ts"
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

/// dev 生命周期精确端点集合（`/api/build/*` 下由预览协调器收口的子集；
/// build/parse 等其余 build 端点不受影响）。
pub const COORDINATED_DEV_PATHS: [&str; 7] = [
    "/api/build/start-dev",
    "/api/build/stop-dev",
    "/api/build/restart-dev",
    "/api/build/keep-alive",
    "/api/build/list-dev",
    "/api/build/port-pool-status",
    "/api/build/get-dev-log",
];

/// dev 生命周期端点判定（容忍尾斜杠；非前缀匹配——`/api/build/start-dev-2` 不命中）。
pub fn is_coordinated_dev_path(path: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    COORDINATED_DEV_PATHS.contains(&trimmed)
}

impl FileServerProxyConfig {
    /// 分流规则纯函数（按 [`RoutePolicy`] 分派）：
    /// - [`RoutePolicy::TsFirst`]：`/api/v1/userapp*` 前缀或
    ///   `x-service-type: userapp` header（任一命中）→ Rust 上游，其余 → TS 上游
    /// - [`RoutePolicy::AllRust`]：一律 Rust 上游
    /// - [`RoutePolicy::AllTs`]：一律 TS 上游
    ///
    /// 例外（优先于策略）：`coordinated_dev_lifecycle` 开启时，dev 生命周期 7 端点
    /// 在**所有策略**下导向 Rust 上游——TS 无身份协调（多副本下误重建/误停），
    /// 这些端点必须由 Rust 协调器受理。
    pub fn upstream_port_for(&self, path: &str, service_type_header: Option<&str>) -> Upstream {
        if self.coordinated_dev_lifecycle && is_coordinated_dev_path(path) {
            return Upstream::Rust(self.rust_upstream_port);
        }
        let to_rust = match self.policy {
            RoutePolicy::TsFirst => {
                is_userapp_path(path) || is_userapp_service_type(service_type_header)
            }
            RoutePolicy::AllRust => true,
            RoutePolicy::AllTs => false,
        };
        if to_rust {
            Upstream::Rust(self.rust_upstream_port)
        } else {
            Upstream::Ts(self.ts_upstream_port)
        }
    }
}

#[cfg(test)]
mod coordinated_dev_tests {
    use super::*;

    fn config(policy: RoutePolicy, coordinated: bool) -> FileServerProxyConfig {
        FileServerProxyConfig {
            policy,
            coordinated_dev_lifecycle: coordinated,
            ..FileServerProxyConfig::default()
        }
    }

    #[test]
    fn dev_lifecycle_paths_go_rust_in_all_policies_when_enabled() {
        for policy in [
            RoutePolicy::TsFirst,
            RoutePolicy::AllRust,
            RoutePolicy::AllTs,
        ] {
            let cfg = config(policy, true);
            for path in COORDINATED_DEV_PATHS {
                assert!(
                    matches!(cfg.upstream_port_for(path, None), Upstream::Rust(_)),
                    "{policy:?} {path} must route to rust"
                );
                // 尾斜杠容忍
                assert!(
                    matches!(
                        cfg.upstream_port_for(&format!("{path}/"), None),
                        Upstream::Rust(_)
                    ),
                    "{policy:?} {path}/ must route to rust"
                );
            }
        }
    }

    #[test]
    fn other_paths_keep_policy_semantics_when_enabled() {
        // build 域其余端点不受影响
        let ts_first = config(RoutePolicy::TsFirst, true);
        assert!(matches!(
            ts_first.upstream_port_for("/api/build/build", None),
            Upstream::Ts(_)
        ));
        assert!(matches!(
            ts_first.upstream_port_for("/api/project/list", None),
            Upstream::Ts(_)
        ));
        assert!(matches!(
            ts_first.upstream_port_for("/api/v1/userapp/dev/list", None),
            Upstream::Rust(_)
        ));
        // 前缀不误命中
        assert!(matches!(
            ts_first.upstream_port_for("/api/build/start-dev-2", None),
            Upstream::Ts(_)
        ));
        // AllTs 下非 dev 端点仍全走 TS（回退通道语义保留）
        let all_ts = config(RoutePolicy::AllTs, true);
        assert!(matches!(
            all_ts.upstream_port_for("/api/build/build", None),
            Upstream::Ts(_)
        ));
    }

    #[test]
    fn disabled_flag_keeps_legacy_routing_exactly() {
        let ts_first = config(RoutePolicy::TsFirst, false);
        for path in COORDINATED_DEV_PATHS {
            assert!(
                matches!(ts_first.upstream_port_for(path, None), Upstream::Ts(_)),
                "{path} must stay on ts when coordination disabled"
            );
        }
    }
}
