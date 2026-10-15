//! nuwax-file-server 前置分流反向代理（60000 端口，阶段三终态）。
//!
//! 架构位置：Java/外部 → `:60000` 本代理 → 按策略分流（词汇表
//! `ts_first | all_rust | all_ts`，serde/CLI/env/helm 四层同源）：
//! - `ts_first`（默认）：`/api/v1/userapp*` 前缀，**或** `x-service-type: userapp`
//!   header → rust 上游（rcoder 拦截层转发 per-app 容器——per-app RBD 架构下
//!   只有容器读得到 app 工作区）；其余（无 userApp 标记的存量流量）→ TS
//!   nuwax-file-server（存量域继续 TS 承载）
//! - `all_rust`：一律 rust 上游（60000 白名单：`/api/*`、`/health`、`/`、`/api-docs*`）
//! - `all_ts`：一律 TS 上游（Rust 故障回退/AB 对照档）
//!
//! rust 上游两种承载（编译期 feature + 运行时开关）：
//! - **纯转发**（容器/rcoder 嵌入形态）：loopback 转发 `rust_upstream_port`
//!   （8086=file-server 路由 merge 进宿主进程；EMBED 未设即此形态）
//! - **进程内直连**（feature `embed-file-server` + `--embed`/EMBED=1）：rust 域
//!   请求直接 `router.oneshot` 进以 lib 集成的 file-server axum Router——无内部
//!   监听端口、零 loopback 跳（npm/Electron 独立形态）
//!
//! 双判据的由来：`/api/v1/userapp` 前缀是 userApp 新契约的专属路径（TS 无此路由，
//! 按 path 分流零歧义）——Java 同事尚未接入 header 契约时的兜底判据；
//! `x-service-type` 是存量路径（computer/project 等两实现同构）上的业务域显式
//! 声明——Java 同事接入后的正名路径。两者任一命中即走 Rust 上游
//! （`ts_first` 例外：header 判据失效，交 TS 内部消费）。
//!
//! **header 契约**（待传达给 Java 同事）：
//! - 走 60000 入口的 userApp 业务请求（含存量路径形态）带 `x-service-type: userapp`
//! - 直连 8086 的 `/api/v1/userapp/*` 请求带 `x-app-id: {app_id}`（转发定位容器；
//!   POST/multipart 的 app_id 不解析 body 拿不到，header 是唯一无损通道）
//!
//! 独立 crate 而不入 rcoder-proxy：rcoder-proxy 是端口参数化容器反代，本模块
//! 只做单一职责的业务域分流；后续 TS→Rust 存量域灰度切流在此演进。
//!
//! ## 运行时生命周期
//!
//! `rcoder file-server {start,stop,restart,status}` CLI 经 rcoder admin API
//! `/api/system/file-server/*` 驱动（模式复刻自阶段二内嵌 file-server 的运行时启停，
//! f55f230）。开发测试期在 60000 入口切换"分流代理 vs TS 直跑"对比两侧实现：
//! 1. `rcoder file-server stop` —— 60000 释放
//! 2. 容器内 `nuwax-file-server start --env production --port 60000`（TS 直跑）
//! 3. 对比完成后 kill TS，`rcoder file-server start` 代理重占
//!
//! 实现用 hyper（listener 自持 + graceful shutdown）而非 pingora：pingora
//! `Server::run_forever` 无程序化停机 API，无法支撑反复启停释放端口。
//! 启动语义：同步 bind（返回时状态准确）；stop 返回时端口已确认释放。

mod config;
mod instance;
mod proxy;

/// PX-08: 停止链共享总预算——supervisor `graceful_stop` 与 `ProxyControl::
/// shutdown_grace` 同源; `stop()` 各阶段（proxy 排水 → embedded 关闭 → TS
/// 树停止）从同一绝对 deadline 取剩余, 不逐段重新计时（修复前串行最坏
/// 10+30+3 秒互相矛盾）。
pub const GRACEFUL_STOP_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

pub use config::{
    AGENT_FILE_SERVER_PORT, FileServerProxyConfig, NUWAX_FILE_SERVER_INTERNAL_PORT, RoutePolicy,
    SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP, USERAPP_PATH_PREFIX, Upstream, parse_route_policy,
};
pub use instance::{init, init_result, status, stop, try_start};

/// PX-03: native 形态文件 API 凭据落盘——与控制通道凭据（receipt token）分离;
/// `credentials.json` 权限 0600, 只供宿主 helper 在 owner root 下读取。
/// 令牌不进日志/stdout/回执/argv; 写失败由调用方 fail-fast。
pub fn write_native_credentials(root: &std::path::Path, token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    // Write only to the private temporary handle: the published path may still
    // be a stale file or symlink, and must never be opened or truncated here.
    serde_json::to_writer(&mut file, &serde_json::json!({ "file_api_token": token }))
        .map_err(std::io::Error::other)?;
    file.flush()?;
    file.as_file().sync_all()?;
    process_utils::atomic_file::persist(file, &root.join("credentials.json"))?;
    #[cfg(unix)]
    {
        std::fs::File::open(root)?.sync_all()?;
    }
    // Windows inherits the state directory ACL; explicit ACL validation is
    // still a separate native-platform acceptance requirement.
    Ok(())
}

#[cfg(all(test, unix))]
mod credential_tests {
    use super::write_native_credentials;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn credential_publication_replaces_symlink_without_modifying_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("unrelated.json");
        std::fs::write(&target, b"keep original bytes").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let credentials = root.path().join("credentials.json");
        symlink(&target, &credentials).unwrap();

        write_native_credentials(root.path(), "first-private-token").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"keep original bytes");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(
            !std::fs::symlink_metadata(&credentials)
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(&credentials)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let first: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&credentials).unwrap()).unwrap();
        assert_eq!(first["file_api_token"], "first-private-token");

        write_native_credentials(root.path(), "second-private-token").unwrap();
        let second: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&credentials).unwrap()).unwrap();
        assert_eq!(second["file_api_token"], "second-private-token");
        assert_eq!(
            std::fs::metadata(&credentials)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
pub use proxy::ProxyBody;
#[cfg(feature = "embed-file-server")]
pub use proxy::{clear_in_process_router, set_in_process_router};

#[cfg(test)]
mod tests;
