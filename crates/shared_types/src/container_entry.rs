//! 容器条目：带引用计数和活跃时间跟踪
//!
//! 每个 ContainerEntry 对应一个物理容器（Docker container 或 K8s Pod）。
//! ref_count 跟踪有多少 project 引用此容器，归零时触发 RAII 清理。
//!
//! 使用 `Arc<ContainerEntry>` 共享，避免 Clone 导致原子状态分裂。
//! `info` 和 `service_type` 使用 `RwLock` 实现内部可变性。

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use parking_lot::RwLock;

use chrono::{DateTime, Utc};
use crate::{ContainerBasicInfo, ServiceType};

/// 容器条目（存储在 DashMap 中，container_name 为 key）
///
/// 通过 `Arc` 共享，确保引用计数和活跃时间在所有持有者之间一致。
pub struct ContainerEntry {
    /// 容器基本信息（RwLock: 允许通过 Arc 更新）
    info: RwLock<ContainerBasicInfo>,
    /// 服务类型（RwLock: 允许通过 Arc 更新）
    service_type: RwLock<ServiceType>,
    /// 逻辑标识（Computer→user_id/pod_id，Web→project_id/pod_id）。
    /// 容器生命周期内稳定，供 RAII 清理作 `CleanupRequest.identifier`
    /// （清理链路按 logical id，而非 DashMap 的 container_name 键）。
    logical_id: String,
    /// 引用计数：有多少 project 引用此容器
    ref_count: AtomicUsize,
    /// 最后活跃时间（Unix 秒，原子更新，无锁）
    last_activity_ts: AtomicI64,
}

impl ContainerEntry {
    /// 创建新条目，ref_count 初始为 1
    pub fn new(info: ContainerBasicInfo, service_type: ServiceType, logical_id: String) -> Self {
        let now = Utc::now().timestamp();
        Self {
            info: RwLock::new(info),
            service_type: RwLock::new(service_type),
            logical_id,
            ref_count: AtomicUsize::new(1),
            last_activity_ts: AtomicI64::new(now),
        }
    }

    /// 创建新条目，指定初始 ref_count
    pub fn with_ref_count(
        info: ContainerBasicInfo,
        service_type: ServiceType,
        logical_id: String,
        ref_count: usize,
    ) -> Self {
        let now = Utc::now().timestamp();
        Self {
            info: RwLock::new(info),
            service_type: RwLock::new(service_type),
            logical_id,
            ref_count: AtomicUsize::new(ref_count),
            last_activity_ts: AtomicI64::new(now),
        }
    }

    /// 获取容器信息的克隆
    pub fn info(&self) -> ContainerBasicInfo {
        self.info.read().clone()
    }

    /// 获取服务类型
    pub fn service_type(&self) -> ServiceType {
        self.service_type.read().clone()
    }

    /// 获取逻辑标识（RAII 清理 identifier 用）
    pub fn logical_id(&self) -> &str {
        &self.logical_id
    }

    /// 更新容器信息和服务类型（logical_id 在容器生命周期内稳定，不更新）
    pub fn update(&self, new_info: ContainerBasicInfo, new_service_type: ServiceType) {
        *self.info.write() = new_info;
        *self.service_type.write() = new_service_type;
    }

    /// 当前引用计数
    pub fn ref_count(&self) -> usize {
        self.ref_count.load(Ordering::Acquire)
    }

    /// 增加引用，返回增加后的值
    pub fn inc_ref(&self) -> usize {
        self.ref_count.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// 减少引用，返回减少后的值
    pub fn dec_ref(&self) -> usize {
        self.ref_count.fetch_sub(1, Ordering::AcqRel) - 1
    }

    /// 最后活跃时间
    pub fn last_activity(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.last_activity_ts.load(Ordering::Relaxed), 0)
            .unwrap_or_default()
    }

    /// 更新活跃时间为当前
    pub fn update_activity(&self) {
        self.last_activity_ts
            .store(Utc::now().timestamp(), Ordering::Relaxed);
    }

    /// 判断是否空闲（超过 idle_minutes 分钟无活跃）
    pub fn is_idle(&self, idle_minutes: i64) -> bool {
        let last = self.last_activity();
        Utc::now().signed_duration_since(last).num_minutes().abs() >= idle_minutes
    }
}

impl std::fmt::Debug for ContainerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerEntry")
            .field("container_name", &self.info())
            .field("service_type", &self.service_type())
            .field("logical_id", &self.logical_id)
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
            ServiceType::WebAgentRunner,
            "proj-1".to_string(),
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
        entry.update_activity();
        let after = entry.last_activity();
        assert!(after >= before);
    }

    #[test]
    fn test_is_idle() {
        let entry = make_entry();
        assert!(!entry.is_idle(1));
    }

    #[test]
    fn test_with_ref_count() {
        let entry = ContainerEntry::with_ref_count(
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
            ServiceType::WebAgentRunner,
            "proj-1".to_string(),
            0,
        );
        assert_eq!(entry.ref_count(), 0);
        assert_eq!(entry.logical_id(), "proj-1");
    }

    #[test]
    fn test_update_info() {
        let entry = make_entry();
        assert_eq!(entry.info().container_name, "test-name");

        let new_info = ContainerBasicInfo {
            container_id: "new-id".to_string(),
            container_name: "new-name".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 9090,
            external_port: 0,
            project_id: "proj-2".to_string(),
            status: "stopped".to_string(),
            created_at: Utc::now(),
            service_url: "http://new".to_string(),
        };
        entry.update(new_info, ServiceType::ComputerAgentRunner);
        assert_eq!(entry.info().container_name, "new-name");
        assert_eq!(entry.info().container_id, "new-id");
        assert_eq!(entry.service_type(), ServiceType::ComputerAgentRunner);
    }
}
