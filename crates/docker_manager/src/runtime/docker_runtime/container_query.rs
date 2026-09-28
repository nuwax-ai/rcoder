use super::*;

// UserAppDeploymentRuntime 完整实现拆至 docker_app_runtime.rs（与 K8s 侧
// k8s_app_*.rs 文件群对称——Docker 语义映射的 app 域自成一档）。
impl DockerRuntime {
    /// Fetch containers from Docker API (used as cache loader)
    pub(super) async fn fetch_containers(
        &self,
    ) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let containers = self.inner.list_containers().await;
        let mut result = Vec::with_capacity(containers.len());
        for c in containers {
            let container_ip = self
                .inner
                .get_container_connection_info(&c)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?
                .unwrap_or_default();

            // 构建环境变量映射（包含 project_id 和 service_type）+ 结构化身份。
            // 身份真源 = 内存缓存：c.project_id 槽存的是创建时的派生主键
            // （container_identifier 单一事实源），c.service_type 还原语义槽位；
            // rcoder 重启后缓存空 → 此函数无条目，名字反解兜底由消费方处理
            let mut env_vars = HashMap::new();
            // 容器内契约要纯 app_id：builder 场景 project_id 槽是复合串——
            // 缓存条目创建时若带显式 builder_app_id 则直用，否则右切还原
            let project_id_env = c.project_id.clone();
            env_vars.insert("PROJECT_ID".to_string(), project_id_env);
            if let Some(ref user_id) = c.user_id {
                env_vars.insert("USER_ID".to_string(), user_id.clone());
            }
            if let Some(ref service_type) = c.service_type {
                env_vars.insert("SERVICE_TYPE".to_string(), service_type.to_string());
            }
            let slots = c
                .service_type
                .as_ref()
                .map(|st| container_runtime_api::slots_from_identifier(st, &c.project_id))
                .unwrap_or_default();

            result.push(RuntimeContainerInfo {
                container_id: c.container_id,
                container_name: c.container_name,
                container_ip,
                status: map_container_status(&c.status),
                created_at: c.created_at,
                env_vars: Some(env_vars),
                service_type: c.service_type,
                project_id: slots.project_id,
                user_id: slots.user_id,
                pod_id: slots.pod_id,
                app_id: slots.app_id,
                workload_uid: None,
            });
        }
        Ok(result)
    }
}

/// 将内部 `ContainerStatus` 映射为运行时 `ContainerRuntimeStatus`
pub(super) fn map_container_status(
    status: &crate::types::ContainerStatus,
) -> ContainerRuntimeStatus {
    match status {
        crate::types::ContainerStatus::Running => ContainerRuntimeStatus::Running,
        crate::types::ContainerStatus::Stopped => ContainerRuntimeStatus::Failed,
        crate::types::ContainerStatus::Creating => ContainerRuntimeStatus::Pending,
        crate::types::ContainerStatus::Restarting => ContainerRuntimeStatus::Pending,
        crate::types::ContainerStatus::Paused => {
            ContainerRuntimeStatus::Unknown("paused".to_string())
        }
        crate::types::ContainerStatus::Dead => ContainerRuntimeStatus::Failed,
        crate::types::ContainerStatus::Removing => ContainerRuntimeStatus::Failed,
        crate::types::ContainerStatus::Exited => ContainerRuntimeStatus::Failed,
        crate::types::ContainerStatus::Unknown(s) => ContainerRuntimeStatus::Unknown(s.clone()),
    }
}
