//! Agent 安装目录策略模块
//!
//! 根据 ServiceType 业务场景，解析 agent 安装目录。
//! 支持 ComputerAgentRunner 和 RCoder 两种业务场景。
//!
//! ## 设计模式
//!
//! 使用策略模式（Strategy Pattern）：
//! - `AgentInstallStrategy` trait 定义策略接口
//! - `ComputerAgentRunnerStrategy` 和 `RcoderStrategy` 实现具体策略
//! - `create_strategy` 工厂函数根据 ServiceType 创建策略实例
//!
//! ## 扩展性
//!
//! 新增 ServiceType 只需：
//! 1. 添加新的 Strategy 结构体
//! 2. 实现 `AgentInstallStrategy` trait
//! 3. 在 `create_strategy` 中添加映射

use std::path::PathBuf;
use std::sync::Arc;

use shared_types::error_codes as ec;
use shared_types::{AppError, PlatformEntry, ProjectAndContainerInfo, RoutingParams, ServiceType};
use tracing::{info, warn};

use super::utils::{build_workspace_path, user_dir};

/// Agent 自动安装请求参数
///
/// 将 `ensure_agent_installed` 的多个 agent 相关参数聚合为一个结构体，
/// 避免函数参数过多（clippy::too_many_arguments）。
pub struct AgentInstallRequest<'a> {
    /// Agent 标识符（必填）
    pub agent_id: &'a str,
    /// 执行命令（必填）
    pub command: &'a str,
    /// 命令参数（可选，写入 registry 供 agent_runner 启动时使用）
    pub args: &'a [String],
    /// 期望版本号（必填，semver 格式）
    pub version: &'a str,
    /// 多平台下载地址（必填）
    pub platforms: &'a std::collections::HashMap<String, PlatformEntry>,
}

/// 安装上下文
///
/// 包含策略解析后的安装目录和容器标识符。
pub struct InstallContext {
    /// 安装目录（如 "/app/computer-project-workspace/{user_id}/acp-agent"）
    pub install_dir: PathBuf,
    /// 容器标识符（用于容器解析）
    pub container_identifier: String,
}

/// Agent 安装策略 trait
///
/// 每个 ServiceType 实现此 trait，定义：
/// - 如何从路由参数中提取标识符
/// - 如何构建安装目录路径
/// - 如何处理校验错误
pub trait AgentInstallStrategy: Send + Sync {
    /// 解析安装上下文
    ///
    /// # 参数
    /// * `project` - 已解析的项目和容器信息
    /// * `routing` - 请求中的路由参数
    ///
    /// # 返回
    /// `InstallContext` 包含安装目录和容器标识符，
    /// 或校验失败时返回 `AppError`。
    fn resolve_install_context(
        &self,
        project: &ProjectAndContainerInfo,
        routing: &RoutingParams,
    ) -> Result<InstallContext, AppError>;
}

// =============================================================================
// ComputerAgentRunner 策略
// =============================================================================

/// ComputerAgentRunner 业务场景的安装策略
///
/// - 标识符：`user_id` 或 `pod_id`
/// - 安装目录：`/app/computer-project-workspace/{user_id}/acp-agent`
pub struct ComputerAgentRunnerStrategy;

impl AgentInstallStrategy for ComputerAgentRunnerStrategy {
    fn resolve_install_context(
        &self,
        project: &ProjectAndContainerInfo,
        routing: &RoutingParams,
    ) -> Result<InstallContext, AppError> {
        // ComputerAgentRunner 需要 user_id 或 pod_id
        let user_id = project
            .user_id()
            .or(project.pod_id())
            .or(routing.user_id.as_deref())
            .or(routing.pod_id.as_deref())
            .ok_or_else(|| {
                AppError::with_message(
                    ec::ERR_VALIDATION,
                    "user_id or pod_id is required for ComputerAgentRunner agent installation",
                )
            })?;

        let user_workspace = user_dir(user_id).map_err(|e| {
            AppError::with_message(ec::ERR_VALIDATION, format!("invalid user_id: {}", e))
        })?;

        let install_dir = PathBuf::from(user_workspace).join("acp-agent");

        Ok(InstallContext {
            install_dir,
            container_identifier: user_id.to_string(),
        })
    }
}

// =============================================================================
// Rcoder 策略
// =============================================================================

/// Rcoder 业务场景的安装策略
///
/// - 标识符：`project_id` 或 `pod_id`
/// - 安装目录：`/app/project_workspace/{project_id}/acp-agent`（支持隔离类型）
pub struct RcoderStrategy;

impl AgentInstallStrategy for RcoderStrategy {
    fn resolve_install_context(
        &self,
        project: &ProjectAndContainerInfo,
        routing: &RoutingParams,
    ) -> Result<InstallContext, AppError> {
        // RCoder 使用 project_id 作为主要标识符
        let project_id = project.project_id();

        // 根据隔离类型构建工作空间路径
        let workspace_path = build_workspace_path(
            routing.isolation_type.as_deref(),
            routing.tenant_id.as_deref(),
            routing.space_id.as_deref(),
            project_id,
        )
        .map_err(|e| {
            AppError::with_message(
                ec::ERR_VALIDATION,
                format!("invalid path params: {}", e),
            )
        })?;

        let install_dir = PathBuf::from(workspace_path).join("acp-agent");

        // 容器标识符：pod_id（共享容器）或 project_id
        let container_identifier = routing
            .pod_id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| project_id.to_string());

        Ok(InstallContext {
            install_dir,
            container_identifier,
        })
    }
}

// =============================================================================
// 策略工厂
// =============================================================================

/// 根据 ServiceType 创建安装策略实例
///
/// # 返回
/// - `Some(Box<dyn AgentInstallStrategy>)` - 支持的 ServiceType
/// - `None` - 不支持的 ServiceType
pub fn create_strategy(service_type: &ServiceType) -> Option<Box<dyn AgentInstallStrategy>> {
    match service_type {
        ServiceType::ComputerAgentRunner => Some(Box::new(ComputerAgentRunnerStrategy)),
        ServiceType::RCoder => Some(Box::new(RcoderStrategy)),
    }
}

// =============================================================================
// Chat 接口自动安装
// =============================================================================

/// Chat 接口自动安装 agent
///
/// 在 chat handler 启动 agent 前调用。如果 agent_id + version 已在缓存中，
/// 直接跳过（零延迟）。否则下载 → 复制到安装目录 → 更新 registry。
pub async fn ensure_agent_installed(
    state: &Arc<crate::router::AppState>,
    project_id: &str,
    request: &AgentInstallRequest<'_>,
    service_type: &ServiceType,
) -> Result<(), AppError> {
    let t0 = std::time::Instant::now();
    let download_manager = &state.agent_download_manager;
    let agent_id = request.agent_id;
    let version = request.version;

    // 快速检查：缓存中已有该版本 → 跳过
    if download_manager.is_cached(agent_id, version) {
        info!(
            "📦 [CHAT] Agent already cached, skipping install: agent_id={}, version={}, elapsed={:?}",
            agent_id, version,
            t0.elapsed()
        );
        return Ok(());
    }

    info!(
        "📦 [CHAT] Agent not cached, starting auto-install: agent_id={}, version={}",
        agent_id, version
    );

    // 1. 平台匹配
    let sys_info = shared_types::SystemInfo::current();
    let platform_key = normalize_platform_key(&sys_info.os, &sys_info.arch);
    let platform_entry = request.platforms.get(&platform_key).ok_or_else(|| {
        AppError::with_message(
            shared_types::error_codes::ERR_AGENT_MGMT_PLATFORM_NOT_FOUND,
            format!(
                "platform not found: {} (available: {:?})",
                platform_key,
                request.platforms.keys().collect::<Vec<_>>()
            ),
        )
    })?;

    // 2. 解析安装目录
    let project = state
        .get_project(project_id)
        .ok_or_else(|| AppError::with_message(
            shared_types::error_codes::ERR_PROJECT_NOT_FOUND,
            format!("project not found: {}", project_id),
        ))?;

    let strategy = create_strategy(service_type).ok_or_else(|| {
        AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            format!("agent installation not supported for service type: {:?}", service_type),
        )
    })?;

    let routing = shared_types::RoutingParams {
        project_id: Some(project_id.to_string()),
        ..Default::default()
    };
    let install_ctx = strategy.resolve_install_context(&project, &routing)?;

    // 3. 下载到缓存
    download_manager
        .download_to_cache(agent_id, version, &platform_entry.url)
        .await
        .map_err(|e| {
            warn!("📦 [CHAT] Auto-install download failed: agent_id={}, version={}, error={}", agent_id, version, e);
            AppError::with_message(
                shared_types::error_codes::ERR_AGENT_MGMT_INSTALL_FAILED,
                format!("auto-install download failed: {}", e),
            )
        })?;

    // 4. 复制到安装目录
    download_manager
        .copy_to_target(agent_id, version, &install_ctx.install_dir)
        .await
        .map_err(|e| {
            warn!("📦 [CHAT] Auto-install copy failed: agent_id={}, version={}, error={}", agent_id, version, e);
            AppError::with_message(
                shared_types::error_codes::ERR_AGENT_MGMT_INSTALL_FAILED,
                format!("auto-install copy failed: {}", e),
            )
        })?;

    // 5. 更新 registry
    crate::agent_download::registry_update::update_registry(
        &install_ctx.install_dir,
        agent_id,
        version,
        request.command,
        request.args,
    )
    .await
    .map_err(|e| {
        warn!("📦 [CHAT] Auto-install registry update failed: agent_id={}, version={}, error={}", agent_id, version, e);
        AppError::with_message(
            shared_types::error_codes::ERR_AGENT_MGMT_INSTALL_FAILED,
            format!("auto-install registry update failed: {}", e),
        )
    })?;

    info!(
        "📦 [CHAT] Auto-install completed: agent_id={}, version={}, install_dir={}, elapsed={:?}",
        agent_id,
        version,
        install_ctx.install_dir.display(),
        t0.elapsed()
    );

    Ok(())
}

/// 归一化平台 key（代理到 shared_types）
use shared_types::version_util::normalize_platform_key;

// =============================================================================
// 测试
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computer_agent_runner_strategy_resolves_user_id() {
        let strategy = ComputerAgentRunnerStrategy;
        let mut project = ProjectAndContainerInfo::new("proj-123".to_string());
        project.set_user_id(Some("user-456".to_string()));
        project.set_service_type(Some(ServiceType::ComputerAgentRunner));

        let routing = RoutingParams::default();
        let ctx = strategy.resolve_install_context(&project, &routing).unwrap();

        assert_eq!(
            ctx.install_dir,
            PathBuf::from("/app/computer-project-workspace/user-456/acp-agent")
        );
        assert_eq!(ctx.container_identifier, "user-456");
    }

    #[test]
    fn computer_agent_runner_strategy_falls_back_to_pod_id() {
        let strategy = ComputerAgentRunnerStrategy;
        let mut project = ProjectAndContainerInfo::new("proj-123".to_string());
        project.set_pod_id(Some("pod-789".to_string()));
        project.set_service_type(Some(ServiceType::ComputerAgentRunner));

        let routing = RoutingParams::default();
        let ctx = strategy.resolve_install_context(&project, &routing).unwrap();

        assert_eq!(
            ctx.install_dir,
            PathBuf::from("/app/computer-project-workspace/pod-789/acp-agent")
        );
        assert_eq!(ctx.container_identifier, "pod-789");
    }

    #[test]
    fn computer_agent_runner_strategy_rejects_missing_identifiers() {
        let strategy = ComputerAgentRunnerStrategy;
        let project = ProjectAndContainerInfo::new("proj-123".to_string());
        let routing = RoutingParams::default();

        let result = strategy.resolve_install_context(&project, &routing);
        assert!(result.is_err());
    }

    #[test]
    fn rcoder_strategy_uses_project_id() {
        let strategy = RcoderStrategy;
        let project = ProjectAndContainerInfo::new("proj-123".to_string());

        let routing = RoutingParams::default();
        let ctx = strategy.resolve_install_context(&project, &routing).unwrap();

        assert_eq!(
            ctx.install_dir,
            PathBuf::from("/app/project_workspace/proj-123/acp-agent")
        );
        assert_eq!(ctx.container_identifier, "proj-123");
    }

    #[test]
    fn rcoder_strategy_with_tenant_isolation() {
        let strategy = RcoderStrategy;
        let project = ProjectAndContainerInfo::new("proj-123".to_string());

        let routing = RoutingParams {
            isolation_type: Some("tenant".to_string()),
            tenant_id: Some("t1".to_string()),
            space_id: Some("s1".to_string()),
            ..Default::default()
        };
        let ctx = strategy.resolve_install_context(&project, &routing).unwrap();

        assert_eq!(
            ctx.install_dir,
            PathBuf::from("/app/project_workspace/t1/s1/proj-123/acp-agent")
        );
    }

    #[test]
    fn rcoder_strategy_uses_pod_id_as_container_identifier() {
        let strategy = RcoderStrategy;
        let project = ProjectAndContainerInfo::new("proj-123".to_string());

        let routing = RoutingParams {
            pod_id: Some("pod-456".to_string()),
            ..Default::default()
        };
        let ctx = strategy.resolve_install_context(&project, &routing).unwrap();

        assert_eq!(ctx.container_identifier, "pod-456");
    }

    #[test]
    fn create_strategy_returns_correct_type() {
        assert!(create_strategy(&ServiceType::ComputerAgentRunner).is_some());
        assert!(create_strategy(&ServiceType::RCoder).is_some());
    }
}
