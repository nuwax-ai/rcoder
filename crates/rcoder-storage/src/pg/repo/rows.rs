//! 数据访问层的行类型（FromRow，自 load.rs 迁入）
//!
//! 仅描述表结构映射；领域对象（ProjectAndContainerInfo 等）的重建在业务层
//! （`pg::load`）完成——行类型不泄漏到 repo 之外。

/// containers 表行
#[derive(sqlx::FromRow)]
pub(in crate::pg) struct ContainerRow {
    pub container_name: String,
    pub container_id: Option<String>,
    pub logical_id: String,
    #[allow(dead_code)] // 预留：按 service_type 过滤容器
    pub service_type: String,
    pub container_ip: String,
    pub internal_port: i32,
    pub external_port: i32,
    pub status: String,
    pub service_url: String,
    #[allow(dead_code)]
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// projects 表行
#[derive(sqlx::FromRow)]
pub(in crate::pg) struct ProjectRow {
    pub project_id: String,
    pub user_id: Option<String>,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
    pub container_name: Option<String>,
    pub latest_session: Option<String>,
    pub model_provider: Option<serde_json::Value>,
    pub request_id: Option<String>,
    pub agent_status: Option<serde_json::Value>,
    pub service_type: Option<String>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// sessions 表行
#[derive(sqlx::FromRow)]
pub(in crate::pg) struct SessionRow {
    pub session_id: String,
    pub project_id: String,
    #[allow(dead_code)]
    pub container_name: Option<String>,
}
