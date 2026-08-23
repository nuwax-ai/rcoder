//! agent_runner 同进程嵌入 file-server。
//!
//! [`spawn_embedded_file_server`] 在 agent_runner 进程内启动 file-server axum
//! (默认端口 60000,由 `FILE_SERVER_PORT` env 覆盖)。与 rcoder 不同,agent_runner 是
//! per-agent 容器,workspace 在本地 (`/home/user`),故直接用 file-server 默认的
//! `LocalWorkspaceResolver`(`FileServerBuilder::build()` 不传 resolver 即默认),
//! 不需要 Subvolume / cephfs 聚合解析。
//!
//! env 开关 `RCODER_EMBED_FILE_SERVER=true|1` 启用;配套 start-up.sh 须设
//! `PROJECT_SOURCE_DIR=/home/user` 等覆盖 file-server 的 `/app/...` 默认路径(默认值在
//! agent-runner 容器内不存在)。任何阶段失败只 `warn!`,不阻断 agent_runner 启动。
//!
//! 参照 `crates/rcoder/src/file_server_embed.rs`(rcoder 版多一层 SubvolumeWorkspaceResolver,
//! 此处省略)。

use file_server::{Config, FileServer};
use tracing::{info, warn};

/// 启动嵌入式 file-server(agent_runner 同进程)。
///
/// `Config::load()`(自读 env)+ `FileServer::builder(config).build()`(默认
/// `LocalWorkspaceResolver`)+ `tokio::spawn` serve。任何阶段失败只 `warn!`,
/// 不阻断 agent_runner 主循环(对齐 rcoder)。
pub async fn spawn_embedded_file_server() {
    let fs_config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            warn!("load file-server config failed, embedded file-server not started: {e:#}");
            return;
        }
    };
    let address = format!("{}:{}", fs_config.listen_host, fs_config.port);
    let fs_server = match FileServer::builder(fs_config).build() {
        Ok(s) => s,
        Err(e) => {
            warn!("build embedded file-server failed, not started: {e:#}");
            return;
        }
    };
    info!(
        version = file_server::VERSION,
        address = %address,
        "file-server (embedded) starting"
    );
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(&address).await {
            Ok(listener) => {
                info!("file-server (embedded) listening on {}", address);
                if let Err(e) = fs_server.serve(listener).await {
                    warn!("embedded file-server serve exited: {e:#}");
                }
            }
            Err(e) => warn!("file-server bind {} failed: {e}", address),
        }
    });
}
