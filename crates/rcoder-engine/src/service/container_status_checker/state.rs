//! 健康状态机：失败计数升降级、skip 窗口判定、过期健康状态清理。
//! [`ContainerStatusChecker`] 的状态机方法以独立 impl 块挂在本模块
//! （与 checker.rs 的流程方法分离，纯状态转移集中于此）。

use chrono::{DateTime, Utc};
use std::time::Duration;
use tracing::{debug, info, warn};

use super::checker::ContainerStatusChecker;

/// 容器健康状态
#[derive(Debug, Clone)]
pub(super) struct ContainerHealthState {
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
    pub(super) fn new() -> Self {
        Self {
            consecutive_failures: 0,
            first_failure_time: None,
            last_check_time: Utc::now(),
            last_success_time: Some(Utc::now()),
        }
    }

    /// 创建失败状态
    pub(super) fn new_failed() -> Self {
        let now = Utc::now();
        Self {
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
    /// 判断是否应该跳过检查（view: 闭包结束读锁立即释放）
    pub(super) fn should_skip_check(&self, lookup_key: &str) -> bool {
        let now = Utc::now();
        self.health_states
            .view(lookup_key, |_, health| {
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

    /// 记录成功并重置失败计数器
    pub(super) fn record_success(&self, lookup_key: &str) {
        let now = Utc::now();

        use dashmap::mapref::entry::Entry;

        match self.health_states.entry(lookup_key.to_string()) {
            Entry::Occupied(mut entry) => {
                // 使用 get_mut 直接修改，避免克隆
                let was_failing = entry.get().consecutive_failures > 0;
                let health = entry.get_mut();
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
                entry.insert(ContainerHealthState::new());
            }
        }
    }

    /// 记录失败并清理连接
    pub(super) fn record_failure(&self, lookup_key: &str, grpc_addr: &str, error: &anyhow::Error) {
        let now = Utc::now();

        use dashmap::mapref::entry::Entry;

        let consecutive_failures = match self.health_states.entry(lookup_key.to_string()) {
            Entry::Occupied(mut entry) => {
                // 使用 get_mut 直接修改，避免克隆
                let health = entry.get_mut();
                health.consecutive_failures += 1;
                health.last_check_time = now;
                if health.first_failure_time.is_none() {
                    health.first_failure_time = Some(now);
                }
                // 无需 insert，修改已生效
                health.consecutive_failures
            }
            Entry::Vacant(entry) => {
                entry.insert(ContainerHealthState::new_failed());
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

        // 第一步：从 health_states 收集数据（iter 读锁在 collect 后释放）
        let candidates: Vec<(String, bool)> = self
            .health_states
            .iter()
            .map(|entry| {
                let lookup_key = entry.key().clone();
                let elapsed = now.signed_duration_since(entry.value().last_check_time);
                let is_stale = elapsed > retention_duration;
                (lookup_key, is_stale)
            })
            .collect();

        // 第二步：跨 map 查询（health_states 锁已释放，无死锁风险）
        let keys_to_remove: Vec<String> = candidates
            .into_iter()
            .filter(|(lookup_key, is_stale)| {
                let not_in_storage = !self.state.contains_project(lookup_key);
                not_in_storage || *is_stale
            })
            .map(|(k, _)| k)
            .collect();

        // 第三步：批量移除
        for key in keys_to_remove {
            if self.health_states.remove(&key).is_some() {
                removed_count += 1;
                debug!("[STATUS_CHECKER] Cleaned up stale health state: {}", key);
            }
        }

        if removed_count > 0 {
            info!(
                "[STATUS_CHECKER] Cleaned up stale health states: removed={}",
                removed_count
            );
        }
    }
}
