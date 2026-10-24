//! 健康状态机：失败计数升降级、skip 窗口判定、过期健康状态清理。
//! [`ContainerStatusChecker`] 的状态机方法以独立 impl 块挂在本模块
//! （与 checker.rs 的流程方法分离，纯状态转移集中于此）。

use chrono::{DateTime, Utc};
use shared_types::ProjectStore as _;
use std::time::Duration;
use tracing::{debug, info, warn};

use super::checker::ContainerStatusChecker;

/// 一次健康观察所属的物理身份（退避按物理代次隔离：逻辑键相同但
/// Pod/容器已换代时，新实例不继承前任的失败窗口）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct HealthScope {
    /// 注册快照捕获的物理 UID（K8s Pod UID / Docker 容器 ID）。
    pub(super) container_id: String,
    /// 注册快照捕获的 workload UID（Docker 为 None）。
    pub(super) workload_uid: Option<String>,
}

impl HealthScope {
    pub(super) fn of(basic: &shared_types::ContainerBasicInfo) -> Self {
        Self {
            container_id: basic.container_id.clone(),
            workload_uid: basic.workload_uid.clone(),
        }
    }

    pub(super) fn key(&self, lookup_key: &str) -> (String, Self) {
        (lookup_key.to_owned(), self.clone())
    }

    fn matches(&self, health: &ContainerHealthState) -> bool {
        health.scope.as_ref() == Some(self)
    }
}

/// 容器健康状态
#[derive(Debug, Clone)]
pub(super) struct ContainerHealthState {
    /// 该状态所属的物理代次；None 为历史条目（按旧数据保留，比较时永不匹配）。
    pub(super) scope: Option<HealthScope>,
    /// 连续失败次数
    pub(super) consecutive_failures: u32,
    /// 首次失败时间
    pub(super) first_failure_time: Option<DateTime<Utc>>,
    /// 最后检查时间
    pub(super) last_check_time: DateTime<Utc>,
    /// 最后成功时间
    pub(super) last_success_time: Option<DateTime<Utc>>,
}

impl ContainerHealthState {
    /// 创建新的健康状态
    pub(super) fn new(scope: HealthScope) -> Self {
        Self {
            scope: Some(scope),
            consecutive_failures: 0,
            first_failure_time: None,
            last_check_time: Utc::now(),
            last_success_time: Some(Utc::now()),
        }
    }

    /// 创建失败状态
    pub(super) fn new_failed(scope: HealthScope) -> Self {
        let now = Utc::now();
        Self {
            scope: Some(scope),
            consecutive_failures: 1,
            first_failure_time: Some(now),
            last_check_time: now,
            last_success_time: None,
        }
    }
}

/// 容器状态检查配置
#[derive(Debug, Clone)]
pub struct ContainerStatusCheckerConfig {
    /// 检查间隔（默认 30 秒）
    pub check_interval: Duration,
    /// 查询超时（默认 5 秒）
    pub query_timeout: Duration,
    /// 连续失败阈值（默认 3 次）
    pub failure_threshold: u32,
    /// 失败容器跳过时间（默认 5 分钟）
    pub skip_duration: Duration,
    /// 健康状态重置周期（默认 30 分钟）
    pub health_reset_interval: Duration,
}

impl Default for ContainerStatusCheckerConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(30),
            query_timeout: Duration::from_secs(5),
            failure_threshold: 3,
            skip_duration: Duration::from_secs(5 * 60),
            health_reset_interval: Duration::from_secs(30 * 60),
        }
    }
}

impl ContainerStatusChecker {
    /// 判断是否应该跳过检查（view: 闭包结束读锁立即释放）。
    /// 仅同一物理代次的失败窗口生效：换代后的新实例立即恢复探测。
    pub(super) fn should_skip_check(&self, lookup_key: &str, scope: &HealthScope) -> bool {
        let now = Utc::now();
        self.health_states
            .view(&scope.key(lookup_key), |_, health| {
                if !scope.matches(health) {
                    return false;
                }
                if health.consecutive_failures >= self.config.failure_threshold
                    && let Some(first_failure) = health.first_failure_time
                    && let Ok(skip_duration) = chrono::Duration::from_std(self.config.skip_duration)
                {
                    return now.signed_duration_since(first_failure) < skip_duration;
                }
                false
            })
            .unwrap_or(false)
    }

    /// 记录成功并重置失败计数器（旧代次的迟到成功不得清掉新代次的退避）
    pub(super) fn record_success(&self, lookup_key: &str, scope: &HealthScope) {
        let now = Utc::now();

        use dashmap::mapref::entry::Entry;

        match self.health_states.entry(scope.key(lookup_key)) {
            Entry::Occupied(mut entry) => {
                let health = entry.get_mut();
                if !scope.matches(health) {
                    // 旧代次的迟到成功回调：不得清掉任何在管代次的退避窗口。
                    // 新代次的首个成功无需条目（无条目=无退避），其后续失败
                    // 会按自身代次重新起算。
                    return;
                }
                let was_failing = health.consecutive_failures > 0;
                health.consecutive_failures = 0;
                health.first_failure_time = None;
                health.last_check_time = now;
                health.last_success_time = Some(now);
                // 无需 insert，修改已生效

                if was_failing {
                    info!("[STATUS_CHECKER] Container recovered: {}", lookup_key);
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(ContainerHealthState::new(scope.clone()));
            }
        }
    }

    /// 记录失败并清理连接（新物理代次的失败不叠加到前任计数上）
    pub(super) fn record_failure(
        &self,
        lookup_key: &str,
        scope: &HealthScope,
        grpc_addr: &str,
        error: &anyhow::Error,
    ) {
        let now = Utc::now();

        use dashmap::mapref::entry::Entry;

        let consecutive_failures = match self.health_states.entry(scope.key(lookup_key)) {
            Entry::Occupied(mut entry) => {
                let health = entry.get_mut();
                if !scope.matches(health) {
                    // 换代：新代次从 1 起算，不继承前任失败窗口
                    *health = ContainerHealthState::new_failed(scope.clone());
                    1
                } else {
                    health.consecutive_failures += 1;
                    health.last_check_time = now;
                    if health.first_failure_time.is_none() {
                        health.first_failure_time = Some(now);
                    }
                    // 无需 insert，修改已生效
                    health.consecutive_failures
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(ContainerHealthState::new_failed(scope.clone()));
                1
            }
        };

        // 🔌 第1次失败或达到阈值时，清理 gRPC 连接池
        // 用 spawn fire-and-forget：status_checker 是周期性任务，下次循环会重新检查，
        // 不需要 remove 立即生效（与 chat 重试路径不同）
        if consecutive_failures == 1 || consecutive_failures == self.config.failure_threshold {
            let pool = self.state.grpc_pool.clone();
            let addr_owned = grpc_addr.to_string();
            tokio::spawn(async move {
                pool.remove(&addr_owned).await;
            });
            info!("[STATUS_CHECKER] Already cleanup connection: {}", grpc_addr);
        }

        // 📊 分级日志输出
        match consecutive_failures {
            1 => {
                // 首次失败：INFO 级别
                info!(
                    "[STATUS_CHECKER] Container first query failed: {} - {}",
                    lookup_key, error
                );
            }
            n if n < self.config.failure_threshold => {
                // 持续失败但未达到阈值：DEBUG 级别
                debug!(
                    "[STATUS_CHECKER] Container continuous failure ({}/{}): {}",
                    n, self.config.failure_threshold, lookup_key
                );
            }
            n if n == self.config.failure_threshold => {
                // 达到阈值：WARN 级别
                warn!(
                    "[STATUS_CHECKER] Container continuous failures reached threshold, will skip check temporarily: {} (failures: {})",
                    lookup_key, n
                );
            }
            _ => {
                // 超过阈值后的偶发检查：DEBUG 级别
                debug!("[STATUS_CHECKER] Skipping check for: {}", lookup_key);
            }
        }
    }

    /// 清理过期的健康状态
    pub(super) fn cleanup_stale_health_states(&self) {
        let now = Utc::now();
        let retention_duration = match chrono::Duration::from_std(self.config.health_reset_interval)
        {
            Ok(d) => d,
            Err(_) => return,
        };

        let mut removed_count = 0;

        // Snapshot live physical keys outside the health map's locks. Logical
        // user keys are not project IDs, and shared containers have many rows.
        let live: std::collections::HashSet<_> = self
            .state
            .projects
            .iter()
            .into_iter()
            .filter_map(|(_, info)| {
                info.container_info()
                    .map(|basic| HealthScope::of(&basic).key(info.container_key()))
            })
            .collect();
        self.health_states.retain(|key, health| {
            let keep = live.contains(key)
                && now.signed_duration_since(health.last_check_time) <= retention_duration;
            if !keep {
                removed_count += 1;
            }
            keep
        });

        if removed_count > 0 {
            info!(
                "[STATUS_CHECKER] Cleaned up stale health states: removed={}",
                removed_count
            );
        }
    }
}
