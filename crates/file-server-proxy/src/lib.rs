//! nuwax-file-server 前置分流反向代理（60000 端口，阶段三终态）。
//!
//! 架构位置：Java/外部 → `:60000` 本代理 → 按策略分流（词汇表
//! `userapp_split | all_rust | all_ts | ts_first`，serde/CLI/env/helm 四层同源）：
//! - `userapp_split`（默认）：`/api/v1/userapp*` 前缀，**或** `x-service-type: userapp`
//!   header → rust 上游；其余 → TS（存量域继续 TS nuwax-file-server）
//! - `all_rust`：一律 rust 上游（60000 白名单：`/api/*`、`/health`、`/`、`/api-docs*`）
//! - `all_ts`：一律 TS 上游（Rust 故障回退/AB 对照档）
//! - `ts_first`：**仅** `/api/v1/userapp*`（TS 无此路由）→ rust 上游；存量同名接口
//!   全走 TS——含带 userApp 标记的请求（header 判据失效，由 TS 以 service_type
//!   入参内部处理 userApp 业务；过渡切流档）
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

pub use config::{
    AGENT_FILE_SERVER_PORT, FileServerProxyConfig, NUWAX_FILE_SERVER_INTERNAL_PORT, RoutePolicy,
    SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP, USERAPP_PATH_PREFIX, Upstream, parse_route_policy,
};
pub use instance::{init, status, stop, try_start};
pub use proxy::ProxyBody;
#[cfg(feature = "embed-file-server")]
pub use proxy::{clear_in_process_router, set_in_process_router};

#[cfg(test)]
mod tests;
