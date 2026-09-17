//! dev server 类型定义: 响应载体 / 管理器本体。
//! 字段 `pub(super)`: 供 start.rs / stop.rs / mod.rs (同属 dev_server) 访问,
//! 对 crate 其余部分保持私有。进程记录 DevProcess 在 crate::models
//! （list-dev wire 双面类型）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::port_pool::PortPool;
use crate::Config;
use crate::models::{DevProcess, KilledPid};

/// 探活回调：(port, base_path, timeout_ms) → boxed future<bool>。
/// 抽成类型别名既绕开 clippy::type_complexity，也方便测试注入 stub（绕开 reqwest 延迟）。
pub(super) type AliveProbe<'a> = &'a (
        dyn for<'s> Fn(
    u16,
    Option<&'s str>,
    u64,
) -> std::pin::Pin<Box<dyn Future<Output = bool> + Send + 's>>
            + Sync
    );

/// start-dev / restart-dev 响应。
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartedDev {
    pub pid: u32,
    pub port: u16,
}

/// stop-dev 响应。
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoppedDev {
    pub killed_pids: Vec<KilledPid>,
}

/// keep-alive 结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct KeepAliveResult {
    pub alive: bool,
    pub action: Option<String>,
    /// 重启分支返回新启动的 pid/port (对齐 nuwax 透传 startDevServer 返回值);
    /// alive 分支为 None (调用方用查询入参的 pid/port)。
    pub pid: Option<u32>,
    pub port: Option<u16>,
}

/// 进程停止后未确认清理状态（P1-05）：并发 dev 操作互斥清理——
/// 旧 supervised 退出后再次受理 dev/start 前必须确认清理完毕。
#[derive(Debug, Clone, Copy)]
pub enum CleanupStatus {
    /// 正在清理：进程已停止但 stdout 排空/状态摘除未完成。
    Cleaning,
    /// 清理完成：可以接受新的 dev 操作。
    Cleaned,
}

/// dev server 进程管理器 (经 Arc 注入 AppState)。
pub struct DevServerManager {
    pub(super) processes: Mutex<HashMap<String, DevProcess>>,
    pub(super) starting: Mutex<HashSet<String>>,
    /// UserApp manifest 编排进程（app-cli）的监督句柄（P1-03：唯一
    /// wait/reap + stdout 管道 + stderr ring；vite 路径不登记）。key 与
    /// processes 同（project_id）；stop_dev 同步摘除。
    pub(super) supervised: Mutex<HashMap<String, Arc<super::supervise::SupervisedChild>>>,
    /// 进程停止后未确认清理状态表（P1-05）：并发 dev 操作互斥清理——
    /// 旧 supervised 退出后再次受理 dev/start 前必须确认清理完毕。
    pub(super) cleanup_state: Arc<Mutex<HashMap<String, CleanupStatus>>>,
    /// 构建前捕获的 owner 期望（P3-03）：key=project_id，
    /// value=(runtime_instance_id, revision)——构建期间 owner 被
    /// stop/restart 时，提交按 ERR_REVISION_MISMATCH 拒绝（不自动刷新重发）。
    pub(super) owner_expectations: Mutex<HashMap<String, (String, u64)>>,
    pub(super) port_pool: PortPool,
    pub(super) config: Arc<Config>,
}

impl DevServerManager {
    pub fn new(config: Arc<Config>) -> Self {
        let pool = PortPool::new(
            config.dev_port_range_start,
            config.dev_port_range_end,
            config.dev_port_reserved_start,
            config.dev_port_reserved_end,
        );
        Self {
            processes: Mutex::new(HashMap::new()),
            starting: Mutex::new(HashSet::new()),
            supervised: Mutex::new(HashMap::new()),
            cleanup_state: Arc::new(Mutex::new(HashMap::new())),
            owner_expectations: Mutex::new(HashMap::new()),
            port_pool: pool,
            config,
        }
    }
}
