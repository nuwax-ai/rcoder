//! Container cleanup operations
//!
//! All container cleanup/removal methods extracted from DockerManager (manager.rs).
//! This module handles:
//! - Full cache cleanup (cleanup_all_containers)
//! - Batch removal by IDs (stop_and_remove_containers_by_ids)
//! - Pattern-based cleanup (cleanup_containers_with_pattern)
//! - Single-container stop/remove helpers (graceful_stop_container, force_stop_container,
//!   remove_single_container, inspect_container_for_cleanup)
//! - Internal mapping cleanup (cleanup_internal_mappings)

use std::time::Instant;

use anyhow::Result;
use bollard::models::ContainerSummary;
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptions, RemoveContainerOptions, StopContainerOptions,
};
use tracing::{debug, error, info, warn};

use crate::{
    CleanupOptions, CleanupResult, ContainerRemovalFailure, ContainerStatus, DockerError,
    DockerManager, DockerResult,
};

/// 已停止的 agent 容器保留多久再回收（默认 10 分钟）。
///
/// 保留窗口的意义：agent 容器不再 `auto_remove`，尸体让
/// `/computer/pod/health` 能直接 `inspect` 到 `State.OOMKilled / ExitCode /
/// FinishedAt`——比 Docker events（只回放最近 256 条，繁忙 daemon 上仅覆盖
/// 几分钟）精确得多。到期由 60s 对账任务回收，不长期占磁盘。
///
/// 可用 `RCODER_STOPPED_AGENT_RETENTION_SECS` 覆盖（运维调优/测试）。
const STOPPED_AGENT_RETENTION_SECS: i64 = 600;

fn stopped_agent_retention_secs() -> i64 {
    std::env::var("RCODER_STOPPED_AGENT_RETENTION_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v >= 0)
        .unwrap_or(STOPPED_AGENT_RETENTION_SECS)
}

impl DockerManager {
    // ========================================================================
    // Cleanup entry points
    // ========================================================================

    /// 回收「已停止超过保留窗口」的 agent 容器。
    ///
    /// **直接扫 Docker 真实容器列表**，不经过任何注册表：agent 容器注册在
    /// rcoder-engine 的 projects 表，而 `DockerManager::containers` 是另一套
    /// 缓存——挂在后者上的回收对 agent 容器永远不会触发（实测确认）。
    /// 直接 list 还能覆盖"从所有缓存里消失但 Docker 里还在"的孤儿。
    ///
    /// 判据（缺一不可）：
    /// - 带 `service-type` label（只看 rcoder 管理的 agent 容器，不碰 userapp）；
    /// - 状态已停止（exited/dead/created，非 running/restarting/paused）；
    /// - `State.FinishedAt` 有效且距今超过保留窗口（零值一律跳过）。
    ///
    /// 返回实际回收数量。
    pub async fn reap_stopped_agents(&self) -> DockerResult<u32> {
        let retention = stopped_agent_retention_secs();
        let listed = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(std::collections::HashMap::from([(
                    "label".into(),
                    vec!["service-type".to_string()],
                )])),
                ..Default::default()
            }))
            .await
            .map_err(|e| DockerError::ConnectionError(format!("list containers for reap: {e}")))?;

        let mut reclaimed = 0u32;
        for container in listed {
            let Some(id) = container.id else { continue };
            // list 的 State 是字符串："exited" / "dead" / "created" / ...
            let state = container.state.map(|s| s.to_string().to_lowercase());
            let terminal = matches!(
                state.as_deref(),
                Some("exited") | Some("dead") | Some("created")
            );
            if !terminal {
                continue;
            }
            // FinishedAt 只在 inspect 里有
            let details = match self
                .docker
                .inspect_container(&id, None::<InspectContainerOptions>)
                .await
            {
                Ok(details) => details,
                Err(e) => {
                    debug!(container_id = %id, error = %e, "reap: inspect failed, skip");
                    continue;
                }
            };
            let finished_at = details
                .state
                .as_ref()
                .and_then(|s| s.finished_at.as_ref())
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t.to_string()).ok())
                .map(|t| t.timestamp())
                .filter(|ts| *ts > 0);
            let Some(finished_at) = finished_at else {
                continue;
            };
            let age = chrono::Utc::now().timestamp() - finished_at;
            if age < retention {
                continue;
            }
            let name = container
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| id.clone());
            info!(
                container_id = %id, container = %name, age_seconds = age,
                retention_secs = retention,
                "reclaiming stopped agent container past retention window"
            );
            match self
                .docker
                .remove_container(&id, None::<RemoveContainerOptions>)
                .await
            {
                Ok(()) => reclaimed += 1,
                Err(e) => warn!(container_id = %id, error = %e, "reap: remove failed"),
            }
        }
        if reclaimed > 0 {
            info!(reclaimed, "stopped agent containers reclaimed");
        }
        Ok(reclaimed)
    }

    /// 清理所有容器
    pub async fn cleanup_all_containers(&self) -> DockerResult<()> {
        info!("Starting cleanup of all containers");

        let project_ids: Vec<String> = self.containers.keys().await;

        for project_id in project_ids {
            if let Err(e) = self.stop_container(&project_id).await {
                error!("cleanup project {} container failed: {}", project_id, e);
            }
        }

        info!("Container cleanup completed");
        Ok(())
    }

    /// 批量停止并删除指定的容器
    ///
    /// # Arguments
    /// * `container_ids` - 要删除的容器ID列表
    /// * `options` - 清理选项
    ///
    /// # Returns
    /// 返回清理操作结果统计
    pub async fn stop_and_remove_containers_by_ids(
        &self,
        container_ids: Vec<String>,
        options: CleanupOptions,
    ) -> DockerResult<CleanupResult> {
        info!("Starting cleanup container: count={}", container_ids.len());

        let start_time = Instant::now();
        let mut result = CleanupResult {
            total_found: container_ids.len(),
            ..Default::default()
        };

        for container_id in &container_ids {
            match self
                .stop_and_remove_single_container(container_id, &options)
                .await
            {
                Ok(_) => {
                    result.successfully_removed += 1;
                    result.removed_container_ids.push(container_id.clone());
                    info!("Container cleanup succeeded: {}", container_id);
                }
                Err(e) => {
                    result.failed_removals += 1;
                    result
                        .failed_removals_details
                        .push(ContainerRemovalFailure {
                            container_id: container_id.clone(),
                            container_name: container_id.clone(), // 我们可能不知道名称，使用ID
                            error_message: e.to_string(),
                        });
                    error!("Container cleanup failed: {} - {}", container_id, e);
                }
            }
        }

        result.duration_ms = start_time.elapsed().as_millis().min(u64::MAX as u128) as u64;

        info!(
            "Batch container cleanup completed: total={}, success={}, failed={}, duration={}ms",
            result.total_found,
            result.successfully_removed,
            result.failed_removals,
            result.duration_ms
        );

        Ok(result)
    }

    /// 使用模式匹配清理容器（主要接口）
    ///
    /// # Arguments
    /// * `pattern` - 容器名称模式（如 "rcoder-agent-*"）
    /// * `options` - 清理选项
    ///
    /// # Returns
    /// 返回清理结果统计
    pub async fn cleanup_containers_with_pattern(
        &self,
        pattern: &str,
        options: CleanupOptions,
    ) -> DockerResult<CleanupResult> {
        info!("Starting cleanup container: pattern={:?}", pattern);

        // 第一步：查找匹配的容器
        let matched_containers = self.list_containers_with_pattern(pattern).await?;

        // 第二步：提取容器ID
        let container_ids: Vec<String> = matched_containers
            .iter()
            .filter_map(|container| container.id.as_ref())
            .cloned()
            .collect();

        info!(
            "Found {} matching containers: pattern={}",
            container_ids.len(),
            pattern
        );

        // 第三步：批量清理
        let result = self
            .stop_and_remove_containers_by_ids(container_ids, options)
            .await;

        // 第四步：从内部映射中移除已清理的容器
        self.cleanup_internal_mappings(&matched_containers).await;

        result
    }

    // ========================================================================
    // Private helpers
    // ========================================================================

    /// 停止并删除单个容器
    async fn stop_and_remove_single_container(
        &self,
        container_id: &str,
        options: &CleanupOptions,
    ) -> DockerResult<()> {
        info!("Cleaning up container: {}", container_id);

        // 第一步：获取容器信息
        let container_info = self.inspect_container_for_cleanup(container_id).await?;

        // 第二步：检查容器状态并决定是否需要停止
        match container_info
            .state
            .as_ref()
            .and_then(|s| s.status.as_ref())
        {
            // 统一走 ContainerStatus 枚举比较（大小写不敏感），不直接比字符串
            Some(status) if ContainerStatus::from(status.to_string()).is_running() => {
                if !options.force_remove_running {
                    info!("Container {} is running, skip (force=false)", container_id);
                    return Ok(());
                }

                if options.wait_for_graceful_stop {
                    info!("Gracefully stopped container: {}", container_id);
                    if let Err(e) = self
                        .graceful_stop_container(container_id, options.stop_timeout_seconds)
                        .await
                    {
                        warn!(
                            "graceful stop failed, force stopped: {} - {}",
                            container_id, e
                        );
                        // 强制停止
                        self.force_stop_container(container_id).await?;
                    }
                } else {
                    // 直接强制停止
                    self.force_stop_container(container_id).await?;
                }
            }
            Some(_) => {
                info!("Container {} is not running", container_id);
            }
            None => {
                warn!("Unable to get container {} status", container_id);
            }
        }

        // 第三步：删除容器
        self.remove_single_container(container_id, options.remove_associated_volumes)
            .await?;

        info!("containercleanupcompleted: {}", container_id);
        Ok(())
    }

    /// 获取容器信息用于清理
    async fn inspect_container_for_cleanup(
        &self,
        container_id: &str,
    ) -> Result<bollard::models::ContainerInspectResponse, DockerError> {
        let options = Some(InspectContainerOptions { size: false });

        self.docker
            .inspect_container(container_id, options)
            .await
            .map_err(|e| {
                DockerError::ConnectionError(format!("failed to get container info: {}", e))
            })
    }

    /// 优雅停止容器
    async fn graceful_stop_container(
        &self,
        container_id: &str,
        timeout_seconds: u64,
    ) -> DockerResult<()> {
        let stop_options = Some(StopContainerOptions {
            t: Some(timeout_seconds as i32),
            signal: None::<String>,
        });

        self.docker
            .stop_container(container_id, stop_options)
            .await
            .map_err(|e| {
                DockerError::ContainerStopError(format!(
                    "failed to gracefully stop container: {}",
                    e
                ))
            })
    }

    /// 强制停止容器
    async fn force_stop_container(&self, container_id: &str) -> DockerResult<()> {
        let stop_options = Some(StopContainerOptions {
            t: None::<i32>,
            signal: None::<String>,
        });

        self.docker
            .stop_container(container_id, stop_options)
            .await
            .map_err(|e| {
                DockerError::ContainerStopError(format!("failed to force stop container: {}", e))
            })
    }

    /// 删除单个容器
    async fn remove_single_container(
        &self,
        container_id: &str,
        remove_volumes: bool,
    ) -> DockerResult<()> {
        let remove_options = Some(RemoveContainerOptions {
            force: true,
            v: remove_volumes,
            ..Default::default()
        });

        self.docker
            .remove_container(container_id, remove_options)
            .await
            .map_err(|e| {
                DockerError::ContainerRemoveError(format!("failed to delete container: {}", e))
            })
    }

    /// 从内部映射中清理已删除的容器
    async fn cleanup_internal_mappings(&self, removed_containers: &[ContainerSummary]) {
        for container in removed_containers {
            if let Some(container_id) = &container.id {
                // 单次 actor 往返按 container_id 移除所有匹配条目（替代 list()+逐个
                // remove_if_container_id 的 O(n²)）；仍按 container_id 精确匹配，保留
                // "防误删重启新容器"语义（重启的新容器 container_id 不同，不被匹配）。
                let removed = self
                    .containers
                    .remove_all_by_container_id(container_id)
                    .await;
                for info in &removed {
                    info!(
                        "Removed from internal mapping: project_id={}, container_id={}",
                        info.project_id, container_id
                    );
                }
            }
        }
    }
}
