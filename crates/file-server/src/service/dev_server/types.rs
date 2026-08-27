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

/// dev server 进程管理器 (经 Arc 注入 AppState)。
pub struct DevServerManager {
    pub(super) processes: Mutex<HashMap<String, DevProcess>>,
    pub(super) starting: Mutex<HashSet<String>>,
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
            port_pool: pool,
            config,
        }
    }
}
