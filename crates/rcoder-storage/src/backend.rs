//! ProjectStoreBackend：存储后端枚举（静态分发）
//!
//! 后端在启动时由配置决定、运行期不变，枚举 match 即静态分发（无虚表），
//! 且新增后端时穷尽性检查强制补全（对比泛型 `AppState<S>` 的类型参数传染、
//! `Arc<dyn>` 的虚表开销，枚举是该场景的惯用解）。
//!
//! - [`ProjectStoreBackend::Memory`]：DashMap 纯内存（docker compose / 单节点默认）
//! - [`ProjectStoreBackend::Postgres`]：内存镜像 + PG write-behind（cfg(feature="pg")，
//!   k8s 部署路径；内部复用 ProjectAdapter 作镜像，持久化见 pg 模块）

use std::sync::Arc;

use shared_types::{
    ContainerBasicInfo, ContainerLookup, ProjectAndContainerInfo, ProjectStore, ServiceType,
    StorageStats,
};

use crate::adapter::ProjectAdapter;

/// 存储后端（AppState.projects 的具体类型，经 `Arc<ProjectStoreBackend>` 共享）
pub enum ProjectStoreBackend {
    /// DashMap 纯内存实现（Arc 与 Postgres 变体对称：枚举恒为单指针大小）
    Memory(Arc<ProjectAdapter>),
    /// PG 持久化实现（内存镜像 + write-behind writer + 启动全量加载）。
    /// Arc 包装：sync/leader/shadow 等后台任务需要独立句柄（pg 子树不反依赖本门面）
    #[cfg(feature = "pg")]
    Postgres(Arc<crate::pg::PgStore>),
}

impl ProjectStoreBackend {
    /// 取内层 ProjectAdapter（Memory 即自身；Postgres 为其镜像）。
    /// 供需要内存实现特有能力（如 load_from_rows）的装配代码使用。
    pub fn memory_mirror(&self) -> &ProjectAdapter {
        match self {
            Self::Memory(adapter) => adapter,
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.inner(),
        }
    }

    /// 会话创建的结构性 op **durable 直写**（PG 模式：内存 + sqlx 事务提交，
    /// 方法返回即主库已提交；超时/失败降级 write-behind 队列，chat 不失败）。
    /// Memory 模式等价普通内存写。chat 完成点调用——保证 session_id 返回给
    /// 前端时任何副本回源直查必命中（跨副本可见性契约）。
    pub async fn insert_project_with_session_durable(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: &str,
    ) -> anyhow::Result<()> {
        match self {
            Self::Memory(inner) => inner.insert_with_session(project_id, info, Some(session_id)),
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                store
                    .insert_with_session_durable(project_id, info, session_id)
                    .await
            }
        }
    }

    /// 追加 session 的 durable 变体（/chat 域响应后映射补录）：与
    /// [`Self::insert_project_with_session_durable`] 同契约——返回 `Ok(true)`
    /// 即主库已提交（超时/失败内部降级 write-behind，chat 不失败）。
    /// Memory 模式等价普通内存写。返回 `Ok(false)` = project 不存在。
    pub async fn add_session_durable(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        match self {
            Self::Memory(inner) => Ok(inner.add_session_to_project(project_id, session_id)),
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.add_session_durable(project_id, session_id).await,
        }
    }

    /// 结构性删除的 durable 变体（stop/清理路径）：与插入类 durable 同事务
    /// 语义——消除"remove 入队 → durable insert 提交 → writer 重放删行"的
    /// 倒挂窗口（跨副本可见性契约）。Memory 模式等价普通内存删。
    pub async fn remove_durable(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        match self {
            Self::Memory(inner) => inner.remove(project_id),
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.remove_durable(project_id).await,
        }
    }

    /// [`Self::remove_durable`] 的 clear_session 同构（stop 清全部会话）。
    pub async fn clear_session_durable(&self, project_id: &str) {
        match self {
            Self::Memory(inner) => inner.clear_session(project_id),
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.clear_session_durable(project_id).await,
        }
    }

    /// [`Self::remove_durable`] 的单 session 删除同构。
    pub async fn clear_session_one_durable(&self, project_id: &str, session_id: &str) -> bool {
        match self {
            Self::Memory(inner) => inner.clear_session_one(project_id, session_id),
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                store
                    .clear_session_one_durable(project_id, session_id)
                    .await
            }
        }
    }

    /// [`Self::remove_durable`] 的容器级删除同构（容器销毁路径）。
    pub async fn delete_container_with_projects_durable(
        &self,
        container_id: &str,
    ) -> (bool, usize) {
        match self {
            Self::Memory(inner) => inner.delete_container_with_projects(container_id),
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                store
                    .delete_container_with_projects_durable(container_id)
                    .await
            }
        }
    }

    /// 按 session_id 读（PG 模式 miss 回源直查主库一次并 hydrate 镜像；
    /// Memory 模式仅内存）。SSE lookup 的兜底路径。
    pub async fn get_by_session_with_fetch(
        &self,
        session_id: &str,
    ) -> Option<Arc<ProjectAndContainerInfo>> {
        match self {
            Self::Memory(inner) => inner.get_by_session_id(session_id),
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.get_by_session_id_with_fetch(session_id).await,
        }
    }

    /// PG 后端引用（P2：sync/leader 等后端特有任务的装配入口；Memory 为 None）。
    /// 返回 &Arc：调用方可 clone 独立句柄传给后台任务
    #[cfg(feature = "pg")]
    pub fn postgres(&self) -> Option<&Arc<crate::pg::PgStore>> {
        match self {
            Self::Memory(_) => None,
            Self::Postgres(store) => Some(store),
        }
    }

    /// 是否 PG 持久化后端（启动/关停行为分叉的判据）
    pub fn is_postgres(&self) -> bool {
        match self {
            Self::Memory(_) => false,
            #[cfg(feature = "pg")]
            Self::Postgres(_) => true,
        }
    }

    /// 优雅关停：PG 模式 flush write-behind 队列（有界等待）；
    /// Memory 模式 no-op 返回 true（参数留待 PG 分支使用）。返回 false = 有结构性 op 未落盘。
    pub async fn shutdown_flush(&self, _timeout: std::time::Duration) -> bool {
        match self {
            Self::Memory(_) => true,
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.writer().flush_and_stop(_timeout).await,
        }
    }
}

impl ProjectStore for ProjectStoreBackend {
    fn get(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        match self {
            Self::Memory(inner) => inner.get(project_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.get(project_id),
        }
    }

    fn contains_key(&self, project_id: &str) -> bool {
        match self {
            Self::Memory(inner) => inner.contains_key(project_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.contains_key(project_id),
        }
    }

    fn iter(&self) -> Vec<(String, Arc<ProjectAndContainerInfo>)> {
        match self {
            Self::Memory(inner) => inner.iter(),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.iter(),
        }
    }

    fn get_by_session_id(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        match self {
            Self::Memory(inner) => inner.get_by_session_id(session_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.get_by_session_id(session_id),
        }
    }

    fn get_container_name_by_session(&self, session_id: &str) -> Option<String> {
        match self {
            Self::Memory(inner) => inner.get_container_name_by_session(session_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.get_container_name_by_session(session_id),
        }
    }

    fn get_all_container_records(&self) -> Vec<ContainerBasicInfo> {
        match self {
            Self::Memory(inner) => inner.get_all_container_records(),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.get_all_container_records(),
        }
    }

    fn get_projects_by_container_id(
        &self,
        container_id: &str,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        match self {
            Self::Memory(inner) => inner.get_projects_by_container_id(container_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.get_projects_by_container_id(container_id),
        }
    }

    fn get_container_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Option<ContainerBasicInfo> {
        match self {
            Self::Memory(inner) => inner.get_container_by_user_id(user_id, service_type),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.get_container_by_user_id(user_id, service_type),
        }
    }

    fn get_container_by_pod_id(&self, pod_id: &str) -> Option<ContainerBasicInfo> {
        match self {
            Self::Memory(inner) => inner.get_container_by_pod_id(pod_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.get_container_by_pod_id(pod_id),
        }
    }

    fn find_projects_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        match self {
            Self::Memory(inner) => inner.find_projects_by_user_id(user_id, service_type),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.find_projects_by_user_id(user_id, service_type),
        }
    }

    fn find_projects_by_pod_id(&self, pod_id: &str) -> Vec<Arc<ProjectAndContainerInfo>> {
        match self {
            Self::Memory(inner) => inner.find_projects_by_pod_id(pod_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.find_projects_by_pod_id(pod_id),
        }
    }

    fn get_stats(&self) -> StorageStats {
        match self {
            Self::Memory(inner) => inner.get_stats(),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.get_stats(),
        }
    }

    fn dump_summary(&self) -> String {
        match self {
            Self::Memory(inner) => inner.dump_summary(),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.dump_summary(),
        }
    }

    fn insert(&self, project_id: String, info: Arc<ProjectAndContainerInfo>) -> anyhow::Result<()> {
        match self {
            Self::Memory(inner) => inner.insert(project_id, info),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.insert(project_id, info),
        }
    }

    fn insert_with_session(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Memory(inner) => inner.insert_with_session(project_id, info, session_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.insert_with_session(project_id, info, session_id),
        }
    }

    fn add_session_to_project(&self, project_id: &str, session_id: &str) -> bool {
        match self {
            Self::Memory(inner) => inner.add_session_to_project(project_id, session_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.add_session_to_project(project_id, session_id),
        }
    }

    fn remove(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        match self {
            Self::Memory(inner) => inner.remove(project_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.remove(project_id),
        }
    }

    fn clear_session(&self, project_id: &str) {
        match self {
            Self::Memory(inner) => inner.clear_session(project_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.clear_session(project_id),
        }
    }

    fn clear_session_one(&self, project_id: &str, session_id: &str) -> bool {
        match self {
            Self::Memory(inner) => inner.clear_session_one(project_id, session_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.clear_session_one(project_id, session_id),
        }
    }

    fn update_activity(&self, project_id: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        match self {
            Self::Memory(inner) => inner.update_activity(project_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.update_activity(project_id),
        }
    }

    fn update_session_activity(&self, session_id: &str) -> bool {
        match self {
            Self::Memory(inner) => inner.update_session_activity(session_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.update_session_activity(session_id),
        }
    }

    fn update_agent_status(&self, project_id: &str, status: i32, message: &str) -> bool {
        match self {
            Self::Memory(inner) => inner.update_agent_status(project_id, status, message),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.update_agent_status(project_id, status, message),
        }
    }

    fn delete_container_with_projects(&self, container_id: &str) -> (bool, usize) {
        match self {
            Self::Memory(inner) => inner.delete_container_with_projects(container_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.delete_container_with_projects(container_id),
        }
    }
}

impl ContainerLookup for ProjectStoreBackend {
    fn find_by_user_id(&self, user_id: &str, service_type: &ServiceType) -> Option<String> {
        match self {
            Self::Memory(inner) => inner.find_by_user_id(user_id, service_type),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.find_by_user_id(user_id, service_type),
        }
    }

    fn find_by_project_id(&self, project_id: &str, service_type: &ServiceType) -> Option<String> {
        match self {
            Self::Memory(inner) => inner.find_by_project_id(project_id, service_type),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.find_by_project_id(project_id, service_type),
        }
    }

    fn find_by_pod_id(&self, pod_id: &str, service_type: &ServiceType) -> Option<String> {
        match self {
            Self::Memory(inner) => inner.find_by_pod_id(pod_id, service_type),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.find_by_pod_id(pod_id, service_type),
        }
    }

    fn find_app_runtime_addr(&self, app_id: &str) -> Option<String> {
        match self {
            Self::Memory(inner) => inner.find_app_runtime_addr(app_id),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.find_app_runtime_addr(app_id),
        }
    }

    fn find_project_scope(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> Option<shared_types::ProjectScope> {
        match self {
            Self::Memory(inner) => inner.find_project_scope(project_id, service_type),
            #[cfg(feature = "pg")]
            Self::Postgres(inner) => inner.find_project_scope(project_id, service_type),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举静态分发的最小语义验证：经 ProjectStore trait 调用与直接调用
    /// 内存实现等价（M3 加入 Postgres 变体后，同一测试面将覆盖两变体一致性）。
    #[test]
    fn memory_backend_dispatches_through_trait() {
        let (adapter, _rx) = ProjectAdapter::new("test-ns".into(), "cluster.local".into());
        let backend = ProjectStoreBackend::Memory(Arc::new(adapter));

        let mut info = ProjectAndContainerInfo::new("proj-1".into());
        info.set_service_type(Some(ServiceType::WebAgentRunner));
        info.set_container(Some(ContainerBasicInfo {
            container_id: "c-1".into(),
            container_name: "container-1".into(),
            container_ip: "127.0.0.1".into(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://container-1".into(),
        }));
        backend
            .insert_with_session("proj-1".into(), Arc::new(info), Some("sess-1"))
            .expect("insert via trait");

        assert!(backend.contains_key("proj-1"));
        assert_eq!(
            backend.get_container_name_by_session("sess-1").as_deref(),
            Some("container-1")
        );
        assert!(backend.get_by_session_id("sess-1").is_some());
        assert_eq!(backend.get_stats().total_projects, 1);
        assert!(backend.remove("proj-1").is_some());
        assert!(!backend.contains_key("proj-1"));
    }

    /// ContainerLookup 经枚举转发（Pingora 注入路径）。
    #[test]
    fn memory_backend_implements_container_lookup() {
        let (adapter, _rx) = ProjectAdapter::new("test-ns".into(), "cluster.local".into());
        let backend = ProjectStoreBackend::Memory(Arc::new(adapter));

        let mut info = ProjectAndContainerInfo::new("proj-2".into());
        info.set_service_type(Some(ServiceType::WebAgentRunner));
        info.set_user_id(Some("user-1".into()));
        backend
            .insert("proj-2".into(), Arc::new(info))
            .expect("insert");

        let lookup: &dyn ContainerLookup = &backend;
        // 无容器信息的 project：find_by_project_id 返回 None（尚未 ensure 容器）
        assert!(
            lookup
                .find_by_project_id("proj-2", &ServiceType::WebAgentRunner)
                .is_none()
        );
        assert!(
            lookup
                .find_by_user_id("user-1", &ServiceType::WebAgentRunner)
                .is_none()
        );
    }

    /// load_from_rows 不存在于契约（快照构造走 memory_mirror 特有能力）。
    /// 此处仅验证 mirror 访问器返回同一实现的统计语义。
    #[test]
    fn memory_mirror_accessor() {
        let (adapter, _rx) = ProjectAdapter::new("test-ns".into(), "cluster.local".into());
        let backend = ProjectStoreBackend::Memory(Arc::new(adapter));
        assert_eq!(backend.memory_mirror().get_stats().total_projects, 0);
    }
}
