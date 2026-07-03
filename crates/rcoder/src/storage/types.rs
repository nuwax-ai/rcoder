//! 存储相关类型定义，适配纯 DashMap 存储。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use shared_types::ServiceType;

/// 存储统计信息
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageStats {
    pub total_containers: usize,
    pub total_projects: usize,
    pub active_sessions: usize,
    pub projects_by_service_type: HashMap<ServiceType, usize>,
}

/// 空闲容器信息（清理任务使用）
#[derive(Debug, Clone)]
pub struct IdleContainerInfo {
    pub container_id: String,
    pub container_name: String,
    pub service_type: ServiceType,
    pub idle_minutes: i64,
    pub project_ids: Vec<String>,
}
