//! ProjectStore：project/session/container 映射存储契约
//!
//! 与 [`crate::ContainerLookup`] 并列的跨 crate 存储契约（单一事实源）：
//! - 实现 A：`rcoder_storage::ProjectAdapter`（DashMap 内存实现，docker compose / 单节点路径）
//! - 实现 B：`rcoder_storage` 的 `pg` feature 下的 PgStore（内存镜像 + PostgreSQL
//!   write-behind 持久化，k8s 多副本路径）
//! - 装配：`rcoder_storage::ProjectStoreBackend` 枚举静态分发（Memory / Postgres）
//!
//! 设计约束：
//! - **同步 trait**：读路径走实现内部的内存镜像（热路径每消息一次 session resolve，
//!   且 [`crate::ContainerLookup`] 的消费方 Pingora 是同步调用），PG 持久化由
//!   实现内部的异步 writer task 完成（write-behind），不在本 trait 暴露 async。
//! - 方法粒度为业务语义（非表操作），返回 owned 值，实现内聚 service_type 校验。
//!
//! 运行态真源说明：容器实际状态以 K8s/Docker API 为准（label + 确定性命名 + PVC），
//! 本存储承载的是路由所需的映射关系与活动状态；rcoder 重启后可由 PG 全量加载
//! 或 pod_ensure 懒重建。

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType, StorageStats};

/// One background status observation bound to the physical incarnation it
/// probed. A probe result may only be written back while the registration
/// still holds exactly this physical identity; a replaced or retired
/// registration must never inherit a predecessor's activity clock or status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentStatusObservation {
    /// Physical registry key captured together with the incarnation.
    pub container_name: String,
    /// Physical UID of the probed instance (K8s Pod UID / Docker container ID).
    pub container_id: String,
    /// Controller identity captured with the probe (Docker: None).
    pub workload_uid: Option<String>,
    /// Project incarnation captured before dispatch, checked by both backends.
    pub project_generation: String,
    /// Structural revision captured with the generation.
    pub project_revision: i64,
    /// Memory-only projections may not have a durable container registration.
    pub container_generation: Option<String>,
    /// Captured container structural revision, never reread from a successor.
    pub container_revision: Option<i64>,
    /// Whether the probed instance reported active work.
    pub active: bool,
}

impl AgentStatusObservation {
    /// Capture owned identities before any runtime/RPC wait. A shared physical
    /// projection already inconsistent with its durable binding is not a probe.
    pub fn capture(info: &ProjectAndContainerInfo, active: bool) -> Option<Self> {
        let basic = info.container_info()?;
        let identity = info.persistence_identity();
        if identity
            .container
            .as_ref()
            .and_then(|value| value.physical_uid.as_ref())
            .is_some_and(|uid| uid != &basic.container_id)
        {
            return None;
        }
        Some(Self {
            container_name: basic.container_name,
            container_id: basic.container_id,
            workload_uid: basic.workload_uid,
            project_generation: identity.generation.clone(),
            project_revision: identity.revision,
            container_generation: identity
                .container
                .as_ref()
                .map(|value| value.generation.clone()),
            container_revision: identity.container.as_ref().map(|value| value.revision),
            active,
        })
    }

    pub fn matches(&self, info: &ProjectAndContainerInfo) -> bool {
        Self::capture(info, self.active).as_ref() == Some(self)
    }
}

/// project/session/container 映射存储契约（内存与 PG 双后端统一接口）
pub trait ProjectStore: Send + Sync {
    // ========== 查询（纯读，无副作用） ==========

    /// 按 project_id 取项目记录（含容器信息快照）
    fn get(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>>;

    /// project 是否存在（cleaner / status_checker 的存在性检查）
    fn contains_key(&self, project_id: &str) -> bool;

    /// 全量遍历（闲置扫描 / 状态巡检 / debug 快照；数据集 = 活跃 project 数）
    fn iter(&self) -> Vec<(String, Arc<ProjectAndContainerInfo>)>;

    /// **热路径**：按 session_id 反查所属 project（gateway `/internal/session/{id}/resolve`
    /// 与 SSE/chat 建流前的定位）。孤儿条目由实现自愈清理。
    fn get_by_session_id(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>>;

    /// **热路径**：session_id → 容器名（SSE progress 定位 agent_runner 容器）
    fn get_container_name_by_session(&self, session_id: &str) -> Option<String>;

    /// 全部容器记录快照（pod_list / 对账）
    fn get_all_container_records(&self) -> Vec<ContainerBasicInfo>;

    /// 按容器 ID 反查关联的全部 project（容器销毁时的连带清理、pod_list）
    fn get_projects_by_container_id(&self, container_id: &str)
    -> Vec<Arc<ProjectAndContainerInfo>>;

    /// 按用户 ID + 服务类型查容器（Computer 模式：cancel / 权限 / VNC 状态）
    fn get_container_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Option<ContainerBasicInfo>;

    /// 按共享容器 Pod ID 查容器（pod 共享模式）
    fn get_container_by_pod_id(&self, pod_id: &str) -> Option<ContainerBasicInfo>;

    /// 按用户 ID 查全部 project（cleanup strategy 判断容器可销毁性）
    fn find_projects_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Vec<Arc<ProjectAndContainerInfo>>;

    /// 按共享容器 Pod ID 查全部 project（cleanup rcoder strategy）
    fn find_projects_by_pod_id(&self, pod_id: &str) -> Vec<Arc<ProjectAndContainerInfo>>;

    /// 存储统计（debug 端点 / cleaner 周期日志）
    fn get_stats(&self) -> StorageStats;

    /// 人类可读摘要（debug/sql 端点）
    fn dump_summary(&self) -> String;

    // ========== 写入（实现保证：内存镜像即时生效，PG 后端另做异步持久化） ==========

    /// 插入或更新 project（upsert，幂等）。
    ///
    /// 自动维护容器引用计数：project 已存在且容器变更时旧容器引用 -1。
    /// # Errors
    /// `service_type` 未设置时返回错误（Fail Fast）。
    fn insert(&self, project_id: String, info: Arc<ProjectAndContainerInfo>) -> Result<()>;

    /// 插入 project 并登记 session（add-only 语义，不清除其他 session）
    fn insert_with_session(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: Option<&str>,
    ) -> Result<()>;

    /// 为 project 追加 session 并刷新活跃时间；project 不存在返回 false
    fn add_session_to_project(&self, project_id: &str, session_id: &str) -> bool;

    /// 删除 project（RAII：自动清理 session 索引与容器引用计数）。
    /// 返回被删除的记录（不存在则 None）。
    fn remove(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>>;

    /// 清空 project 的全部 session（agent stop 场景）
    fn clear_session(&self, project_id: &str);

    /// 移除单个 session（SSE 单流结束）；返回该 session 是否曾存在
    fn clear_session_one(&self, project_id: &str, session_id: &str) -> bool;

    /// 刷新 project（及其容器）活跃时间；返回刷新后的时间
    fn update_activity(&self, project_id: &str) -> Option<DateTime<Utc>>;

    /// **热路径**：按 session 刷新所属 project + 容器活跃时间（SSE 每事件回调，
    /// 实现内部节流持久化）；session 未知返回 false
    fn update_session_activity(&self, session_id: &str) -> bool;

    /// 更新 agent 运行状态（status_checker / idle 标记）
    fn update_agent_status(&self, project_id: &str, status: i32, message: &str) -> bool;

    /// 按观察代次应用后台状态结论：仅当注册仍持有该物理实例（UID/workload
    /// 一致）时更新 agent 状态，且仅在 `active` 时刷新活动时钟；换代/缺席
    /// 返回 false，观察结果整体丢弃，绝不写给后继代次。
    fn apply_agent_status_observation(
        &self,
        project_id: &str,
        observation: &AgentStatusObservation,
    ) -> bool;

    // ========== 删除清理 ==========

    /// 仅删除容器及其全部关联 project 的元数据。
    ///
    /// 返回 (容器是否存在, 连带删除的 project 数)。运行时调用方负责原物理实例
    /// 的受控停止及连接退休；本方法不会提交额外物理删除、SSE或路由清理。
    fn delete_container_with_projects(&self, container_id: &str) -> (bool, usize);
}
