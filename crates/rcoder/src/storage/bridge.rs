//! 数据转换桥接器
//!
//! 提供 DuckDB 记录和 shared_types 结构之间的转换

use chrono::Utc;
use duckdb_manager::{ContainerRecord, ProjectRecord};
use shared_types::{
    AgentStatus, ContainerBasicInfo, ModelProviderConfig, ProjectAndContainerInfo, ServiceType,
};

/// 数据桥接器
///
/// 提供 DuckDB 记录和应用层结构之间的转换方法
pub struct DataBridge;

impl DataBridge {
    // ========== ProjectRecord <-> ProjectAndContainerInfo 转换 ==========

    /// 将 ProjectRecord 转换为 ProjectAndContainerInfo
    pub fn project_record_to_info(
        record: &ProjectRecord,
        container: Option<ContainerBasicInfo>,
    ) -> ProjectAndContainerInfo {
        let mut info = ProjectAndContainerInfo::new(record.project_id.clone());

        // 设置基本信息
        info.set_session_id(record.session_id.clone());
        info.set_user_id(record.user_id.clone());
        info.set_service_type(Some(record.service_type.clone()));
        info.set_container(container);

        // 设置 Agent 状态
        if let (Some(code), Some(name)) = (&record.agent_status_code, &record.agent_status_name) {
            let status = Self::code_to_agent_status(*code, name);
            info.set_status(Some(status));
        }

        // 设置请求ID
        info.set_request_id(record.request_id.clone());

        // 设置模型提供商配置
        if let Some(json) = &record.model_provider_json {
            if let Ok(config) = serde_json::from_str::<ModelProviderConfig>(json) {
                info.set_model_provider(Some(config));
            }
        }

        info
    }

    /// 将 ProjectAndContainerInfo 转换为 ProjectRecord
    pub fn info_to_project_record(
        info: &ProjectAndContainerInfo,
        project_id: &str,
    ) -> ProjectRecord {
        let service_type = info.service_type().unwrap_or(ServiceType::RCoder);
        let container_id = info
            .container()
            .map(|c| c.container_id.clone())
            .unwrap_or_default();

        let mut record = ProjectRecord::new(
            project_id.to_string(),
            service_type,
            container_id,
        );

        // 设置会话信息
        record.session_id = info.session_id().map(|s| s.to_string());

        // 设置用户ID
        record.user_id = info.user_id().map(|s| s.to_string());

        // 设置 Agent 状态
        if let Some(status) = info.status() {
            let (code, name) = Self::agent_status_to_code(status);
            record.agent_status_code = Some(code);
            record.agent_status_name = Some(name);
        }

        // 设置请求ID
        record.request_id = info.request_id().map(|s| s.to_string());

        // 设置模型提供商配置
        if let Some(config) = info.model_provider() {
            if let Ok(json) = serde_json::to_string(config) {
                record.model_provider_json = Some(json);
            }
        }

        // 设置时间戳
        record.created_at = info.created_at();
        record.last_activity = info.last_activity();

        // 如果有会话，设置会话时间
        if info.session_id().is_some() {
            record.session_created_at = Some(info.created_at());
            record.session_last_activity = Some(info.last_activity());
        }

        record
    }

    // ========== ContainerRecord <-> ContainerBasicInfo 转换 ==========

    /// 将 ContainerRecord 转换为 ContainerBasicInfo
    pub fn container_record_to_info(record: &ContainerRecord) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: record.container_id.clone(),
            container_name: record.container_name.clone(),
            container_ip: record.container_ip.clone(),
            internal_port: record.internal_port,
            external_port: record.external_port,
            project_id: String::new(), // 从关联的项目获取
            status: record.status.clone(),
            created_at: record.created_at,
            service_url: record.service_url.clone(),
        }
    }

    /// 将 ContainerBasicInfo 转换为 ContainerRecord
    pub fn container_info_to_record(
        info: &ContainerBasicInfo,
        service_type: Option<ServiceType>,
    ) -> ContainerRecord {
        ContainerRecord::new(
            info.container_id.clone(),
            info.container_name.clone(),
            info.container_ip.clone(),
            info.internal_port,
            info.external_port,
            service_type.unwrap_or(ServiceType::RCoder),
            info.status.clone(),
            info.service_url.clone(),
        )
    }

    // ========== AgentStatus <-> 状态码/名称 转换 ==========

    /// 将 AgentStatus 转换为状态码和名称
    pub fn agent_status_to_code(status: &AgentStatus) -> (i32, String) {
        match status {
            AgentStatus::Idle => (0, "idle".to_string()),
            AgentStatus::Active => (1, "active".to_string()),
            AgentStatus::Terminating => (2, "terminating".to_string()),
        }
    }

    /// 将状态码和名称转换为 AgentStatus
    pub fn code_to_agent_status(code: i32, _name: &str) -> AgentStatus {
        match code {
            0 => AgentStatus::Idle,
            1 => AgentStatus::Active,
            2 => AgentStatus::Terminating,
            _ => AgentStatus::Idle, // 默认
        }
    }

    // ========== 辅助方法 ==========

    /// 创建项目记录并关联会话
    pub fn create_project_with_session(
        project_id: &str,
        session_id: &str,
        service_type: ServiceType,
        container_id: &str,
    ) -> ProjectRecord {
        let now = Utc::now();
        let mut record = ProjectRecord::new(
            project_id.to_string(),
            service_type,
            container_id.to_string(),
        );
        record.session_id = Some(session_id.to_string());
        record.session_created_at = Some(now);
        record.session_last_activity = Some(now);
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_status_conversion() {
        // 测试各种状态的转换
        let statuses = vec![
            AgentStatus::Idle,
            AgentStatus::Active,
            AgentStatus::Terminating,
        ];

        for status in statuses {
            let (code, name) = DataBridge::agent_status_to_code(&status);
            let converted = DataBridge::code_to_agent_status(code, &name);
            assert_eq!(
                std::mem::discriminant(&status),
                std::mem::discriminant(&converted)
            );
        }
    }

    #[test]
    fn test_container_info_conversion() {
        let info = ContainerBasicInfo {
            container_id: "c1".to_string(),
            container_name: "test-container".to_string(),
            container_ip: "192.168.1.100".to_string(),
            internal_port: 8080,
            external_port: 9090,
            project_id: "p1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://localhost:9090".to_string(),
        };

        let record = DataBridge::container_info_to_record(&info, Some(ServiceType::RCoder));

        assert_eq!(record.container_id, info.container_id);
        assert_eq!(record.container_name, info.container_name);
        assert_eq!(record.container_ip, info.container_ip);
        assert_eq!(record.internal_port, info.internal_port);
        assert_eq!(record.external_port, info.external_port);

        let converted = DataBridge::container_record_to_info(&record);

        assert_eq!(converted.container_id, info.container_id);
        assert_eq!(converted.container_name, info.container_name);
    }

    #[test]
    fn test_project_record_conversion() {
        let mut info = ProjectAndContainerInfo::new("test-project".to_string());
        info.set_service_type(Some(ServiceType::RCoder));
        info.set_session_id(Some("session-1".to_string()));
        info.set_status(Some(AgentStatus::Active));

        let record = DataBridge::info_to_project_record(&info, "test-project");

        assert_eq!(record.project_id, "test-project");
        assert_eq!(record.session_id, Some("session-1".to_string()));
        assert_eq!(record.agent_status_code, Some(1));
        assert_eq!(record.agent_status_name, Some("active".to_string()));
    }
}
