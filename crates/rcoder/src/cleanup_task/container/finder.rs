//! 容器查找器
//!
//! 根据不同服务类型的规则查找容器

use anyhow::Result;
use docker_manager::DockerContainerInfo;
use shared_types::ServiceType;
use std::sync::Arc;

/// 容器查找器
pub struct ContainerFinder {
    pub docker_manager: Arc<docker_manager::DockerManager>,
}

impl ContainerFinder {
    pub fn new(docker_manager: Arc<docker_manager::DockerManager>) -> Self {
        Self { docker_manager }
    }

    /// 根据容器标识符查找容器
    ///
    /// 对于 RCoder: identifier 是 project_id
    /// 对于 ComputerAgentRunner: identifier 是 user_id
    pub async fn find_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> Result<Option<DockerContainerInfo>> {
        Ok(self
            .docker_manager
            .find_agent_container(identifier, service_type)
            .await)
    }
}
