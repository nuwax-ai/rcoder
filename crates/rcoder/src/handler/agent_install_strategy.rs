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
use tracing::{debug, info};

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
/// 包含策略解析后的安装目录与安装身份。
pub struct InstallContext {
    /// 安装目录（如 "/app/computer-project-workspace/{user_id}/acp-agent"）
    pub install_dir: PathBuf,
    /// 解析出的安装身份（ComputerAgentRunner=user_id/pod_id，Rcoder=project_id）。
    ///
    /// 用于 per-user/per-project 的"是否已安装"判定与日志。
    /// install_dir 即按此 identifier 解析得出，两者同源——判定路径与安装路径一致。
    pub identifier: String,
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
            identifier: user_id.to_string(),
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
            AppError::with_message(ec::ERR_VALIDATION, format!("invalid path params: {}", e))
        })?;

        let install_dir = PathBuf::from(workspace_path).join("acp-agent");

        Ok(InstallContext {
            install_dir,
            identifier: project_id.to_string(),
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
        ServiceType::WebAgentRunner => Some(Box::new(RcoderStrategy)),
        // UserApp / UserAppBuilder 不是 agent(无 ACP chat),无 agent install strategy。
        // UserAppBuilder 仅跑 file-server build(本地编译),不需要安装 agent bundle。
        ServiceType::UserApp | ServiceType::UserAppBuilder => None,
    }
}

// =============================================================================
// Chat 接口自动安装
// =============================================================================

// 从 URL 安装 agent 的核心逻辑已下沉到共享 crate `agent_provisioning::install_agent`，
// 供 rcoder（本文件 `ensure_agent_installed` + `agent_mgmt_handler` 的 install-from-url API）
// 与 agent_runner（bundle 缺失兜底自装）复用。

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
    let agent_id = request.agent_id;
    let version = request.version;

    // 1. 先解析本 user/project 的安装上下文（install_dir 与 identifier 同源）
    let project = state.get_project(project_id).ok_or_else(|| {
        AppError::with_message(
            shared_types::error_codes::ERR_PROJECT_NOT_FOUND,
            format!("project not found: {}", project_id),
        )
    })?;

    let strategy = create_strategy(service_type).ok_or_else(|| {
        AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            format!(
                "agent installation not supported for service type: {:?}",
                service_type
            ),
        )
    })?;

    let routing = RoutingParams {
        project_id: Some(project_id.to_string()),
        ..Default::default()
    };
    let install_ctx = strategy.resolve_install_context(&project, &routing)?;
    let install_dir = &install_ctx.install_dir;
    let identifier = &install_ctx.identifier;

    // 2. per-user/per-project 判定：以 (identifier, agent_id, version) 为 key
    //    ★ 不能用全局 is_cached：它只判 cache_dir/{agent_id}/{version}（下载缓存），与 user 无关，
    //      会导致"首个 user 装上、其余 user 被跳过"——bundle.mjs 缺失 → ACP InitializeRequest 50s 超时。
    if is_agent_installed_for(identifier, install_dir, agent_id, version) {
        info!(
            "📦 [CHAT] Agent already installed for {}, skipping: agent_id={}, version={}, install_dir={}, elapsed={:?}",
            identifier,
            agent_id,
            version,
            install_dir.display(),
            t0.elapsed()
        );
        return Ok(());
    }

    info!(
        "📦 [CHAT] Agent not installed for {}, installing: agent_id={}, version={}, install_dir={}",
        identifier,
        agent_id,
        version,
        install_dir.display()
    );

    // 3. 未装 → 走完整安装；agent_provisioning::install_agent 内部 download_to_cache 会复用全局缓存自动跳过下载，只做 per-user 复制
    agent_provisioning::install_agent(
        &state.agent_download_manager,
        agent_id,
        version,
        request.command,
        request.args,
        request.platforms,
        install_dir,
    )
    .await?;

    info!(
        "📦 [CHAT] Auto-install completed for {}: agent_id={}, version={}, install_dir={}, elapsed={:?}",
        identifier,
        agent_id,
        version,
        install_dir.display(),
        t0.elapsed()
    );

    Ok(())
}

/// 判断 agent 是否已为指定身份（user_id/pod_id/project_id）安装到其工作区。
///
/// **per-user 判定的核心**：以 `(identifier, agent_id, version)` 为 key，
/// 检查 `install_dir/{agent_id}/{version}` 是否存在且非空。
/// 该路径与 `AgentDownloadManager::copy_to_target` 写入的目标同源
/// （install_dir 由 strategy 按 identifier 解析得出），保证判定路径与安装路径一致。
///
/// 不能用全局 `AgentDownloadManager::is_cached`——它只判
/// `cache_dir/{agent_id}/{version}`（下载缓存），与 user 无关。
fn is_agent_installed_for(
    identifier: &str,
    install_dir: &std::path::Path,
    agent_id: &str,
    version: &str,
) -> bool {
    let installed = agent_provisioning::is_agent_installed(install_dir, agent_id, version);
    debug!(
        identifier = %identifier,
        install_dir = %install_dir.display(),
        installed,
        "is_agent_installed_for: check per-user install state"
    );
    installed
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
        let ctx = strategy
            .resolve_install_context(&project, &routing)
            .unwrap();

        assert_eq!(
            ctx.install_dir,
            PathBuf::from("/app/computer-project-workspace/user-456/acp-agent")
        );
    }

    #[test]
    fn computer_agent_runner_strategy_falls_back_to_pod_id() {
        let strategy = ComputerAgentRunnerStrategy;
        let mut project = ProjectAndContainerInfo::new("proj-123".to_string());
        project.set_pod_id(Some("pod-789".to_string()));
        project.set_service_type(Some(ServiceType::ComputerAgentRunner));

        let routing = RoutingParams::default();
        let ctx = strategy
            .resolve_install_context(&project, &routing)
            .unwrap();

        assert_eq!(
            ctx.install_dir,
            PathBuf::from("/app/computer-project-workspace/pod-789/acp-agent")
        );
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
        let ctx = strategy
            .resolve_install_context(&project, &routing)
            .unwrap();

        assert_eq!(
            ctx.install_dir,
            PathBuf::from("/app/project_workspace/proj-123/acp-agent")
        );
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
        let ctx = strategy
            .resolve_install_context(&project, &routing)
            .unwrap();

        assert_eq!(
            ctx.install_dir,
            PathBuf::from("/app/project_workspace/t1/s1/proj-123/acp-agent")
        );
    }

    #[test]
    fn create_strategy_returns_correct_type() {
        assert!(create_strategy(&ServiceType::ComputerAgentRunner).is_some());
        assert!(create_strategy(&ServiceType::WebAgentRunner).is_some());
    }

    #[test]
    fn computer_strategy_exposes_user_id_identifier() {
        let strategy = ComputerAgentRunnerStrategy;
        let mut project = ProjectAndContainerInfo::new("proj-123".to_string());
        project.set_user_id(Some("user-456".to_string()));
        project.set_service_type(Some(ServiceType::ComputerAgentRunner));

        let ctx = strategy
            .resolve_install_context(&project, &RoutingParams::default())
            .unwrap();
        assert_eq!(ctx.identifier, "user-456");
    }

    #[test]
    fn computer_strategy_identifier_falls_back_to_pod_id() {
        let strategy = ComputerAgentRunnerStrategy;
        let mut project = ProjectAndContainerInfo::new("proj-123".to_string());
        project.set_pod_id(Some("pod-789".to_string()));
        project.set_service_type(Some(ServiceType::ComputerAgentRunner));

        let ctx = strategy
            .resolve_install_context(&project, &RoutingParams::default())
            .unwrap();
        assert_eq!(ctx.identifier, "pod-789");
    }

    #[test]
    fn rcoder_strategy_identifier_is_project_id() {
        let strategy = RcoderStrategy;
        let project = ProjectAndContainerInfo::new("proj-123".to_string());

        let ctx = strategy
            .resolve_install_context(&project, &RoutingParams::default())
            .unwrap();
        assert_eq!(ctx.identifier, "proj-123");
    }

    #[test]
    fn is_agent_installed_for_detects_install_state() {
        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path();

        // 不存在 → 未安装
        assert!(!is_agent_installed_for(
            "user-1",
            install_dir,
            "agentA",
            "1.0.0"
        ));

        // 版本目录存在且有文件 → 已安装
        let ver_dir = install_dir.join("agentA").join("1.0.0");
        std::fs::create_dir_all(&ver_dir).unwrap();
        std::fs::write(ver_dir.join("bundle.mjs"), "x").unwrap();
        assert!(is_agent_installed_for(
            "user-1",
            install_dir,
            "agentA",
            "1.0.0"
        ));

        // 版本目录存在但为空（不完整安装）→ 未安装
        let empty_ver = install_dir.join("agentB").join("2.0.0");
        std::fs::create_dir_all(&empty_ver).unwrap();
        assert!(!is_agent_installed_for(
            "user-1",
            install_dir,
            "agentB",
            "2.0.0"
        ));
    }

    /// 回归测试：两个 user 共享全局下载缓存，但 per-user 安装状态独立。
    /// user-1 已装 → user-2 仍判定为"未装"（这正是旧 bug 误判的地方）。
    #[test]
    fn per_user_install_state_is_independent() {
        let user1 = tempfile::tempdir().unwrap(); // /app/computer-project-workspace/user-1/acp-agent
        let user2 = tempfile::tempdir().unwrap(); // /app/computer-project-workspace/user-2/acp-agent
        let agent_id = "33290548";
        let version = "1.0.1";

        // user-1 装好（版本目录非空即可，is_agent_installed_for 只看这一点）
        let u1_ver = user1.path().join(agent_id).join(version);
        std::fs::create_dir_all(&u1_ver).unwrap();
        std::fs::write(u1_ver.join("bundle.mjs"), "x").unwrap();

        // user-1 已装、user-2 未装 —— per-user 独立判定
        assert!(is_agent_installed_for(
            "user-1",
            user1.path(),
            agent_id,
            version
        ));
        assert!(!is_agent_installed_for(
            "user-2",
            user2.path(),
            agent_id,
            version
        ));
    }
}
