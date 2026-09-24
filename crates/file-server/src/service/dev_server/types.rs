//! dev server 类型定义: 响应载体 / 管理器本体。
//! 字段 `pub(super)`: 供 start.rs / stop.rs / mod.rs (同属 dev_server) 访问,
//! 对 crate 其余部分保持私有。进程记录 DevProcess 在 crate::models
//! （list-dev wire 双面类型）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::port_pool::PortPool;
#[cfg(test)]
use super::support::lock;
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
    /// The owner acknowledged stopping its services; its coordinator stays alive.
    #[serde(skip)]
    pub owner_stopped: bool,
}
impl StoppedDev {
    pub fn message(&self) -> &'static str {
        if self.killed_pids.iter().any(|pid| !pid.killed) {
            "Partially stopped but continue execution"
        } else if self.owner_stopped || !self.killed_pids.is_empty() {
            "Stopped"
        } else {
            "No running process found"
        }
    }
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

/// 外部 owner 停止操作记录（R05）：提交受理后立即持久化——响应丢失、
/// 轮询断连、file-server 重启后都按**原 operation_id** 恢复查询，
/// 不产生新 ID、不退回 ps 扫描。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExternalStopRecord {
    pub operation_id: String,
    pub workspace_id: String,
    pub submitted_at_ms: u64,
}

/// 构建前 owner 观察结果（R07 三态：捕获/确认无 owner/观察失败）。
#[derive(Debug, Clone)]
pub(crate) enum OwnerExpectation {
    /// 命中 owner：提交按捕获的 (instance, revision) 校验。
    Captured {
        runtime_instance_id: String,
        revision: u64,
    },
    /// 确认无 owner（探测无应答且非 legacy）——spawn 路径提交时活取。
    NoOwner,
    /// 观察失败（owner 应答存在但凭据/状态不可读，或 legacy 占位）——
    /// 提交拒绝：不能用"没捕获到"刷新期望绕过停止屏障。
    ObservationFailed { reason: String },
}

/// dev server 进程管理器 (经 Arc 注入 AppState)。
pub struct DevServerManager {
    pub(super) processes: Mutex<HashMap<String, DevProcess>>,
    pub(super) starting: Mutex<HashSet<String>>,
    /// Serializes coordinated stops for one preview key through process exit.
    /// A second stop must not see NotRegistered while the first is still killing
    /// the process. Idle entries are pruned when the next stop is admitted.
    pub(super) coordinated_stop_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// UserApp manifest 编排进程（app-cli）的监督句柄（P1-03：唯一
    /// wait/reap + stdout 管道 + stderr ring；vite 路径不登记）。key 与
    /// processes 同（project_id）；stop_dev 同步摘除。
    pub(super) supervised: Mutex<HashMap<String, Arc<super::supervise::SupervisedChild>>>,
    /// 进程停止后未确认清理状态表（P1-05）：并发 dev 操作互斥清理——
    /// 旧 supervised 退出后再次受理 dev/start 前必须确认清理完毕。
    pub(super) cleanup_state: Arc<Mutex<HashMap<String, CleanupStatus>>>,
    /// 构建前捕获的 owner 期望（P3-03/R07）：key=project_id。构建期间 owner
    /// 被 stop/restart 时，提交按 ERR_REVISION_MISMATCH 拒绝（不自动刷新
    /// 重发）；观察失败（owner 在但读不到身份/凭据/状态）记录
    /// [`OwnerExpectation::ObservationFailed`]——提交明确拒绝，不能用
    /// "没捕获到"绕过停止屏障（R07 反例：预检断连 → 用户 Stop → 网络恢复
    /// → 旧构建结束 → 旧提交必须被拒）。
    pub(super) owner_expectations: Mutex<HashMap<String, OwnerExpectation>>,
    /// 在途/最近的外部 owner 停止操作（R05）：确认 Succeeded 前保留——
    /// 重试按原 operation_id 查询（幂等），不重复提交。
    pub(super) external_stops: Mutex<HashMap<String, ExternalStopRecord>>,
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
        let manager = Self {
            processes: Mutex::new(HashMap::new()),
            starting: Mutex::new(HashSet::new()),
            coordinated_stop_locks: Mutex::new(HashMap::new()),
            supervised: Mutex::new(HashMap::new()),
            cleanup_state: Arc::new(Mutex::new(HashMap::new())),
            owner_expectations: Mutex::new(HashMap::new()),
            external_stops: Mutex::new(HashMap::new()),
            port_pool: pool,
            config,
        };
        manager.restore_external_state();
        manager
    }

    /// 恢复持久化的 external owner 登记与在途停止记录（R05：file-server
    /// 重启不丢 owner 控制关系）。坏文件按空处理（诊断 warn，不阻塞启动）。
    fn restore_external_state(&self) {
        let Ok(content) = std::fs::read_to_string(self.external_state_path()) else {
            return;
        };
        let Ok(state) = serde_json::from_str::<super::external_store::State>(&content) else {
            tracing::warn!(
                path = %self.external_state_path().display(),
                "external owner state file unreadable; starting with no persisted owner relations"
            );
            return;
        };
        if let Ok(mut processes) = self.processes.lock() {
            for (key, record) in state.owners {
                // token 置空哨兵：停止路径发现空 token 时从 owner 状态根重读
                processes.insert(
                    key.clone(),
                    DevProcess {
                        pid: record.pid,
                        port: record.port,
                        project_id: record.project_id,
                        instance_id: None,
                        base_path: None,
                        started_at: 0,
                        log_dir: self.config.log_base_dir.clone(),
                        temp_log_name: String::new(),
                        external_owner: Some(crate::models::ExternalOwner {
                            address: record.owner.address,
                            token: String::new(),
                            runtime_instance_id: record.owner.runtime_instance_id,
                        }),
                    },
                );
            }
        }
        if let Ok(mut stops) = self.external_stops.lock() {
            for (key, record) in state.stops {
                stops.insert(key, record);
            }
        }
        tracing::info!("restored persisted external owner state");
    }

    /// Test/legacy registration import. Never overwrite newer disk intents from cache snapshots.
    #[cfg(test)]
    pub(super) fn persist_external_state(&self) -> anyhow::Result<()> {
        let processes = lock(&self.processes).map_err(|error| anyhow::anyhow!("{error}"))?;
        self.external_transaction(|state| {
            for (key, process) in processes.iter() {
                if let Some(owner) = &process.external_owner {
                    state.owners.entry(key.clone()).or_insert_with(|| {
                        super::external_store::OwnerRecord {
                            pid: process.pid,
                            port: process.port,
                            project_id: process.project_id.clone(),
                            registration_operation_id: None,
                            owner: super::external_store::OwnerIdentity {
                                address: owner.address.clone(),
                                runtime_instance_id: owner.runtime_instance_id.clone(),
                            },
                        }
                    });
                }
            }
            Ok(())
        })
    }
}
