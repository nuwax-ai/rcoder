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

use shared_types::error_codes as ec;
use shared_types::{AppError, ProjectAndContainerInfo, RoutingParams, ServiceType};

use super::utils::{build_workspace_path, user_dir};

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

    /// 返回此策略处理的 ServiceType
    fn service_type(&self) -> ServiceType;
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

    fn service_type(&self) -> ServiceType {
        ServiceType::ComputerAgentRunner
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

    fn service_type(&self) -> ServiceType {
        ServiceType::RCoder
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
