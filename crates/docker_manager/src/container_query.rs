//! Container query operations
//!
//! All container query and inspection methods extracted from DockerManager.
//! This module handles:
//! - Cache lookups (get_container_info, list_containers)
//! - Realtime Docker API queries (find_container_realtime, get_container_info_by_name)
//! - Container status checks (is_container_running, find_project_container)
//! - Network and connection info (get_container_network_info, get_container_connection_info)
//! - Container logs (get_container_logs)
//! - Pattern-based listings (list_containers_with_pattern)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bollard::query_parameters::{InspectContainerOptions, ListContainersOptions, LogsOptions};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use shared_types::ContainerBasicInfo;
use tracing::{debug, error, info, warn};

use crate::{
    ContainerQueryResult, ContainerStatus, DockerContainerInfo, DockerError, DockerManager,
    DockerResult,
};

impl DockerManager {
    // ========================================================================
    // Cache-based queries
    // ========================================================================

    /// 获取容器信息（从缓存）
    ///
    /// 从内存缓存中获取容器信息，速度快但可能不是最新的。
    /// 如果需要最新状态，请使用 `find_container_realtime`。
    ///
    /// # Arguments
    /// * `project_id` - 项目 ID
    ///
    /// # Returns
    /// 容器信息（如果缓存中存在）
    pub async fn get_container_info(&self, project_id: &str) -> Option<DockerContainerInfo> {
        self.containers.get(project_id).await
    }

    /// 清理容器缓存
    ///
    /// 从 DockerManager 的内存缓存中移除容器信息。
    /// 通常在容器被销毁后调用，以保持缓存与实际状态同步。
    pub async fn remove_container_cache(&self, project_id: &str) -> Option<DockerContainerInfo> {
        self.containers.remove(project_id).await
    }

    /// 获取所有容器信息（从缓存）
    pub async fn list_containers(&self) -> Vec<DockerContainerInfo> {
        self.containers.list().await
    }

    // ========================================================================
    // Realtime Docker API queries
    // ========================================================================

    /// 通过多种方式查找容器：project_id 或容器名称
    ///
    /// # ⚠️ 已废弃
    ///
    /// 此方法返回的容器信息可能包含过期的 `container_id`。
    ///
    /// **推荐使用**：
    /// - [`find_container_realtime`](Self::find_container_realtime) - 实时查询 Docker API，获取最新的容器信息和 ID
    ///
    /// **问题**：
    /// - 返回的 `container_id` 可能是缓存中的旧值
    /// - 容器重启后 ID 会变化，导致使用旧 ID 操作失败（404 错误）
    ///
    /// **迁移指南**：
    /// ```text
    /// // ❌ 旧方式（可能使用过期的 container_id）
    /// if let Some(info) = docker_manager.find_container_by_identifier("container_name").await {
    ///     docker_manager.stop_container_by_id(&info.container_id).await?;
    /// }
    ///
    /// // ✅ 新方式（获取最新的 container_id）
    /// if let Ok(Some((container_id, _, _, _))) =
    ///     docker_manager.find_container_realtime("container_name").await
    /// {
    ///     docker_manager.stop_container_by_id(&container_id).await?;
    /// }
    /// ```
    #[deprecated(
        since = "0.1.0",
        note = "返回的 container_id 可能过期。请使用 find_container_realtime() 获取最新的容器信息"
    )]
    pub async fn find_container_by_identifier(
        &self,
        identifier: &str,
    ) -> Option<DockerContainerInfo> {
        // 1. 首先尝试通过 project_id 查找
        if let Some(info) = self.containers.get(identifier).await {
            return Some(info);
        }

        // 2. 如果没找到，尝试通过容器名称查找
        for info in self.containers.list().await {
            if info.container_name == identifier {
                return Some(info);
            }
        }

        // 3. 如果还没找到，尝试通过 Docker API 直接查找容器（适用于容器存在但映射缺失的情况）
        let options = Some(ListContainersOptions {
            all: true,
            ..Default::default()
        });

        if let Ok(containers) = self.docker.list_containers(options).await {
            for container in containers {
                if let Some(names) = container.names {
                    for name in names {
                        // Docker 容器名称通常以 '/' 开头，需要去掉
                        let clean_name = name.trim_start_matches('/');
                        if clean_name == identifier {
                            let container_id = container.id.clone().unwrap_or_default();
                            info!(
                                "Found container via Docker API: {} (ID: {})",
                                identifier, container_id
                            );

                            // 🛡️ 从容器信息中获取真实的创建时间
                            // 使用统一的时间戳解析函数
                            let created_at = if let Some(created_timestamp) = container.created {
                                // list_containers API 返回的是 Unix 秒时间戳
                                Self::parse_unix_timestamp(
                                    created_timestamp,
                                    &format!("container {}", clean_name),
                                )
                                .unwrap_or_else(|e| {
                                    warn!(
                                        "parse container created failed: {}, retry with current time",
                                        e
                                    );
                                    Utc::now()
                                })
                            } else {
                                warn!(
                                    "Container missing creation time info, using current time as fallback: container_id={}",
                                    container_id
                                );
                                Utc::now()
                            };

                            // 创建一个临时的容器信息，用于销毁
                            return Some(DockerContainerInfo {
                                container_id,
                                container_name: clean_name.to_string(),
                                project_id: "unknown".to_string(), // 我们无法直接知道 project_id
                                user_id: None,
                                service_type: None,
                                image: container.image.unwrap_or_default(),
                                status: ContainerStatus::Unknown(
                                    "found_via_docker_api".to_string(),
                                ),
                                created_at,
                                started_at: None,
                                host_path: String::new(),
                                container_path: String::new(),
                                port_bindings: std::collections::HashMap::new(),
                                assigned_port: 0,
                                health_status: None,
                                service_health: None,
                                internal_port: 0,
                                network_name: "unknown".to_string(), // 临时容器信息，网络名称未知
                            });
                        }
                    }
                }
            }
        }

        None
    }

    /// 检查指定ID的容器是否正在运行
    pub async fn is_container_running(&self, container_id: &str) -> DockerResult<bool> {
        match self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                if let Some(state) = details.state
                    && let Some(status) = state.status
                {
                    return Ok(status == bollard::models::ContainerStateStatusEnum::RUNNING);
                }
                Ok(false)
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // 容器不存在，安全地返回 false
                Ok(false)
            }
            Err(e) => {
                // 其他类型的错误，作为错误返回
                Err(DockerError::BollardError(e))
            }
        }
    }

    /// 实时查询容器状态（使用缓存 + 超时保护）
    ///
    /// 与 `find_container_by_identifier` 不同，此方法跳过内存缓存，
    /// 直接查询 Docker API 获取最新的容器状态。
    ///
    /// 🔧 优化：使用 Moka 缓存减少 Docker API 调用，使用超时保护防止阻塞
    /// 📝 缓存策略：同时缓存 container_id 和 container_name，支持 404 响应缓存
    ///
    /// # 参数
    /// * `identifier` - 容器名称或容器 ID
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(ContainerQueryResult)`
    /// * 如果容器不存在，返回 `None`
    pub async fn find_container_realtime(
        &self,
        identifier: &str,
    ) -> DockerResult<Option<ContainerQueryResult>> {
        debug!(
            "[REALTIME] Getting container status: identifier={}",
            identifier
        );

        // 1. 尝试从缓存获取（只缓存成功结果，不缓存 404）
        if let Some(Some(cached)) = self.api_cache.get_status(identifier).await {
            debug!("[REALTIME] cache hit: identifier={}", identifier);
            // Arc::clone 只是增加引用计数，开销很小
            return Ok(Some((*cached).clone()));
        }

        // 2. 缓存未命中，调用 Docker API（带超时）
        let timeout = Duration::from_secs(self.config.api_timeout_quick_seconds);
        let result = match self.inspect_with_timeout(identifier, timeout).await {
            Ok(details) => {
                // 解析结果
                let container_id = details.id.unwrap_or_default();
                let container_name = details
                    .name
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| identifier.to_string());

                let (status, is_running) = if let Some(state) = details.state {
                    if let Some(state_status) = state.status {
                        let is_running =
                            state_status == bollard::models::ContainerStateStatusEnum::RUNNING;
                        let status = ContainerStatus::from(state_status.to_string());
                        (status, is_running)
                    } else {
                        (ContainerStatus::Unknown("no status".to_string()), false)
                    }
                } else {
                    (ContainerStatus::Unknown("no state".to_string()), false)
                };

                // 🔧 获取容器 IP（用于 gRPC 连接）
                let container_ip = match self.get_container_network_info(&container_id).await {
                    Ok(network_ips) => {
                        let network_name = self.get_main_network_name().await;
                        network_ips
                            .get(&network_name)
                            .cloned()
                            .or_else(|| network_ips.values().next().cloned())
                            .unwrap_or_default()
                    }
                    Err(e) => {
                        warn!(
                            "[REALTIME] Failed to get container AP, will retry later: container_id={}, error={}",
                            container_id, e
                        );
                        String::new()
                    }
                };

                // 🔧 解析容器创建时间（从 Docker API 的 RFC3339 字符串）
                let created_at = details
                    .created
                    .as_deref()
                    .and_then(|s| Self::parse_rfc3339_timestamp(s, "realtime_created").ok())
                    .unwrap_or_else(|| {
                        warn!(
                            "[REALTIME] Failed to parse container created time, using current time: container_id={}",
                            container_id
                        );
                        Utc::now()
                    });

                // 🔧 使用 Arc 包装，减少 clone 开销
                let query_result = ContainerQueryResult::new(
                    container_id.clone(),
                    container_name.clone(),
                    status,
                    is_running,
                    container_ip,
                    created_at,
                );
                let result_arc = Arc::new(query_result);

                // 同时用 container_id 和 container_name 作为缓存 key
                // Arc::clone 只是增加引用计数，开销很小
                self.api_cache
                    .insert_status(container_id.clone(), Some(result_arc.clone()))
                    .await;
                self.api_cache
                    .insert_status(container_name.clone(), Some(result_arc.clone()))
                    .await;

                info!(
                    "[REALTIME] Container status query succeeded: id={}, name={}, status={:?}, running={}, ip={}",
                    container_id,
                    container_name,
                    result_arc.status,
                    result_arc.is_running,
                    result_arc.container_ip
                );

                // 返回解引用后的值（因为返回类型不是 Arc）
                Some((*result_arc).clone())
            }
            Err(DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })) => {
                // 🔧 修复：不再缓存 404 响应
                // 原因：容器可能刚被创建，缓存 404 会导致 SSE 连接时序问题
                // 容器状态变化快，404 缓存收益小但风险大
                // 同时清理 network_cache 避免残留旧 IP（容器重建后 IP 可能变化）
                self.api_cache.invalidate(identifier).await;
                debug!(
                    "[REALTIME] Container does not exist (not caching 404): identifier={}",
                    identifier
                );
                None
            }
            Err(DockerError::Timeout(_)) => {
                warn!(
                    "[REALTIME] Query timeout, trying to get from cache: identifier={}",
                    identifier
                );
                // 超时时，尝试返回缓存中的旧值（如果有的话）
                if let Some(Some(cached)) = self.api_cache.get_status(identifier).await {
                    return Ok(Some((*cached).clone()));
                }
                return Err(DockerError::Timeout(format!(
                    "Container status query timeout with no available cache: identifier={}",
                    identifier
                )));
            }
            Err(e) => {
                error!(
                    "[REALTIME] Query container status failed: identifier={}, error={}",
                    identifier, e
                );
                return Err(e);
            }
        };

        Ok(result)
    }

    /// 通过容器名称获取容器创建时间
    ///
    /// 直接查询 Docker API 获取容器的创建时间，不使用缓存。
    /// 主要用于容器保护期检查，确保刚创建的容器不会被误清理。
    ///
    /// # 参数
    /// * `container_name` - 容器名称
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(created_time)`
    /// * 如果容器不存在，返回 `None`
    /// * 如果解析时间失败，返回错误
    ///
    /// # 示例
    /// ```ignore
    /// let created = docker_manager
    ///     .get_container_creation_time_by_name("rcoder-agent-123")
    ///     .await?;
    /// if let Some(time) = created {
    ///     let age = Utc::now().signed_duration_since(time);
    ///     if age.num_seconds() < protection_seconds {
    ///         // 在保护期内，跳过清理
    ///     }
    /// }
    /// ```
    pub async fn get_container_creation_time_by_name(
        &self,
        container_name: &str,
    ) -> DockerResult<Option<DateTime<Utc>>> {
        debug!(
            "[DOCKER_MGR] Querying container creation time: container_name={}",
            container_name
        );

        match self
            .docker
            .inspect_container(container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                if let Some(ref created_str) = details.created {
                    match Self::parse_rfc3339_timestamp(
                        created_str,
                        &format!("container {}", container_name),
                    ) {
                        Ok(created_time_utc) => {
                            debug!(
                                "[DOCKER_MGR] Container creation time: container_name={}, created={}",
                                container_name, created_time_utc
                            );
                            Ok(Some(created_time_utc))
                        }
                        Err(e) => {
                            error!(
                                "[DOCKER_MGR] Failed to parse container creation time: container_name={}, error={}",
                                container_name, e
                            );
                            Err(DockerError::InvalidTimestamp(e))
                        }
                    }
                } else {
                    warn!(
                        "[DOCKER_MGR] Container creation time field is empty: container_name={}",
                        container_name
                    );
                    Ok(None)
                }
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // Container does not exist
                debug!(
                    "[DOCKER_MGR] Container does not exist: container_name={}",
                    container_name
                );
                Ok(None)
            }
            Err(e) => {
                error!(
                    "[DOCKER_MGR] Query container info failed: container_name={}, error={}",
                    container_name, e
                );
                Err(DockerError::BollardError(e))
            }
        }
    }

    /// 解析 RFC3339 时间戳字符串
    ///
    /// 内部辅助函数，统一处理 Docker API 返回的 RFC3339 时间戳解析
    ///
    /// # 参数
    /// * `timestamp_str` - RFC3339 格式的时间戳字符串
    /// * `context` - 上下文描述（用于日志）
    ///
    /// # 返回
    /// * `Ok(DateTime<Utc>)` - 解析成功
    /// * `Err(String)` - 解析失败，返回错误描述
    pub(crate) fn parse_rfc3339_timestamp(
        timestamp_str: &str,
        context: &str,
    ) -> Result<DateTime<Utc>, String> {
        DateTime::parse_from_rfc3339(timestamp_str)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| {
                format!(
                    "Failed to parse RFC3339 timestamp for {}: '{}', error: {}",
                    context, timestamp_str, e
                )
            })
    }

    /// 解析 Unix 秒时间戳
    ///
    /// 内部辅助函数，统一处理 Docker API 返回的 Unix 秒时间戳解析
    /// 用于 `list_containers` API 返回的 created 字段
    ///
    /// # 参数
    /// * `timestamp_secs` - Unix 秒时间戳
    /// * `context` - 上下文描述（用于日志）
    ///
    /// # 返回
    /// * `Ok(DateTime<Utc>)` - 解析成功
    /// * `Err(String)` - 解析失败，返回错误描述
    ///
    /// # 注意
    /// Docker 的 list_containers API 返回的是 Unix **秒**时间戳，不是毫秒
    pub(crate) fn parse_unix_timestamp(
        timestamp_secs: i64,
        context: &str,
    ) -> Result<DateTime<Utc>, String> {
        DateTime::from_timestamp(timestamp_secs, 0).ok_or_else(|| {
            format!(
                "Failed to parse Unix timestamp for {}: {} (out of range)",
                context, timestamp_secs
            )
        })
    }

    /// 通过容器名称从 Docker API 获取完整容器信息
    ///
    /// 直接查询 Docker API 获取最新的容器信息，不使用缓存。
    /// 返回完整的 DockerContainerInfo 结构，包含所有容器元数据。
    ///
    /// # 参数
    /// * `container_name` - 容器名称
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(DockerContainerInfo)`
    /// * 如果容器不存在，返回 `None`
    ///
    /// # 示例
    /// ```ignore
    /// if let Some(info) = docker_manager
    ///     .get_container_info_by_name("rcoder-agent-123")
    ///     .await?
    /// {
    /// println!("containerstatus: {:?}, created message : {}", info.status, info.created_at);
    /// }
    /// ```
    ///
    /// # 与其他方法的对比
    /// - [`get_container_info`](Self::get_container_info): 通过 project_id 从缓存查询（快速但可能过期）
    /// - [`find_container_realtime`](Self::find_container_realtime): 返回简化信息（只有 id/name/status）
    /// - **此方法**: 通过 name 查询完整信息（最新数据）
    pub async fn get_container_info_by_name(
        &self,
        container_name: &str,
    ) -> DockerResult<Option<DockerContainerInfo>> {
        debug!(
            "[DOCKER_MGR] Querying full info by container name: container_name={}",
            container_name
        );

        match self
            .docker
            .inspect_container(container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                // 解析容器 ID
                let container_id = details.id.ok_or_else(|| {
                    DockerError::ConfigurationError("Container ID is empty".to_string())
                })?;

                // 解析容器名称（去除前导斜杠）
                let name = details
                    .name
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| container_name.to_string());

                // 解析状态和启动时间
                let (status, started_at) = if let Some(state) = details.state {
                    let status_str = state
                        .status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    // 使用统一的时间解析函数
                    let started = state
                        .started_at
                        .and_then(|s| Self::parse_rfc3339_timestamp(&s, "started_at").ok());

                    (ContainerStatus::from(status_str), started)
                } else {
                    (ContainerStatus::Unknown("no state".to_string()), None)
                };

                // 解析创建时间 - 使用统一的时间解析函数
                let created_at = details
                    .created
                    .ok_or_else(|| {
                        DockerError::InvalidTimestamp("Container missing created field".to_string())
                    })
                    .and_then(|s| {
                        Self::parse_rfc3339_timestamp(&s, "created")
                            .map_err(DockerError::InvalidTimestamp)
                    })?;

                // 解析镜像
                let image = details
                    .config
                    .as_ref()
                    .and_then(|c| c.image.clone())
                    .unwrap_or_default();

                // 解析挂载信息（查找工作目录绑定）
                let (host_path, container_path) = details
                    .mounts
                    .as_ref()
                    .and_then(|mounts| {
                        mounts.iter().find(|m: &&bollard::models::MountPoint| {
                            m.typ.as_deref() == Some("bind")
                        })
                    })
                    .and_then(|mount| {
                        let source = mount.source.clone()?;
                        let destination = mount.destination.clone()?;
                        Some((source, destination))
                    })
                    .unwrap_or_else(|| (String::new(), String::new()));

                // 解析网络和端口信息
                let (network_name, port_bindings, assigned_port) =
                    if let Some(ref network_settings) = details.network_settings {
                        // 解析网络名称
                        let net_name = network_settings
                            .networks
                            .as_ref()
                            .and_then(|networks| networks.keys().next().cloned())
                            .unwrap_or_default();

                        // 解析端口映射
                        let mut ports = HashMap::new();
                        let mut assigned = 0u16;

                        if let Some(ref port_map) = network_settings.ports {
                            for (container_port, host_bindings) in port_map {
                                if let Some(bindings) = host_bindings {
                                    for binding in bindings {
                                        if let Some(ref host_port) = binding.host_port {
                                            ports.insert(container_port.clone(), host_port.clone());
                                            // 尝试解析为数字端口
                                            if assigned == 0
                                                && let Ok(port) = host_port.parse::<u16>()
                                            {
                                                assigned = port;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        (net_name, ports, assigned)
                    } else {
                        (String::new(), HashMap::new(), 0u16)
                    };

                // 从 Labels 中提取 project_id, user_id, service_type
                let labels = details.config.as_ref().and_then(|c| c.labels.as_ref());
                let project_id = labels
                    .and_then(|l| l.get("project_id"))
                    .cloned()
                    .unwrap_or_default();
                let user_id = labels.and_then(|l| l.get("user_id")).cloned();
                let service_type = labels
                    .and_then(|l| l.get("service_type"))
                    .and_then(|s| s.parse().ok()); // 使用 FromStr trait

                // 内部端口（默认）
                let internal_port = match service_type {
                    Some(shared_types::ServiceType::WebAgentRunner) => {
                        shared_types::GRPC_DEFAULT_PORT
                    }
                    Some(shared_types::ServiceType::ComputerAgentRunner) => {
                        shared_types::HTTP_DEFAULT_PORT
                    }
                    // UserApp 端口不固定，此处仅兜底；实际端口由 app_manager 管理
                    Some(shared_types::ServiceType::UserApp) => shared_types::GRPC_DEFAULT_PORT,
                    None => shared_types::GRPC_DEFAULT_PORT,
                };

                let info = DockerContainerInfo {
                    container_id,
                    container_name: name,
                    project_id,
                    user_id,
                    service_type,
                    image,
                    status,
                    created_at,
                    started_at,
                    host_path,
                    container_path,
                    port_bindings,
                    assigned_port,
                    health_status: None,
                    service_health: None,
                    internal_port,
                    network_name,
                };

                debug!(
                    "[DOCKER_MGR] Container info query succeeded: name={}, id={}, status={:?}",
                    info.container_name, info.container_id, info.status
                );

                Ok(Some(info))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // Container does not exist
                debug!(
                    "[DOCKER_MGR] Container does not exist: container_name={}",
                    container_name
                );
                Ok(None)
            }
            Err(e) => {
                error!(
                    "[DOCKER_MGR] Query container info failed: container_name={}, error={}",
                    container_name, e
                );
                Err(DockerError::BollardError(e))
            }
        }
    }

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
        // 使用 service_config.container_prefix() 获取配置的前缀
        let prefix = match self.get_service_config(service_type).await {
            Ok(config) => config.container_prefix().to_string(),
            Err(e) => {
                warn!(
                    "[FIND_CONTAINER] Failed to get service config, using default prefix: service_type={:?}, error={}",
                    service_type, e
                );
                service_type.container_prefix().to_string()
            }
        };
        let expected_container_name = format!("{}-{}", prefix, project_id);

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
        let server_url = format!("http://{}:{}", container_ip, shared_types::HTTP_DEFAULT_PORT);

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

    /// 获取用户容器信息（ComputerAgentRunner 模式专用）
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID，用作容器标识符
    ///
    /// # 说明
    /// - ComputerAgentRunner 模式下，一个用户对应一个容器
    /// - 容器命名规则：`computer-agent-runner-{user_id}`
    /// - 容器内可以运行多个 project_id 的 Agent 实例
    ///
    /// # 返回
    /// 容器信息（如果存在），否则返回 None
    pub async fn get_user_container_info(
        &self,
        user_id: &str,
    ) -> DockerResult<Option<ContainerBasicInfo>> {
        // 内部调用 get_agent_info，但参数名更清晰
        self.get_agent_info(user_id).await
    }

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
        // 使用 service_config.container_prefix() 获取配置的前缀
        let prefix = match self.get_service_config(service_type).await {
            Ok(config) => config.container_prefix().to_string(),
            Err(e) => {
                warn!(
                    "[FIND_CONTAINER] Failed to get service config, using default prefix: service_type={:?}, error={}",
                    service_type, e
                );
                service_type.container_prefix().to_string()
            }
        };
        let expected_container_name = format!("{}-{}", prefix, user_id);

        // 直接返回 find_container_realtime 的结果
        self.find_container_realtime(&expected_container_name).await
    }

    /// 通过用户 ID 获取容器 ID（ComputerAgentRunner 模式专用）
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID
    ///
    /// # 返回
    /// 容器 ID（如果存在），否则返回 None
    pub async fn get_user_container_id(&self, user_id: &str) -> DockerResult<Option<String>> {
        // 从容器信息中获取 container_id
        Ok(self
            .get_container_info(user_id)
            .await
            .map(|info| info.container_id))
    }

    /// 检查用户容器是否存在（ComputerAgentRunner 模式专用）
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID
    ///
    /// # 返回
    /// true 如果容器存在且运行中，否则返回 false
    pub async fn is_user_container_running(&self, user_id: &str) -> bool {
        match self
            .find_user_container(user_id, &shared_types::ServiceType::ComputerAgentRunner)
            .await
        {
            Ok(Some(result)) => result.is_running,
            _ => false,
        }
    }

    /// 列出匹配指定模式的容器
    ///
    /// 使用 Docker API 列出所有容器（包括停止的），并根据名称模式过滤。
    /// 自动排除当前容器自身（通过 HOSTNAME 环境变量识别）。
    ///
    /// # Arguments
    /// * `pattern` - 容器名称匹配模式
    ///
    /// # Returns
    /// 匹配的容器列表
    pub async fn list_containers_with_pattern(
        &self,
        pattern: &str,
    ) -> DockerResult<Vec<bollard::models::ContainerSummary>> {
        info!("Listing containers: pattern={}", pattern);

        // 🎯 获取当前容器的 ID（用于排除自己）
        let current_container_id = std::env::var("HOSTNAME").ok();

        // 使用 Docker API 列出所有容器（包括停止的）
        let options = Some(bollard::query_parameters::ListContainersOptions {
            all: true,
            ..Default::default()
        });

        let containers = self.docker.list_containers(options).await.map_err(|e| {
            DockerError::ConnectionError(format!("failed to get container list: {}", e))
        })?;

        // 创建过滤器
        let filter = crate::ContainerFilter::name_pattern(pattern);

        // 过滤容器，排除当前容器自己
        let matched_containers: Vec<bollard::models::ContainerSummary> = containers
            .clone()
            .into_iter()
            .filter(|container| {
                // 排除当前容器自己
                if let Some(ref current_id) = current_container_id
                    && let Some(ref container_id) = container.id
                {
                    // HOSTNAME 是容器 ID 的前 12 位
                    if container_id.starts_with(current_id) {
                        info!("skip removing container: {}", container_id);
                        return false;
                    }
                }
                filter.matches(container)
            })
            .collect();

        info!(
            "Container lookup completed: total={}, matched={} (self excluded), pattern={}",
            containers.len(),
            matched_containers.len(),
            pattern
        );

        Ok(matched_containers)
    }

    /// 获取容器日志
    ///
    /// # Arguments
    /// * `project_id` - 项目 ID
    /// * `lines` - 获取最后 N 行日志
    ///
    /// # Returns
    /// 容器日志字符串
    pub async fn get_container_logs(&self, project_id: &str, lines: i64) -> DockerResult<String> {
        let container_info = if let Some(info) = self.containers.get(project_id).await {
            info
        } else {
            return Err(DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("project {} has no corresponding container", project_id),
            )));
        };

        let log_options = LogsOptions {
            stdout: true,
            stderr: true,
            tail: lines.to_string(),
            timestamps: true,
            ..Default::default()
        };

        let mut log_stream = self
            .docker
            .logs(&container_info.container_id, Some(log_options));
        let mut logs = String::new();

        while let Some(result) = log_stream.next().await {
            match result {
                Ok(output) => {
                    logs.push_str(&String::from_utf8_lossy(&output.into_bytes()));
                }
                Err(e) => {
                    warn!("get container logs failed: {}", e);
                }
            }
        }

        Ok(logs)
    }
}
