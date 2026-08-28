//! 按标识定位容器：project/user 维度查找 + Agent 连接信息组装。
//!
//! 从 container_query.rs 目录化拆出（extension-impl，方法体原样搬迁）。

use shared_types::ContainerBasicInfo;
use tracing::{debug, warn};

use crate::{
    ContainerQueryResult, ContainerStatus, DockerContainerInfo, DockerError, DockerManager,
    DockerResult,
};

impl DockerManager {
    /// 查找项目容器
    ///
    /// 根据 project_id 和 service_type 查找容器：
    /// - 容器命名规则：`{prefix}-{project_id}`
    /// - RCoder 模式前缀：`rcoder-agent`
    /// - ComputerAgentRunner 模式前缀：`computer-agent-runner`
    ///
    /// # 参数
    /// * `project_id` - 项目 ID
    /// * `service_type` - 服务类型
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(ContainerQueryResult)`
    /// * 如果容器不存在，返回 `None`
    pub async fn find_project_container(
        &self,
        project_id: &str,
        service_type: &shared_types::ServiceType,
    ) -> DockerResult<Option<ContainerQueryResult>> {
        // 1. 查 DashMap 缓存 (如果存在且运行中，通过容器名查询 IP)
        if let Some(info) = self.containers.get(project_id).await {
            // 🎯 验证 service_type 是否匹配
            // 避免 WebAgentRunner 容器被错误地用于 ComputerAgentRunner 请求
            if let Some(ref container_service_type) = info.service_type {
                if container_service_type != service_type {
                    debug!(
                        "[FIND_CONTAINER] Service type mismatch: expected={:?}, found={:?}, container={}, skipping",
                        service_type, container_service_type, info.container_name
                    );
                    // 继续查找，不返回这个容器
                } else {
                    // service_type 匹配，继续检查容器状态
                    let is_running = matches!(info.status, ContainerStatus::Running);
                    if !is_running {
                        return Ok(Some(ContainerQueryResult::new(
                            info.container_id.clone(),
                            info.container_name.clone(),
                            info.status.clone(),
                            false,
                            String::new(),
                            info.created_at,
                        )));
                    }

                    // 容器运行中，通过容器名查询获取 IP（Moka API 缓存优先，miss 时才调 Docker API）
                    // 如果无法获取 IP（容器已被销毁但 DashMap 缓存未清理），标记为非运行状态
                    let (container_ip, effective_status) = match self
                        .find_container_realtime(&info.container_name)
                        .await
                    {
                        Ok(Some(realtime_info)) if !realtime_info.container_ip.is_empty() => {
                            (realtime_info.container_ip, info.status.clone())
                        }
                        Ok(Some(realtime_info)) => {
                            warn!(
                                "[FIND_CONTAINER] Container in DashMap marked Running but has empty IP, treating as stopped: container_name={}, container_id={}",
                                info.container_name, info.container_id
                            );
                            (realtime_info.container_ip, ContainerStatus::Stopped)
                        }
                        _ => {
                            warn!(
                                "[FIND_CONTAINER] Container in DashMap marked Running but not found via Docker API, treating as stopped: container_name={}, container_id={}",
                                info.container_name, info.container_id
                            );
                            (String::new(), ContainerStatus::Stopped)
                        }
                    };

                    let is_running = matches!(effective_status, ContainerStatus::Running);
                    return Ok(Some(ContainerQueryResult::new(
                        info.container_id.clone(),
                        info.container_name.clone(),
                        effective_status,
                        is_running,
                        container_ip,
                        info.created_at,
                    )));
                }
            }
        }

        // 2. 实时查询 Docker API (构造名称)
        // UserApp/UserAppBuilder 短路多镜像配置查询：这两类从不配置 service image
        // （get_service_config 必 Err 且每次深克隆 MultiImageConfig + warn 日志），
        // 前缀直接取 ServiceType 常量——runtime 终端代理每请求走此路径。
        let prefix = match service_type {
            shared_types::ServiceType::UserApp | shared_types::ServiceType::UserAppBuilder => {
                service_type.container_prefix().to_string()
            }
            _ => match self.get_service_config(service_type).await {
                Ok(config) => config.container_prefix().to_string(),
                Err(e) => {
                    warn!(
                        "[FIND_CONTAINER] Failed to get service config, using default prefix: service_type={:?}, error={}",
                        service_type, e
                    );
                    service_type.container_prefix().to_string()
                }
            },
        };
        // 容器名统一走 DockerUtils::generate_container_name（含合法性校验，与创建路径一致）
        let expected_container_name =
            crate::utils::DockerUtils::generate_container_name(&prefix, project_id)
                .map_err(DockerError::ConfigurationError)?;

        // 直接返回 find_container_realtime 的结果
        self.find_container_realtime(&expected_container_name).await
    }

    /// 获取 Agent 容器的高级信息
    ///
    /// 封装了容器查找、IP解析、URL构建和信息转换逻辑
    /// 替代 rcoder 层的手动拼装逻辑
    pub async fn get_agent_info(
        &self,
        project_id: &str,
    ) -> DockerResult<Option<ContainerBasicInfo>> {
        // 1. 查找容器信息（内存映射）
        let container_info = match self.get_container_info(project_id).await {
            Some(info) => info,
            None => return Ok(None),
        };

        // 2. 获取容器 IP (优先使用主网络)
        // 注意：如果容器已被外部删除（如手动 docker rm），此处会出错
        let network_name = self.get_main_network_name().await;
        let network_ips = match self
            .get_container_network_info(&container_info.container_id)
            .await
        {
            Ok(ips) => ips,
            Err(e) => {
                // 检查是否是容器不存在的错误（404 状态码）
                // 容器已被外部删除，清理内存映射并返回 None
                // 这样上层调用者可以重新创建容器
                if matches!(
                    &e,
                    DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    })
                ) {
                    warn!(
                        "[GET_AGENT_INFO] Container was externally deleted (status 404), cleaning up memory mapping: project_id={}, container_id={}",
                        project_id, container_info.container_id
                    );
                    self.containers.remove(project_id).await;
                    return Ok(None);
                }
                // 其他错误正常传播
                return Err(e);
            }
        };

        // 如果网络信息为空，说明容器可能已被删除或未正确连接到网络
        // 清理内存映射并返回 None，让上层调用者重新创建容器
        if network_ips.is_empty() {
            warn!(
                "[GET_AGENT_INFO] Container has no network info (may have been deleted), cleaning up memory mapping: project_id={}, container_id={}",
                project_id, container_info.container_id
            );
            self.containers.remove(project_id).await;
            return Ok(None);
        }

        let container_ip = network_ips
            .get(&network_name)
            .cloned()
            .or_else(|| network_ips.values().next().cloned())
            .ok_or_else(|| {
                DockerError::ConnectionError("Container not connected to any network".to_string())
            })?;

        // 3. 构建服务 URL (Agent 内部默认监听 HTTP_DEFAULT_PORT=8086)
        let server_url = format!(
            "http://{}:{}",
            container_ip,
            shared_types::HTTP_DEFAULT_PORT
        );

        // 4. 转换并返回
        Ok(Some(ContainerBasicInfo {
            container_id: container_info.container_id,
            container_name: container_info.container_name,
            container_ip,
            internal_port: container_info.internal_port,
            external_port: container_info.assigned_port,
            project_id: container_info.project_id,
            status: container_info.status.to_string(),
            created_at: container_info.created_at,
            service_url: server_url,
        }))
    }

    /// 获取容器的连接信息 (IP)
    ///
    /// 用于清理任务获取资源回收所需的信息
    pub async fn get_container_connection_info(
        &self,
        container_info: &DockerContainerInfo,
    ) -> DockerResult<Option<String>> {
        // 1. 获取 IP
        let ip_addr = match self
            .get_container_network_info(&container_info.container_id)
            .await
        {
            Ok(network_ips) => network_ips
                .get(&container_info.network_name)
                .cloned()
                .or_else(|| network_ips.values().next().cloned()),
            Err(e) => {
                warn!("get container ip failed: {}", e);
                None
            }
        };

        Ok(ip_addr)
    }

    // ========================================================================

    // ComputerAgentRunner 专用接口
    // ========================================================================
    //
    // ComputerAgentRunner 模式与 RCoder 模式不同：
    // - 容器命名：computer-agent-runner-{user_id}（而非 project_id）
    // - 一个 user_id 对应一个容器
    // - 容器内可以运行多个 project_id 的 Agent 实例
    //
    // 以下接口专门用于 ComputerAgentRunner 模式，参数名更清晰，
    // 避免与 RCoder 模式的 project_id 参数混淆。

    /// 查找用户容器（ComputerAgentRunner 模式专用）
    ///
    /// 根据 user_id 和 service_type 查找容器：
    /// - 容器命名规则：`{prefix}-{user_id}`
    /// - ComputerAgentRunner 模式前缀：`computer-agent-runner`
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID
    /// * `service_type` - 服务类型（应该是 ComputerAgentRunner）
    ///
    /// # 返回
    /// * `Ok(Some(ContainerQueryResult))` - 容器存在
    /// * `Ok(None)` - 容器不存在
    /// * `Err(...)` - 查询出错
    pub async fn find_user_container(
        &self,
        user_id: &str,
        service_type: &shared_types::ServiceType,
    ) -> DockerResult<Option<ContainerQueryResult>> {
        // 1. 查 Map (如果存在且运行中，直接返回)
        if let Some(info) = self.containers.get(user_id).await {
            return Ok(Some(ContainerQueryResult::new(
                info.container_id.clone(),
                info.container_name.clone(),
                info.status.clone(),
                matches!(info.status, ContainerStatus::Running),
                String::new(), // 缓存命中时 IP 可能已过期，依赖后续实时查询更新
                info.created_at,
            )));
        }

        // 2. 实时查询 Docker API (构造名称)
        // UserApp/UserAppBuilder 短路多镜像配置查询：这两类从不配置 service image
        // （get_service_config 必 Err 且每次深克隆 MultiImageConfig + warn 日志），
        // 前缀直接取 ServiceType 常量——runtime 终端代理每请求走此路径。
        let prefix = match service_type {
            shared_types::ServiceType::UserApp | shared_types::ServiceType::UserAppBuilder => {
                service_type.container_prefix().to_string()
            }
            _ => match self.get_service_config(service_type).await {
                Ok(config) => config.container_prefix().to_string(),
                Err(e) => {
                    warn!(
                        "[FIND_CONTAINER] Failed to get service config, using default prefix: service_type={:?}, error={}",
                        service_type, e
                    );
                    service_type.container_prefix().to_string()
                }
            },
        };
        // 容器名统一走 DockerUtils::generate_container_name（含合法性校验，与创建路径一致）
        let expected_container_name =
            crate::utils::DockerUtils::generate_container_name(&prefix, user_id)
                .map_err(DockerError::ConfigurationError)?;

        // 直接返回 find_container_realtime 的结果
        self.find_container_realtime(&expected_container_name).await
    }
}
