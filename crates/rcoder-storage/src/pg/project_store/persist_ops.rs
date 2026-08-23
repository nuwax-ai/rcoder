//! 持久化操作模型（write-behind 队列的消息类型）
//!
//! PgStore 在内存镜像更新完成后同步 enqueue 语义化 op（微秒级、非阻塞），
//! 后台 writer task 消费并执行 SQL。非 cfg-gated：op 模型与节流逻辑可独立单测。
//!
//! 丢弃策略（队列深度超阈值时，writer 侧执行）：
//! - **结构性 op**（Upsert*/Remove*/Add*/Clear*/Delete*）：永不丢弃——丢了会造成
//!   PG 与镜像的永久性分叉；FIFO 保序保证同 project 的操作顺序与本地镜像一致。
//! - **Touch*/UpdateAgentStatus**：幂等（重放结果一致），可丢弃，仅影响 idle
//!   判据/状态快照的时间精度（秒级误差可接受）。

use chrono::{DateTime, Utc};
use serde_json::Value;
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};

use crate::adapter::container_entry_key;

/// 单个 project 的持久化快照（whole-row upsert）
#[derive(Debug, Clone)]
pub struct ProjectSnapshot {
    pub project_id: String,
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
            project_id: info.project_id().to_string(),
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
#[derive(Debug, Clone)]
pub struct ContainerSnapshot {
    pub container_name: String,
    pub container_id: Option<String>,
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
    pub fn from_info(key: &str, info: &ContainerBasicInfo, service_type: &ServiceType) -> Self {
        let container_id = if info.container_id.is_empty() {
            None
        } else {
            Some(info.container_id.clone())
        };
        Self {
            container_name: key.to_string(),
            container_id,
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
        }
    }
}

/// write-behind 队列消息
///
/// 两个快照变体装箱（Box）：快照显著大于其余变体，装箱抹平枚举尺寸差
/// （variant_size_differences），队列常驻内存更紧凑。
#[derive(Debug, Clone)]
pub enum PersistOp {
    // ===== 结构性（永不丢弃） =====
    /// project 整行 upsert（含容器关联）。容器行须先于本 op 入队（FK 顺序）。
    UpsertProject(Box<ProjectSnapshot>),
    /// 容器整行 upsert
    UpsertContainer(Box<ContainerSnapshot>),
    /// 删除 project（sessions 经 FK ON DELETE CASCADE 级联）
    RemoveProject { project_id: String },
    /// 登记 session（含冗余 container_name，resolve 单查直达）
    AddSession {
        project_id: String,
        session_id: String,
        container_name: Option<String>,
    },
    /// 移除单个 session
    RemoveSession { session_id: String },
    /// 清空 project 的全部 session
    ClearSessions { project_id: String },
    /// 删除容器及其全部关联 project（唯一物理销毁触发点的持久化侧；
    /// SQL 侧 DELETE projects WHERE container_name IN (...) + DELETE containers）
    DeleteContainerWithProjects { container_id: String },

    // ===== 幂等（超深可丢弃） =====
    /// 刷新 project 活跃时间（节流入队）
    TouchProject {
        project_id: String,
        last_activity: DateTime<Utc>,
    },
    /// 刷新容器活跃时间（节流入队）
    TouchContainer {
        container_name: String,
        last_activity: DateTime<Utc>,
    },
    /// 刷新 session 活跃时间（update_session_activity 节流入队）
    TouchSession {
        session_id: String,
        last_seen_at: DateTime<Utc>,
    },
    /// 更新 agent 状态快照
    UpdateAgentStatus {
        project_id: String,
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
            Self::UpsertProject(_) => "upsert_project",
            Self::UpsertContainer(_) => "upsert_container",
            Self::RemoveProject { .. } => "remove_project",
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

/// 会话创建的结构性 op 集（container + project + session）——单一构造点。
///
/// 此前"与 XX 完全一致"的注释散布 5 处靠人肉同步（入队路径/降级路径/
/// durable 事务体），加字段必漏。所有消费方从这里取：
/// - store_impl::insert_with_session → 逐个入队
/// - durable::insert_with_session_durable → 事务内执行 + 降级入队
pub(in crate::pg) fn structural_ops_for_insert(
    info: &ProjectAndContainerInfo,
    session_id: &str,
) -> anyhow::Result<Vec<PersistOp>> {
    let mut ops = Vec::with_capacity(3);
    if let (Some(basic), Some(st)) = (info.container_info(), info.service_type()) {
        ops.push(PersistOp::UpsertContainer(Box::new(
            ContainerSnapshot::from_info(&container_entry_key(info), &basic, &st),
        )));
    }
    ops.push(PersistOp::UpsertProject(Box::new(
        ProjectSnapshot::from_info(info)?,
    )));
    ops.push(PersistOp::AddSession {
        project_id: info.project_id().to_string(),
        session_id: session_id.to_string(),
        container_name: info.container_info().map(|_| container_entry_key(info)),
    });
    Ok(ops)
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
    fn container_snapshot_maps_basic_info() {
        let info = info_with_container();
        let basic = info.container_info().expect("container");
        let snapshot =
            ContainerSnapshot::from_info("container-1", &basic, &ServiceType::WebAgentRunner);
        assert_eq!(snapshot.container_id.as_deref(), Some("cid-1"));
        assert_eq!(snapshot.internal_port, 50051);
        assert_eq!(snapshot.service_type, "web-agent-runner");
    }

    #[test]
    fn structural_classification() {
        assert!(
            PersistOp::UpsertProject(Box::new(
                ProjectSnapshot::from_info(&info_with_container()).unwrap()
            ))
            .is_structural()
        );
        assert!(
            PersistOp::AddSession {
                project_id: "p".into(),
                session_id: "s".into(),
                container_name: None
            }
            .is_structural()
        );
        assert!(
            !PersistOp::TouchProject {
                project_id: "p".into(),
                last_activity: Utc::now()
            }
            .is_structural()
        );
    }
}
