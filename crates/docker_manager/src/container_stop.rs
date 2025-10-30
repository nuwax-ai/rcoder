//! 统一的容器停止模块
//!
//! 提供两种容器停止策略：
//! 1. 启动时清理（startup_cleanup）：用于服务启动时清理遗留容器
//!    - 使用5秒超时
//!    - 过滤409冲突错误（容器已在删除中）
//!    - 不阻塞服务启动
//!
//! 2. 运行时清理（runtime_cleanup）：用于运行时快速清理容器
//!    - 使用3秒优雅停止超时
//!    - 超时后立即强制停止
//!    - 快速释放资源
//!
//! # 使用示例
//!
//! ```rust,no_run
//! use docker_manager::container_stop;
//!
//! // 启动时清理
//! let result = container_stop::startup_cleanup_containers(
//!     &docker_manager,
//!     "rcoder-agent-*"
//! ).await?;
//!
//! // 运行时清理单个容器
//! container_stop::runtime_cleanup_container(
//!     &docker_manager,
//!     "container_id_123"
//! ).await?;
//! ```

use crate::{CleanupResult, ContainerRemovalFailure, DockerError, DockerManager, DockerResult};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

/// 启动清理超时时间（秒）
/// 
/// 启动时使用较短的超时时间，快速清理遗留容器
const STARTUP_CLEANUP_TIMEOUT_SECONDS: u64 = 5;

/// 运行时清理超时时间（秒）
///
/// 运行时给容器3秒优雅退出时间，然后强制停止
const RUNTIME_CLEANUP_TIMEOUT_SECONDS: u64 = 3;

/// 容器停止后的等待时间（毫秒）
///
/// 给Docker一些时间完成清理操作
const POST_STOP_WAIT_MS: u64 = 100;

/// 启动时容器清理策略
///
/// 用于服务启动时清理遗留的容器。此函数会：
/// - 查找匹配指定模式的所有容器
/// - 🚀 并发停止所有容器（提高清理速度）
/// - 使用5秒超时停止每个容器
/// - 过滤409冲突错误（容器已在删除中）
/// - 返回详细的清理统计信息
///
/// # Arguments
///
/// * `docker_manager` - Docker管理器实例
/// * `pattern` - 容器名称匹配模式（如 "rcoder-agent-*"）
///
/// # Returns
///
/// 返回 `CleanupResult` 包含清理统计信息
///
/// # Examples
///
/// ```rust,no_run
/// use docker_manager::container_stop;
///
/// let result = container_stop::startup_cleanup_containers(
///     &docker_manager,
///     "rcoder-agent-*"
/// ).await?;
///
/// info!("清理了 {} 个容器", result.successfully_removed);
/// ```
pub async fn startup_cleanup_containers(
    docker_manager: &Arc<DockerManager>,
    pattern: &str,
) -> DockerResult<CleanupResult> {
    info!("🧹 [STARTUP_CLEANUP] 开始清理容器: pattern={}", pattern);
    let start_time = Instant::now();

    // 查找匹配模式的容器
    let matched_containers = docker_manager.list_containers_with_pattern(pattern).await?;
    
    let total_found = matched_containers.len();
    info!("🔍 [STARTUP_CLEANUP] 找到 {} 个匹配的容器", total_found);

    if total_found == 0 {
        return Ok(CleanupResult {
            total_found: 0,
            successfully_removed: 0,
            failed_removals: 0,
            skipped_running: 0,
            removed_container_ids: Vec::new(),
            failed_removals_details: Vec::new(),
            duration_ms: start_time.elapsed().as_millis() as u64,
        });
    }

    let mut successfully_removed = 0;
    let mut failed_removals = 0;
    let mut removed_container_ids = Vec::new();
    let mut failed_removals_details = Vec::new();

    // 🚀 并发停止所有容器
    let mut tasks = Vec::new();
    for container in &matched_containers {
        if let Some(container_id) = &container.id {
            let container_name = container
                .names
                .as_ref()
                .and_then(|names| names.first())
                .map(|n| n.trim_start_matches('/'))
                .unwrap_or("unknown")
                .to_string();
            let docker_manager_clone = Arc::clone(docker_manager);
            let container_id_clone = container_id.clone();
            let task = tokio::spawn(async move {
                let result = stop_container_startup_mode(&docker_manager_clone, &container_id_clone).await;
                (container_id_clone, container_name, result)
            });
            tasks.push(task);
        }
    }
    
    // 等待所有任务完成
    for task in tasks {
        if let Ok((container_id, container_name, result)) = task.await {
            match result {
                Ok(_) => {
                    successfully_removed += 1;
                    removed_container_ids.push(container_id.clone());
                    info!("✅ [STARTUP_CLEANUP] 容器清理成功: container_id={}, name={}", 
                          container_id, container_name);
                }
                Err(e) => {
                    // 检查是否为409冲突错误
                    if is_409_conflict_error(&e) {
                        info!("🔄 [STARTUP_CLEANUP] 容器已在删除中，跳过: container_id={}, name={}", 
                              container_id, container_name);
                        // 409错误不计入失败统计
                        successfully_removed += 1;
                        removed_container_ids.push(container_id);
                    } else {
                        failed_removals += 1;
                        warn!("⚠️ [STARTUP_CLEANUP] 容器清理失败: container_id={}, name={}, error={}", 
                              container_id, container_name, e);
                        failed_removals_details.push(ContainerRemovalFailure {
                            container_id,
                            container_name,
                            error_message: e.to_string(),
                        });
                    }
                }
            }
        }
    }

    let duration_ms = start_time.elapsed().as_millis() as u64;
    
    info!(
        "🎯 [STARTUP_CLEANUP] 清理完成: 总数={}, 成功={}, 失败={}, 耗时={}ms",
        total_found, successfully_removed, failed_removals, duration_ms
    );

    Ok(CleanupResult {
        total_found,
        successfully_removed,
        failed_removals,
        skipped_running: 0, // 启动清理不跳过运行中的容器
        removed_container_ids,
        failed_removals_details,
        duration_ms,
    })
}

/// 停止单个容器（启动模式）
///
/// 使用启动清理的超时设置停止容器
///
/// # Arguments
///
/// * `docker_manager` - Docker管理器实例
/// * `container_id` - 容器ID
///
/// # Returns
///
/// 成功返回 `Ok(())`，失败返回 `DockerError`
async fn stop_container_startup_mode(
    docker_manager: &Arc<DockerManager>,
    container_id: &str,
) -> DockerResult<()> {
    docker_manager
        .stop_container_by_id_with_timeout(container_id, STARTUP_CLEANUP_TIMEOUT_SECONDS)
        .await?;

    // 给Docker一些时间完成清理
    tokio::time::sleep(tokio::time::Duration::from_millis(POST_STOP_WAIT_MS)).await;

    Ok(())
}

/// 检查是否为409冲突错误
///
/// 409错误表示容器已经在删除过程中，这在启动清理时是正常情况
///
/// # Arguments
///
/// * `error` - Docker错误
///
/// # Returns
///
/// 如果是409冲突错误返回 `true`，否则返回 `false`
fn is_409_conflict_error(error: &DockerError) -> bool {
    let error_str = error.to_string();
    error_str.contains("409") && error_str.contains("already in progress")
}

/// 运行时容器清理策略（单个容器）
///
/// 用于运行时快速清理单个容器。此函数会：
/// - 使用3秒优雅停止超时
/// - 超时后立即强制停止
/// - 快速释放资源
///
/// # Arguments
///
/// * `docker_manager` - Docker管理器实例
/// * `container_id` - 容器ID
///
/// # Returns
///
/// 成功返回 `Ok(())`，失败返回 `DockerError`
///
/// # Examples
///
/// ```rust,no_run
/// use docker_manager::container_stop;
///
/// container_stop::runtime_cleanup_container(
///     &docker_manager,
///     "container_id_123"
/// ).await?;
/// ```
pub async fn runtime_cleanup_container(
    docker_manager: &Arc<DockerManager>,
    container_id: &str,
) -> DockerResult<()> {
    info!("🔥 [RUNTIME_CLEANUP] 开始停止容器: container_id={}", container_id);

    match stop_container_runtime_mode(docker_manager, container_id).await {
        Ok(_) => {
            info!("✅ [RUNTIME_CLEANUP] 容器停止成功: container_id={}", container_id);
            Ok(())
        }
        Err(e) => {
            warn!("⚠️ [RUNTIME_CLEANUP] 容器停止失败: container_id={}, error={}", 
                  container_id, e);
            Err(e)
        }
    }
}

/// 运行时容器清理策略（批量）
///
/// 用于运行时批量清理多个容器。此函数会：
/// - 🚀 并发停止所有容器（提高清理速度）
/// - 使用3秒优雅停止超时
/// - 超时后立即强制停止
/// - 返回详细的清理统计信息
///
/// # Arguments
///
/// * `docker_manager` - Docker管理器实例
/// * `container_ids` - 容器ID列表
///
/// # Returns
///
/// 返回 `CleanupResult` 包含清理统计信息
///
/// # Examples
///
/// ```rust,no_run
/// use docker_manager::container_stop;
///
/// let container_ids = vec!["id1".to_string(), "id2".to_string()];
/// let result = container_stop::runtime_cleanup_containers(
///     &docker_manager,
///     container_ids
/// ).await?;
/// ```
pub async fn runtime_cleanup_containers(
    docker_manager: &Arc<DockerManager>,
    container_ids: Vec<String>,
) -> DockerResult<CleanupResult> {
    info!("🔥 [RUNTIME_CLEANUP] 开始批量清理容器: 数量={}", container_ids.len());
    let start_time = Instant::now();

    let total_found = container_ids.len();
    let mut successfully_removed = 0;
    let mut failed_removals = 0;
    let mut removed_container_ids = Vec::new();
    let mut failed_removals_details = Vec::new();

    // 🚀 并发停止所有容器
    let mut tasks = Vec::new();
    for container_id in &container_ids {
        let docker_manager_clone = Arc::clone(docker_manager);
        let container_id_clone = container_id.clone();
        let task = tokio::spawn(async move {
            let result = stop_container_runtime_mode(&docker_manager_clone, &container_id_clone).await;
            (container_id_clone, result)
        });
        tasks.push(task);
    }
    
    // 等待所有任务完成
    for task in tasks {
        if let Ok((container_id, result)) = task.await {
            match result {
                Ok(_) => {
                    successfully_removed += 1;
                    removed_container_ids.push(container_id.clone());
                    info!("✅ [RUNTIME_CLEANUP] 容器清理成功: container_id={}", container_id);
                }
                Err(e) => {
                    failed_removals += 1;
                    warn!("⚠️ [RUNTIME_CLEANUP] 容器清理失败: container_id={}, error={}", 
                          container_id, e);
                    failed_removals_details.push(ContainerRemovalFailure {
                        container_id: container_id.clone(),
                        container_name: container_id.clone(), // 批量清理时可能不知道名称
                        error_message: e.to_string(),
                    });
                }
            }
        }
    }

    let duration_ms = start_time.elapsed().as_millis() as u64;
    
    info!(
        "🎯 [RUNTIME_CLEANUP] 批量清理完成: 总数={}, 成功={}, 失败={}, 耗时={}ms",
        total_found, successfully_removed, failed_removals, duration_ms
    );

    Ok(CleanupResult {
        total_found,
        successfully_removed,
        failed_removals,
        skipped_running: 0, // 运行时清理不跳过运行中的容器
        removed_container_ids,
        failed_removals_details,
        duration_ms,
    })
}

/// 停止单个容器（运行时模式）
///
/// 使用运行时清理的超时设置停止容器
///
/// # Arguments
///
/// * `docker_manager` - Docker管理器实例
/// * `container_id` - 容器ID
///
/// # Returns
///
/// 成功返回 `Ok(())`，失败返回 `DockerError`
async fn stop_container_runtime_mode(
    docker_manager: &Arc<DockerManager>,
    container_id: &str,
) -> DockerResult<()> {
    docker_manager
        .stop_container_by_id_with_timeout(container_id, RUNTIME_CLEANUP_TIMEOUT_SECONDS)
        .await?;

    // 给Docker一些时间完成清理
    tokio::time::sleep(tokio::time::Duration::from_millis(POST_STOP_WAIT_MS)).await;

    Ok(())
}
