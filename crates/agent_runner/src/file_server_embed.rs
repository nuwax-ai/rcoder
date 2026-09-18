//! agent_runner 内嵌 file-server（阶段三：路由合并 + 前置代理，与 rcoder 主 pod 同构）。
//!
//! 容器内形态：
//!
//! ```text
//! 外部(rcoder userapp_forward / Java) → :60000 file-server-proxy
//!   └─ policy（FILE_SERVER_PROXY_POLICY env）→ 127.0.0.1:{agent_runner HTTP 端口}
//! agent_runner HTTP (8086) = 自身路由(/chat /computer/* /health …) + file-server 路由
//!   (/api/project /api/computer /api/git /api/build /api/version …，两族路径零冲突)
//! ```
//!
//! 与独立 listener 方案（内嵌 file-server 单独绑 60002）相比：少一个 listener 与
//! 端口配置；与 rcoder 主 pod（8086 merge + 60000 分流代理）架构完全对称。
//! 60000 proxy 策略由 env `FILE_SERVER_PROXY_POLICY` 控制（对齐主 pod
//! config.yml `file_server_proxy.policy`）：`all_rust`（默认）/ `ts_first` /
//! `all_ts`。容器内 TS nuwax-file-server（60001）由 start-up.sh 拉起热备。
//!
//! agent_runner 是 per-agent 容器,workspace 在本地 (`/home/user`),直接用
//! file-server 默认的 `LocalWorkspaceResolver`,不需要 Subvolume / cephfs 聚合解析。
//!
//! env 开关 `RCODER_EMBED_FILE_SERVER=true|1` 启用（路由 merge + proxy 拉起）;
//! 配套 start-up.sh 须设 `PROJECT_SOURCE_DIR=/home/user` 等覆盖 file-server 的
//! `/app/...` 默认路径。任何阶段失败只 `warn!`,不阻断 agent_runner 启动。
//!
//! 参照 `crates/rcoder/src/file_server_embed.rs`(rcoder 版多一层 SubvolumeWorkspaceResolver,
//! 此处省略)。

use axum::Router;
use file_server::{Config, FileServer};
use file_server_proxy::{FileServerProxyConfig, RoutePolicy};
use tracing::{info, warn};

use file_server_proxy::{AGENT_FILE_SERVER_PORT, NUWAX_FILE_SERVER_INTERNAL_PORT};

/// 构造合并进 agent_runner 主 Router 的 file-server 路由（无独立 listener/端口）。
///
/// 用 **container 路由集**（`file_server_userapp::container_router`：全量业务路由
/// 含 `/api/v1/userapp`——开发容器是 userApp 域本地实现的宿主，rcoder 转发层的上游；
/// userApp 域已拆至 file-server-userapp crate，组装经其完成。曾误用
/// `router_base`（排除 userapp 的 rcoder 主进程集）导致容器内 /api/v1/userapp/*
/// 全 404）。返回 `Err` 时主服务照常启动（缺路由不致命，warn 可见）——
/// 与 rcoder 主服务的降级语义同款。
pub fn merged_router() -> Result<Router, String> {
    let fs_config = Config::load().map_err(|e| format!("load file-server config: {e}"))?;
    let fs_server = FileServer::builder(fs_config)
        .build()
        .map_err(|e| format!("build merged file-server: {e}"))?;
    file_server_userapp::container_router(&fs_server)
        .map_err(|e| format!("build merged file-server router: {e}"))
}

/// 启动 60000 前置分流代理（`FILE_SERVER_PROXY_POLICY` 控制路由策略）。
///
/// 上游即本进程的 8086（file-server 路由已 merge），不再有独立内嵌 listener。
/// `rust_upstream_port`: agent_runner HTTP 端口（main 的 `config.port`）。
/// 失败只 `warn!` 不阻断（外部经 60000 的请求会 502，8086 直连路径不受影响）。
pub async fn spawn_file_server_proxy(rust_upstream_port: u16) {
    let listen_port = env_port("FILE_SERVER_PORT", AGENT_FILE_SERVER_PORT);
    let policy = resolve_policy(std::env::var("FILE_SERVER_PROXY_POLICY").ok().as_deref());
    file_server_proxy::init(FileServerProxyConfig {
        listen_host: "0.0.0.0".to_string(),
        // N07：容器形态令牌可选注入（沙箱网络边界内；启用后入口即认证）
        auth_token: std::env::var("FILE_SERVER_PROXY_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        listen_port,
        rust_upstream_port,
        ts_upstream_port: NUWAX_FILE_SERVER_INTERNAL_PORT,
        policy,
        // 沙箱容器无多副本预览协调需求（K8s 共享部署才启用）；env 透传
        // 供未来沙箱形态统一配置，缺省 false 保持历史分流行为。
        coordinated_dev_lifecycle: std::env::var("FILE_SERVER_COORDINATED_DEV_LIFECYCLE")
            .ok()
            .is_some_and(|v| v == "true" || v == "1"),
    });
    match file_server_proxy::try_start().await {
        Ok(address) => {
            info!(
                "file-server 前置代理启动: {address} → 127.0.0.1:{rust_upstream_port} ({})",
                policy.as_str()
            )
        }
        Err(e) => warn!("file-server-proxy (container form) start failed: {e}"),
    }
}

/// 读 u16 端口 env（非法值回落默认并留痕）。
fn env_port(name: &str, default: u16) -> u16 {
    match std::env::var(name) {
        Ok(v) => v.parse().unwrap_or_else(|_| {
            warn!("invalid {name}={v}, fallback to {default}");
            default
        }),
        Err(_) => default,
    }
}

/// 解析路由策略 env 值（纯函数，可测）。
///
/// 缺省或非法值均回落 `AllRust`（锚定容器现行为，零变化）。
/// 非 `all_rust` 档 `info!` 留痕——切档操作可见。
fn resolve_policy(env_value: Option<&str>) -> RoutePolicy {
    let Some(raw) = env_value else {
        return RoutePolicy::AllRust;
    };
    match file_server_proxy::parse_route_policy(raw) {
        Ok(policy) => {
            if policy != RoutePolicy::AllRust {
                info!("file-server-proxy 策略已切换: {raw} (非默认 all_rust)");
            }
            policy
        }
        Err(msg) => {
            warn!("{msg}, fallback to all_rust");
            RoutePolicy::AllRust
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_policy_default() {
        // 缺省 = AllRust（容器现行为零变化）
        assert_eq!(resolve_policy(None), RoutePolicy::AllRust);
    }

    #[test]
    fn resolve_policy_three_wires() {
        assert_eq!(resolve_policy(Some("all_rust")), RoutePolicy::AllRust);
        assert_eq!(resolve_policy(Some("ts_first")), RoutePolicy::TsFirst);
        assert_eq!(resolve_policy(Some("all_ts")), RoutePolicy::AllTs);
    }

    #[test]
    fn resolve_policy_illegal_fallback() {
        assert_eq!(resolve_policy(Some("bogus")), RoutePolicy::AllRust);
        assert_eq!(resolve_policy(Some("")), RoutePolicy::AllRust);
    }

    #[test]
    fn resolve_policy_wrong_case_fallback() {
        // wire 值是小写 snake_case；错误大小写回落 AllRust（parse_route_policy Err）
        assert_eq!(resolve_policy(Some("AllRust")), RoutePolicy::AllRust);
    }
}
