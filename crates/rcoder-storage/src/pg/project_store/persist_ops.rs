//! 持久化操作模型（write-behind 队列的消息类型）
//!
//! PgStore 在内存镜像更新完成后同步 enqueue 语义化 op（微秒级、非阻塞），
//! 后台 writer task 消费并执行 SQL。非 cfg-gated：op 模型与节流逻辑可独立单测。
//!
//! 丢弃策略（队列深度超阈值时，writer 侧执行）：
//! - **结构性 op**（Upsert*/Remove*/Add*/Clear*/Delete*）：永不丢弃——丢了会造成
//!   PG 与镜像的永久性分叉；队列与直写交错使用代次条件和墓碑拒绝过期操作。
//! - **Touch*/UpdateAgentStatus**：幂等（重放结果一致），可丢弃，仅影响 idle
//!   判据/状态快照的时间精度（秒级误差可接受）。

use chrono::{DateTime, Utc};
use serde_json::Value;
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};

use crate::adapter::container_entry_key;

/// 单个 project 的持久化快照（whole-row upsert）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectSnapshot {
    pub sessions: std::collections::BTreeMap<String, String>,
    pub retired_sessions: std::collections::BTreeMap<String, String>,
    pub expected_revision: i64,
    pub container_generation: Option<String>,
    pub project_id: String,
    pub generation: String,
    pub predecessor: Option<String>,
    pub user_id: Option<String>,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
    /// containers 表键（container_name 或 logical_id 回退占位）
    pub container_name: Option<String>,
    pub latest_session: Option<String>,
    /// ModelProviderConfig JSON（含明文 api_key，运维排查决策）
    pub model_provider: Option<Value>,
    pub request_id: Option<String>,
    /// AgentStatus JSON
    pub agent_status: Option<Value>,
    /// ServiceType 字符串（Display 的 kebab-case，from_str 可逆解析）
    pub service_type: Option<String>,
    pub last_activity: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl ProjectSnapshot {
    /// 从内存镜像的领域对象构造快照。
    ///
    /// # Errors
    /// `service_type` 为 None 时报错（与 ProjectAdapter::insert 的 Fail Fast 一致；
    /// 实际不会发生——insert 已拦截，此处兜底）。
    pub fn from_info(info: &ProjectAndContainerInfo) -> anyhow::Result<Self> {
        let service_type = info
            .service_type()
            .map(|st| st.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "service_type required for snapshot: project_id={}",
                    info.project_id()
                )
            })?;
        Ok(Self {
            sessions: info.persistence_identity().sessions.clone(),
            retired_sessions: info.persistence_identity().retired_sessions.clone(),
            expected_revision: info
                .persistence_identity()
                .revision
                .checked_sub(1)
                .filter(|r| *r >= 0)
                .ok_or_else(|| anyhow::anyhow!("Project snapshot has no registered revision"))?,
            container_generation: info
                .persistence_identity()
                .container
                .as_ref()
                .map(|c| c.generation.clone()),
            project_id: info.project_id().to_string(),
            generation: info.persistence_identity().generation.clone(),
            predecessor: info.persistence_identity().predecessor.clone(),
            user_id: info.user_id().map(str::to_string),
            pod_id: info.pod_id().map(str::to_string),
            tenant_id: info.tenant_id().map(str::to_string),
            space_id: info.space_id().map(str::to_string),
            isolation_type: info.isolation_type().map(str::to_string),
            // 仅真实容器才写关联（FK→containers）；无容器信息的占位 project 置 None，
            // 避免引用不存在的容器行（内存镜像的 logical_id 回退键不落库）
            container_name: info.container_info().map(|_| container_entry_key(info)),
            latest_session: info.latest_session().map(str::to_string),
            model_provider: info
                .model_provider()
                .map(serde_json::to_value)
                .transpose()?,
            request_id: info.request_id().map(str::to_string),
            agent_status: info.status().map(serde_json::to_value).transpose()?,
            service_type: Some(service_type),
            last_activity: info.last_activity(),
            created_at: info.created_at(),
        })
    }
}

/// 单个容器条目的持久化快照（whole-row upsert）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContainerSnapshot {
    pub generation: String,
    pub predecessor: Option<String>,
    pub predecessor_revision: Option<i64>,
    pub expected_revision: i64,
    pub container_name: String,
    pub container_id: Option<String>,
    /// 持久 workload 身份（K8s 控制器 UID；Docker 恒 None）。与 container_id
    /// （Pod UID）一同捕获；换代事务按"同 workload_uid"条件更新。
    pub workload_uid: Option<String>,
    pub logical_id: String,
    /// ServiceType 字符串
    pub service_type: String,
    pub container_ip: String,
    pub internal_port: i32,
    pub external_port: i32,
    pub status: String,
    pub service_url: String,
    pub last_activity: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl ContainerSnapshot {
    /// 从容器基本信息构造快照。
    ///
    /// `key` 为 containers 表键（container_name 优先，占位时为 logical_id，
    /// 与内存镜像的键完全一致）；`service_type` 由调用方提供（project 的
    /// service_type 与容器的同源）。
    pub fn from_info(
        key: &str,
        info: &ContainerBasicInfo,
        service_type: &ServiceType,
        identity: &shared_types::persistence::ContainerPersistenceIdentity,
        workload_uid: Option<&str>,
    ) -> anyhow::Result<Self> {
        let container_id = if info.container_id.is_empty() {
            None
        } else {
            Some(info.container_id.clone())
        };
        anyhow::ensure!(
            identity.physical_uid == container_id,
            "Container snapshot physical identity differs from registration"
        );
        Ok(Self {
            generation: identity.generation.clone(),
            predecessor: identity.predecessor.clone(),
            predecessor_revision: identity.predecessor_revision,
            expected_revision: identity
                .revision
                .checked_sub(1)
                .filter(|r| *r >= 0)
                .ok_or_else(|| anyhow::anyhow!("Container snapshot has no registered revision"))?,
            container_name: key.to_string(),
            container_id,
            workload_uid: workload_uid
                .filter(|uid| !uid.is_empty())
                .map(str::to_string),
            // ContainerBasicInfo.project_id 即容器归属的 project/logical 标识
            logical_id: info.project_id.clone(),
            service_type: service_type.to_string(),
            container_ip: info.container_ip.clone(),
            internal_port: i32::from(info.internal_port),
            external_port: i32::from(info.external_port),
            status: info.status.clone(),
            service_url: info.service_url.clone(),
            // ContainerBasicInfo 无独立活跃时间字段，容器行 last_activity 以
            // 创建时刻为基准（活跃刷新走 touch_container）
            last_activity: info.created_at,
            created_at: info.created_at,
        })
    }
}

/// write-behind 队列消息
///
/// 两个快照变体装箱（Box）：快照显著大于其余变体，装箱抹平枚举尺寸差
/// （variant_size_differences），队列常驻内存更紧凑。
#[derive(Debug, Clone)]
pub enum PersistOp {
    // ===== 结构性（永不丢弃） =====
    /// One queue item and one rollback boundary for the entire registration.
    RegisterProject {
        request_id: String,
        container: Option<Box<ContainerSnapshot>>,
        project: Box<ProjectSnapshot>,
    },
    /// Fault-injection helper; production registrations are indivisible.
    #[cfg(test)]
    UpsertContainer(Box<ContainerSnapshot>),
    /// 删除指定项目代次及其会话；显式退休，不使用级联删除。
    RemoveProject {
        project_id: String,
        generation: String,
    },
    /// Resource cleanup additionally fences physical container replacement.
    RemoveProjectForContainer {
        project_id: String,
        generation: String,
        container_id: String,
        container_name: String,
        container_generation: String,
    },
    /// 登记 session；容器关联由其 project 的代次化外键解析。
    AddSession {
        project_id: String,
        session_id: String,
        project_generation: String,
        generation: String,
        predecessor: Option<String>,
    },
    /// 移除单个 session
    RemoveSession {
        session_id: String,
        generation: String,
    },
    /// 清空 project 的全部 session
    ClearSessions {
        project_id: String,
        generation: String,
        sessions: Vec<(String, String)>,
    },
    /// 删除容器及其全部关联 project（唯一物理销毁触发点的持久化侧；
    /// SQL 侧 DELETE projects WHERE container_name IN (...) + DELETE containers）
    DeleteContainerWithProjects {
        container_id: String,
        /// Container-name/registration-generation pairs captured at admission.
        containers: Vec<(String, String)>,
        projects: Vec<(String, String)>,
    },

    // ===== 幂等（超深可丢弃） =====
    /// 刷新 project 活跃时间（节流入队）
    TouchProject {
        project_id: String,
        generation: String,
        last_activity: DateTime<Utc>,
    },
    /// 刷新容器活跃时间（节流入队）
    TouchContainer {
        container_name: String,
        generation: String,
        last_activity: DateTime<Utc>,
    },
    /// 刷新 session 活跃时间（update_session_activity 节流入队）
    TouchSession {
        session_id: String,
        generation: String,
        project_id: String,
        project_generation: String,
        last_seen_at: DateTime<Utc>,
    },
    /// 更新 agent 状态快照
    UpdateAgentStatus {
        project_id: String,
        generation: String,
        expected_revision: i64,
        agent_status: Value,
    },
}

impl PersistOp {
    /// 是否结构性 op（writer 队列超深时据此决定可否丢弃）
    pub fn is_structural(&self) -> bool {
        !matches!(
            self,
            Self::TouchProject { .. }
                | Self::TouchContainer { .. }
                | Self::TouchSession { .. }
                | Self::UpdateAgentStatus { .. }
        )
    }

    /// 日志用的简短标签
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RegisterProject { .. } => "register_project",
            #[cfg(test)]
            Self::UpsertContainer(_) => "upsert_container",
            Self::RemoveProject { .. } => "remove_project",
            Self::RemoveProjectForContainer { .. } => "remove_project_for_container",
            Self::AddSession { .. } => "add_session",
            Self::RemoveSession { .. } => "remove_session",
            Self::ClearSessions { .. } => "clear_sessions",
            Self::DeleteContainerWithProjects { .. } => "delete_container_with_projects",
            Self::TouchProject { .. } => "touch_project",
            Self::TouchContainer { .. } => "touch_container",
            Self::TouchSession { .. } => "touch_session",
            Self::UpdateAgentStatus { .. } => "update_agent_status",
        }
    }
}

/// Session creation uses one indivisible registration command. The project
/// snapshot includes all session identities; no separately queued session write.
pub(in crate::pg) fn structural_ops_for_insert(
    info: &ProjectAndContainerInfo,
    session_id: &str,
) -> anyhow::Result<Vec<PersistOp>> {
    anyhow::ensure!(
        info.persistence_identity()
            .sessions
            .contains_key(session_id),
        "Session identity missing: {session_id}"
    );
    Ok(vec![registration_for_info(info)?])
}

pub(in crate::pg) fn registration_for_info(
    info: &ProjectAndContainerInfo,
) -> anyhow::Result<PersistOp> {
    let project = Box::new(ProjectSnapshot::from_info(info)?);
    let container = if let (Some(basic), Some(st)) = (info.container_info(), info.service_type()) {
        // workload_uid：注册链捕获侧（K8s ownerReference UID）随契约四解析器
        // 分层接入；Docker 无 workload 对象恒 None。schema 已冻结本列。
        Some(Box::new(ContainerSnapshot::from_info(
            &container_entry_key(info),
            &basic,
            &st,
            info.persistence_identity()
                .container
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Container registration identity missing"))?,
            None,
        )?))
    } else {
        None
    };
    Ok(PersistOp::RegisterProject {
        request_id: uuid::Uuid::new_v4().to_string(),
        container,
        project,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo};

    fn info_with_container() -> ProjectAndContainerInfo {
        let mut info = ProjectAndContainerInfo::new("proj-1".into());
        info.set_service_type(Some(ServiceType::WebAgentRunner));
        info.set_user_id(Some("user-1".into()));
        info.set_container(Some(ContainerBasicInfo {
            container_id: "cid-1".into(),
            container_name: "container-1".into(),
            container_ip: "10.0.0.1".into(),
            internal_port: 50051,
            external_port: 0,
            project_id: "proj-1".into(),
            status: "running".into(),
            created_at: Utc::now(),
            service_url: "http://container-1".into(),
        }));
        let mut identity = info.persistence_identity().clone();
        identity.revision = 1;
        identity.container = Some(shared_types::persistence::ContainerPersistenceIdentity {
            generation: "cg1".into(),
            revision: 1,
            physical_uid: Some("cid-1".into()),
            predecessor: None,
            predecessor_revision: None,
        });
        info.set_persistence_identity(identity);
        info
    }

    #[test]
    fn project_snapshot_captures_all_fields() {
        let info = info_with_container();
        let snapshot = ProjectSnapshot::from_info(&info).expect("snapshot");
        assert_eq!(snapshot.project_id, "proj-1");
        assert_eq!(snapshot.user_id.as_deref(), Some("user-1"));
        assert_eq!(snapshot.container_name.as_deref(), Some("container-1"));
        assert_eq!(snapshot.service_type.as_deref(), Some("web-agent-runner"));
        assert!(snapshot.agent_status.is_none());
    }

    #[test]
    fn project_snapshot_fails_fast_without_service_type() {
        let info = ProjectAndContainerInfo::new("proj-2".into());
        assert!(ProjectSnapshot::from_info(&info).is_err());
    }

    #[test]
    fn snapshots_reject_unregistered_or_mismatched_identity() {
        let mut info = info_with_container();
        let mut identity = info.persistence_identity().clone();
        identity.revision = 0;
        info.set_persistence_identity(identity.clone());
        assert!(ProjectSnapshot::from_info(&info).is_err());
        let basic = info.container_info().unwrap();
        let container = identity.container.as_mut().unwrap();
        container.physical_uid = Some("replacement".into());
        assert!(
            ContainerSnapshot::from_info(
                "container-1",
                &basic,
                &ServiceType::WebAgentRunner,
                container
            )
            .is_err()
        );
        container.physical_uid = Some("cid-1".into());
        container.revision = 0;
        assert!(
            ContainerSnapshot::from_info(
                "container-1",
                &basic,
                &ServiceType::WebAgentRunner,
                container
            )
            .is_err()
        );
    }

    #[test]
    fn cloned_snapshot_preserves_captured_compare_and_swap_revision() {
        let mut info = info_with_container();
        let mut identity = info.persistence_identity().clone();
        identity.revision = 9;
        info.set_persistence_identity(identity);
        let snapshot = ProjectSnapshot::from_info(&info).unwrap();
        assert_eq!(snapshot.expected_revision, 8);
        assert_eq!(snapshot.clone().expected_revision, 8);
    }

    #[test]
    fn container_snapshot_maps_basic_info() {
        let info = info_with_container();
        let basic = info.container_info().expect("container");
        let snapshot = ContainerSnapshot::from_info(
            "container-1",
            &basic,
            &ServiceType::WebAgentRunner,
            info.persistence_identity().container.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.container_id.as_deref(), Some("cid-1"));
        assert_eq!(snapshot.internal_port, 50051);
        assert_eq!(snapshot.service_type, "web-agent-runner");
    }

    #[test]
    fn registration_is_one_structural_queue_item() {
        let mut info = info_with_container();
        info.add_session("session1");
        let ops = structural_ops_for_insert(&info, "session1").unwrap();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].is_structural());
        let PersistOp::RegisterProject {
            container, project, ..
        } = &ops[0]
        else {
            panic!("registration must not be split into independent commands");
        };
        assert!(container.is_some());
        assert_eq!(
            project.sessions["session1"],
            info.persistence_identity().sessions["session1"]
        );
        assert!(structural_ops_for_insert(&info, "missing").is_err());
    }

    #[test]
    fn structural_classification() {
        assert!(
            registration_for_info(&info_with_container())
                .unwrap()
                .is_structural()
        );
        assert!(
            PersistOp::AddSession {
                project_id: "p".into(),
                session_id: "s".into(),
                project_generation: "pg".into(),
                predecessor: None,
                generation: "sg".into(),
            }
            .is_structural()
        );
        assert!(
            !PersistOp::TouchProject {
                project_id: "p".into(),
                generation: "pg".into(),
                last_activity: Utc::now()
            }
            .is_structural()
        );
    }
}
