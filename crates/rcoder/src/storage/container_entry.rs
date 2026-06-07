//! 容器条目：带引用计数和活跃时间跟踪
//!
//! 每个 ContainerEntry 对应一个物理容器（Docker container 或 K8s Pod）。
//! ref_count 跟踪有多少 project 引用此容器，归零时触发 RAII 清理。

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use shared_types::{ContainerBasicInfo, ServiceType};

/// 容器条目（存储在 DashMap 中，container_key 为 key）
pub struct ContainerEntry {
    /// 容器基本信息
    pub info: ContainerBasicInfo,
    /// 服务类型（RCoder / ComputerAgentRunner）
    pub service_type: ServiceType,
    /// 引用计数：有多少 project 引用此容器
    ref_count: AtomicUsize,
    /// 最后活跃时间（Unix 秒，原子更新，无锁）
    last_activity_ts: AtomicI64,
}

impl ContainerEntry {
    /// 创建新条目，ref_count 初始为 1
    pub fn new(info: ContainerBasicInfo, service_type: ServiceType) -> Self {
        let now = Utc::now().timestamp();
        Self {
            info,
            service_type,
            ref_count: AtomicUsize::new(1),
            last_activity_ts: AtomicI64::new(now),
        }
    }

    /// 当前引用计数
    pub fn ref_count(&self) -> usize {
        self.ref_count.load(Ordering::Relaxed)
    }

    /// 增加引用，返回增加后的值
    pub fn inc_ref(&self) -> usize {
        self.ref_count.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 减少引用，返回减少后的值
    pub fn dec_ref(&self) -> usize {
        self.ref_count.fetch_sub(1, Ordering::Relaxed) - 1
    }

    /// 最后活跃时间
    pub fn last_activity(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.last_activity_ts.load(Ordering::Relaxed), 0)
            .unwrap_or_default()
    }

    /// 更新活跃时间为当前
    pub fn update_activity(&self) {
        self.last_activity_ts.store(Utc::now().timestamp(), Ordering::Relaxed);
    }

    /// 判断是否空闲（超过 idle_minutes 分钟无活跃）
    pub fn is_idle(&self, idle_minutes: i64) -> bool {
        let last = self.last_activity();
        Utc::now()
            .signed_duration_since(last)
            .num_minutes()
            .abs()
            >= idle_minutes
    }
}

impl Clone for ContainerEntry {
    fn clone(&self) -> Self {
        Self {
            info: self.info.clone(),
            service_type: self.service_type.clone(),
            ref_count: AtomicUsize::new(self.ref_count.load(Ordering::Relaxed)),
            last_activity_ts: AtomicI64::new(self.last_activity_ts.load(Ordering::Relaxed)),
        }
    }
}

impl std::fmt::Debug for ContainerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerEntry")
            .field("container_name", &self.info.container_name)
            .field("service_type", &self.service_type)
            .field("ref_count", &self.ref_count())
            .field("last_activity", &self.last_activity())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry() -> ContainerEntry {
        ContainerEntry::new(
            ContainerBasicInfo {
                container_id: "test-id".to_string(),
                container_name: "test-name".to_string(),
                container_ip: "127.0.0.1".to_string(),
                internal_port: 8086,
                external_port: 0,
                project_id: "proj-1".to_string(),
                status: "running".to_string(),
                created_at: Utc::now(),
                service_url: "http://test".to_string(),
            },
            ServiceType::RCoder,
        )
    }

    #[test]
    fn test_initial_ref_count() {
        let entry = make_entry();
        assert_eq!(entry.ref_count(), 1);
    }

    #[test]
    fn test_inc_dec_ref() {
        let entry = make_entry();
        assert_eq!(entry.inc_ref(), 2);
        assert_eq!(entry.inc_ref(), 3);
        assert_eq!(entry.ref_count(), 3);
        assert_eq!(entry.dec_ref(), 2);
        assert_eq!(entry.dec_ref(), 1);
        assert_eq!(entry.ref_count(), 1);
    }

    #[test]
    fn test_update_activity() {
        let entry = make_entry();
        let before = entry.last_activity();
        // Small sleep is not needed — update_activity sets to Utc::now()
        entry.update_activity();
        let after = entry.last_activity();
        assert!(after >= before);
    }

    #[test]
    fn test_is_idle() {
        let entry = make_entry();
        // Just created, should not be idle
        assert!(!entry.is_idle(1));
    }
}
