//! agent_runner 内嵌 file-server（阶段三：路由合并 + 前置代理，与 rcoder 主 pod 同构）。
//!
//! 容器内形态：
//!
//! ```text
//! 外部(rcoder userapp_forward / devapps 探活 / Java) → :60000 file-server-proxy
//!   └─ AllRust → 127.0.0.1:{agent_runner HTTP 端口}
//! agent_runner HTTP (8086) = 自身路由(/chat /computer/* /health …) + file-server 路由
//!   (/api/project /api/computer /api/git /api/build /api/version …，两族路径零冲突)
//! ```
//!
//! 与独立 listener 方案（内嵌 file-server 单独绑 60002）相比：少一个 listener 与
//! 端口配置；与 rcoder 主 pod（8086 merge + 60000 分流代理）架构完全对称。
//! 复用接口族（push-skills-to-workspace 等）最终走 Rust 还是 TS nuwax-file-server
//! 尚未拍板，60000 proxy 是未来的路由策略切换控制点（[`RoutePolicy`] 加值即可）。
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

/// 对外 file-server 入口端口（proxy 监听；外部契约固定 60000）。
const FILE_SERVER_PORT_DEFAULT: u16 = 60000;

/// 构造合并进 agent_runner 主 Router 的 file-server 路由（无独立 listener/端口）。
///
/// 返回 `Err` 时主服务照常启动（缺 file-server 路由不致命，warn 可见）——
/// 与 rcoder 主服务的 `merged_router` 同款降级语义。
pub fn merged_router() -> Result<Router, String> {
    let fs_config = Config::load().map_err(|e| format!("load file-server config: {e:#}"))?;
    let fs_server = FileServer::builder(fs_config)
        .build()
        .map_err(|e| format!("build merged file-server: {e:#}"))?;
    fs_server
        .router_base()
        .map_err(|e| format!("build merged file-server router: {e:#}"))
}

/// 启动 60000 前置分流代理（AllRust → agent_runner 自身 HTTP 端口）。
///
/// 上游即本进程的 8086（file-server 路由已 merge），不再有独立内嵌 listener。
/// `rust_upstream_port`: agent_runner HTTP 端口（main 的 `config.port`）。
/// 失败只 `warn!` 不阻断（外部经 60000 的请求会 502，8086 直连路径不受影响）。
pub async fn spawn_file_server_proxy(rust_upstream_port: u16) {
    let listen_port = env_port("FILE_SERVER_PORT", FILE_SERVER_PORT_DEFAULT);
    file_server_proxy::init(FileServerProxyConfig {
        listen_port,
        rust_upstream_port,
        ts_upstream_port: 60001,
        policy: RoutePolicy::AllRust,
    });
    match file_server_proxy::try_start().await {
        Ok(address) => {
            info!("file-server 前置代理启动: {address} → 127.0.0.1:{rust_upstream_port} (AllRust)")
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
