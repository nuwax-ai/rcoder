//! 服务类型定义
//!
//! 定义 RCoder 系统支持的服务类型，目前包括 RCoder 和 ComputerAgentRunner 两种类型。

use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

/// 服务类型枚举
///
/// 定义了 RCoder 系统支持的服务类型，每个服务类型对应不同的 Docker 镜像和运行环境。
/// 注意：不实现 Default trait，强制要求明确指定服务类型。
///
/// 支持多种格式的序列化和反序列化：
/// - 中划线格式（kebab-case）：web-agent-runner, computer-agent-runner
/// - 大驼峰格式（PascalCase）：WebAgentRunner, ComputerAgentRunner
/// - 旧枚举名（向后兼容）：RCoder, rcoder
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ToSchema)]
pub enum ServiceType {
    /// Web Agent Runner 服务
    /// 提供完整的 AI 开发功能，包括项目管理、代码生成、文件操作等
    /// 容器标识为 project_id，用于网页应用开发场景
    WebAgentRunner,
    /// Computer Agent Runner 服务
    /// 专注于代理运行和执行，提供轻量级的代理执行环境
    /// 容器标识为 user_id，用于桌面应用开发场景
    ComputerAgentRunner,
    /// 常规项目（normal project）
    /// 与 ComputerAgentRunner 复用同一 per-user 容器（identifier=user_id、同镜像、
    /// 同容器名/PVC/挂载——见 `container_family()`），仅 agent 工作目录不同：
    /// `/home/user/normalProject/{project_id}`。
    ComputerNormalProject,
    /// 用户应用（Userapp）
    /// 由 app_manager 托管的用户业务应用（Java/Python/Go/前端等），区别于 agent。
    /// 容器标识为 app_id；镜像/命令/端口由调用方提供，不走 select_image。
    /// K8s 模式下对应 Deployment（而非 agent 的裸 Pod）。
    Userapp,
    /// Userapp 构建/开发 agent-runner（路B：独立 per-app PVC + 复用 dev-rcoder-agent-runner 镜像）。
    /// 容器标识为 app_id（值经 project_id 槽位进容器基建）；走 create_container（STS）；与 ComputerAgentRunner 容器隔离。
    UserappBuilder,
}

// 自定义 Serialize 实现，输出中划线格式
impl Serialize for ServiceType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

// 自定义 Deserialize 实现，支持多种格式
impl<'de> Deserialize<'de> for ServiceType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<ServiceType>().map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for ServiceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceType::WebAgentRunner => write!(f, "web-agent-runner"),
            ServiceType::ComputerAgentRunner => write!(f, "computer-agent-runner"),
            ServiceType::ComputerNormalProject => write!(f, "computer-normal-project"),
            ServiceType::Userapp => write!(f, "user-app"),
            ServiceType::UserappBuilder => write!(f, "user-app-builder"),
        }
    }
}

impl std::str::FromStr for ServiceType {
    type Err = ServiceTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // 空字符串检查
        if s.trim().is_empty() {
            return Err(ServiceTypeError::EmptyServiceType);
        }

        // 支持多种格式：约定规范词、中划线旧格式（双向兼容 PG 存量与旧
        // label 值）、大驼峰（兼容旧配置）、旧枚举名
        match s {
            // 约定规范词（入参与序列化统一 userapp 族）
            "userapp" => Ok(ServiceType::Userapp),
            "userapp-builder" => Ok(ServiceType::UserappBuilder),
            // 中划线旧格式
            "user-app" => Ok(ServiceType::Userapp),
            "user-app-builder" => Ok(ServiceType::UserappBuilder),
            "web-agent-runner" => Ok(ServiceType::WebAgentRunner),
            "computer-agent-runner" => Ok(ServiceType::ComputerAgentRunner),
            "computer-normal-project" => Ok(ServiceType::ComputerNormalProject),
            // ChatServiceScope（chat 契约旧枚举，已统一并入本枚举）曾接受的
            // camelCase wire 词——保留使输入面零收缩（file-server
            // workspace_type 通道同词）
            "normalProject" => Ok(ServiceType::ComputerNormalProject),
            // 大驼峰格式（兼容旧配置）
            "WebAgentRunner" => Ok(ServiceType::WebAgentRunner),
            "ComputerAgentRunner" => Ok(ServiceType::ComputerAgentRunner),
            "ComputerNormalProject" => Ok(ServiceType::ComputerNormalProject),
            "UserApp" | "Userapp" => Ok(ServiceType::Userapp),
            "UserAppBuilder" | "UserappBuilder" => Ok(ServiceType::UserappBuilder),
            // 旧枚举名（向后兼容）
            "RCoder" | "rcoder" => Ok(ServiceType::WebAgentRunner),
            "application" | "app" => Ok(ServiceType::Userapp),
            _ => Err(ServiceTypeError::InvalidServiceType(s.to_string())),
        }
    }
}

/// 计算容器标识符时缺少必需字段（由 [`ServiceType::container_identifier`] 返回）。
///
/// 各运行时（docker / k8s）调用方应将其 `map_err` 转成各自的错误类型
/// （`DockerError::ConfigurationError` / `ContainerRuntimeError::ConfigurationError`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MissingIdentifier {
    /// `ComputerAgentRunner` 需要 `user_id`
    #[error("user_id is required for ComputerAgentRunner")]
    UserId,
    /// `WebAgentRunner` / `Userapp` 需要 `project_id`
    #[error("project_id is required for WebAgentRunner/Userapp")]
    ProjectId,
}

impl ServiceType {
    /// 获取服务类型的描述
    pub fn description(&self) -> &str {
        match self {
            ServiceType::WebAgentRunner => {
                "Web Agent Runner service, providing full AI development functionality for web applications"
            }
            ServiceType::ComputerAgentRunner => {
                "Computer Agent Runner service, focused on agent execution for desktop applications"
            }
            ServiceType::ComputerNormalProject => {
                "Normal project service, sharing the ComputerAgentRunner per-user container with a dedicated agent workdir (/home/user/normalProject/{project_id})"
            }
            ServiceType::Userapp => {
                "User application managed by app_manager (long-running service owned by the user, not an agent)"
            }
            ServiceType::UserappBuilder => {
                "Userapp build/dev agent-runner (per-app PVC, reuses dev-rcoder-agent-runner image)"
            }
        }
    }

    /// 获取容器名称前缀（硬编码的降级兜底值）
    ///
    /// **警告**：正常情况下应通过 `DockerManager::get_service_config()` 获取
    /// `ServiceImageConfig::container_prefix()`，该方法优先读取配置文件中的
    /// `image_tag_prefix` 字段。本方法仅在配置获取失败时作为安全回退使用。
    /// 直接调用本方法构造容器名可能导致与实际创建的容器名称不一致。
    pub fn container_prefix(&self) -> &str {
        // 归一：常规项目与 Computer 复用同一容器，降级兜底前缀也必须一致，
        // 否则兜底构造的容器名会分裂出第二个容器。
        match self {
            ServiceType::WebAgentRunner => "web-agent-runner",
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
                "computer-agent-runner"
            }
            ServiceType::Userapp => "rcoder-app",
            ServiceType::UserappBuilder => "rcoder-app-builder",
        }
    }

    /// 容器路由组判定：两类型是否复用同一物理容器资源。
    ///
    /// ComputerNormalProject 与 ComputerAgentRunner 复用同一 per-user 容器
    /// （同镜像/容器名/label/PVC/挂载）。**谓词判断而非值转换**——各自业务
    /// 语义保留（日志/注册表里的类型值不再有"归一而来"的歧义）；容器基建
    /// 的等值比较统一走本方法。新类型仅在 agent 工作目录链保留本义。
    pub fn same_container_family(self, other: ServiceType) -> bool {
        self.container_family_key() == other.container_family_key()
    }

    /// 是否属 Computer 容器路由组（常规项目与 Computer 沙箱共容器）。
    pub fn is_computer_family(self) -> bool {
        matches!(
            self,
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject
        )
    }

    /// 族代表值归一——**仅注册表/缓存的写入与查询边界使用**：
    /// projects/containers 注册表、Docker 容器表、K8s pod_cache 恒存族代表值
    /// （物理容器身份视角，与 K8s label `rcoder.io/service-type` 同值域）。
    ///
    /// 业务语义层（chat 受理、gRPC forward、ttyd header）**禁止调用**——
    /// 业务本义（ComputerNormalProject）以请求瞬态传递，不落注册表。
    /// 穷尽 match（无通配臂）：新增 ServiceType 变体时编译期报错，
    /// 强制声明其路由组归属。
    pub fn family_representative(self) -> ServiceType {
        match self {
            ServiceType::ComputerNormalProject => ServiceType::ComputerAgentRunner,
            ServiceType::ComputerAgentRunner
            | ServiceType::WebAgentRunner
            | ServiceType::Userapp
            | ServiceType::UserappBuilder => self,
        }
    }

    /// 容器基建字符串键（家族代表词：配置取键 / label / 命名前缀）。
    ///
    /// 穷尽 match（无通配臂）——新增 ServiceType 变体时此处编译期报错，
    /// 强制声明其路由组归属。仅容器基建使用；业务语义（agent 工作目录链
    /// 等）用原始值。
    pub fn container_family_key(self) -> &'static str {
        match self {
            // Computer 容器路由组：常规项目与 Computer 沙箱共容器，键取族代表词
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
                "computer-agent-runner"
            }
            ServiceType::WebAgentRunner => "web-agent-runner",
            ServiceType::Userapp => "user-app",
            ServiceType::UserappBuilder => "user-app-builder",
        }
    }

    /// 检查服务是否在给定的多镜像配置中启用
    pub fn is_enabled(&self, config: &crate::MultiImageConfig) -> bool {
        let service_key = self.container_family_key().to_string();
        if let Some(service_config) = config.services.get(&service_key) {
            service_config.enabled
        } else {
            tracing::warn!("Service '{}' not found in config", service_key);
            false
        }
    }

    /// 计算容器标识符（docker / k8s / handler 三处复用的**单一事实源**）。
    ///
    /// 优先级：
    ///   - `pod_id` 存在（共享容器场景）→ 返回 `pod_id`
    ///   - 否则按 service_type：
    ///     - [`ComputerAgentRunner`](ServiceType::ComputerAgentRunner) → `user_id`
    ///     - [`WebAgentRunner`](ServiceType::WebAgentRunner) | [`Userapp`](ServiceType::Userapp) → `project_id`
    ///
    /// 缺必需字段时返回 `Err(MissingIdentifier)`，由调用方转成各自的错误类型。
    /// 返回借用（零分配）；调用方需要 owned 字符串自行 `.to_string()`。
    ///
    /// ⚠️ 不要在各运行时里重写这段优先级逻辑，否则会导致 ensure 与 chat 为同一项目
    ///   造出不同名 pod + 不同 PVC（如 `rcoder-k8s-{user_id}` vs `rcoder-k8s-{project_id}`）。
    pub fn container_identifier<'a>(
        &self,
        pod_id: Option<&'a str>,
        user_id: Option<&'a str>,
        project_id: Option<&'a str>,
    ) -> Result<&'a str, MissingIdentifier> {
        if let Some(pid) = pod_id {
            return Ok(pid);
        }
        match self {
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
                user_id.ok_or(MissingIdentifier::UserId)
            }
            ServiceType::WebAgentRunner | ServiceType::Userapp | ServiceType::UserappBuilder => {
                project_id.ok_or(MissingIdentifier::ProjectId)
            }
        }
    }
}

/// Service type validation error
#[derive(Debug, Error)]
pub enum ServiceTypeError {
    #[error("service type cannot be empty")]
    EmptyServiceType,
    #[error(
        "unsupported service type '{0}', please use 'web-agent-runner'/'WebAgentRunner'/'RCoder', 'computer-agent-runner'/'ComputerAgentRunner', 'computer-normal-project'/'ComputerNormalProject', 'user-app'/'Userapp'/'application', or 'user-app-builder'/'UserappBuilder'"
    )]
    InvalidServiceType(String),
    #[error("service type '{0}' is disabled")]
    ServiceDisabled(String),
}

/// 获取所有支持的服务类型
pub fn get_supported_service_types() -> Vec<String> {
    vec![
        "web-agent-runner".to_string(),
        "computer-agent-runner".to_string(),
        "computer-normal-project".to_string(),
        "user-app".to_string(),
        "user-app-builder".to_string(),
    ]
}

/// 获取启用的服务类型列表
pub fn get_enabled_service_types(config: &crate::MultiImageConfig) -> Vec<String> {
    let supported = get_supported_service_types();
    supported
        .into_iter()
        .filter_map(|service_type| {
            // 使用 parse() 替代 from_str()
            match service_type.parse::<ServiceType>() {
                Ok(service) => {
                    if service.is_enabled(config) {
                        Some(service_type)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    tracing::warn!(" parse service type failed: {} - {:?}", service_type, e);
                    None
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MultiImageConfig;
    use std::collections::HashMap;

    // ---- container_identifier 单一事实源测试 ----

    #[test]
    fn pod_id_takes_priority_over_others() {
        // 共享容器场景：pod_id 存在时一律用它，无视 service_type
        for st in [
            ServiceType::WebAgentRunner,
            ServiceType::ComputerAgentRunner,
            ServiceType::Userapp,
            ServiceType::UserappBuilder,
        ] {
            assert_eq!(
                st.container_identifier(Some("shared-pod"), Some("u1"), Some("p1")),
                Ok("shared-pod")
            );
        }
    }

    #[test]
    fn web_uses_project_id() {
        let st = ServiceType::WebAgentRunner;
        assert_eq!(
            st.container_identifier(None, Some("u1"), Some("p1")),
            Ok("p1")
        );
        // user_id 给了也不用
        assert_eq!(
            st.container_identifier(None, Some("u1"), None),
            Err(MissingIdentifier::ProjectId)
        );
    }

    #[test]
    fn userapp_uses_project_id() {
        assert_eq!(
            ServiceType::Userapp.container_identifier(None, None, Some("app-9")),
            Ok("app-9")
        );
    }

    #[test]
    fn userapp_builder_uses_project_id() {
        // UserappBuilder identifier = app_id（值经 project_id 槽位传入）
        assert_eq!(
            ServiceType::UserappBuilder.container_identifier(None, None, Some("app-9")),
            Ok("app-9")
        );
        // user_id 给了也不用
        assert_eq!(
            ServiceType::UserappBuilder.container_identifier(None, Some("u1"), None),
            Err(MissingIdentifier::ProjectId)
        );
    }

    #[test]
    fn computer_uses_user_id() {
        let st = ServiceType::ComputerAgentRunner;
        assert_eq!(
            st.container_identifier(None, Some("u7"), Some("p1")),
            Ok("u7")
        );
        assert_eq!(
            st.container_identifier(None, None, Some("p1")),
            Err(MissingIdentifier::UserId)
        );
    }

    #[test]
    fn normal_project_shares_the_computer_container_identity() {
        // 路由组判定：同一物理容器（identifier/prefix/配置键全按 ComputerAgentRunner）
        let st = ServiceType::ComputerNormalProject;
        assert_eq!(st.container_family_key(), "computer-agent-runner");
        assert!(st.is_computer_family());
        assert!(st.same_container_family(ServiceType::ComputerAgentRunner));
        // 族代表值归一（仅注册表写入/查询边界用）：NormalProject → Computer
        assert_eq!(st.family_representative(), ServiceType::ComputerAgentRunner);
        assert_eq!(
            ServiceType::ComputerAgentRunner.family_representative(),
            ServiceType::ComputerAgentRunner
        );
        assert!(ServiceType::ComputerAgentRunner.same_container_family(st));
        assert!(!st.same_container_family(ServiceType::WebAgentRunner));
        assert_eq!(
            st.container_identifier(None, Some("u7"), Some("p1")),
            Ok("u7")
        );
        assert_eq!(
            st.container_identifier(None, None, Some("p1")),
            Err(MissingIdentifier::UserId)
        );
        assert_eq!(st.container_prefix(), "computer-agent-runner");
        // wire 词保留本义
        assert_eq!(st.to_string(), "computer-normal-project");
        assert_eq!(
            "computer-normal-project".parse::<ServiceType>().unwrap(),
            ServiceType::ComputerNormalProject
        );
        assert_eq!(
            "ComputerNormalProject".parse::<ServiceType>().unwrap(),
            ServiceType::ComputerNormalProject
        );
        // 其余类型家族恒等
        for other in [
            ServiceType::WebAgentRunner,
            ServiceType::ComputerAgentRunner,
            ServiceType::Userapp,
            ServiceType::UserappBuilder,
        ] {
            assert_eq!(other.container_family_key(), other.to_string().as_str());
        }
    }

    #[test]
    fn normal_project_is_enabled_through_computer_config() {
        use crate::ServiceImageConfig;
        let mut config = create_test_config();
        let computer = config
            .services
            .get("computer-agent-runner")
            .cloned()
            .unwrap();
        config.services.insert(
            "computer-agent-runner".to_string(),
            ServiceImageConfig {
                enabled: true,
                ..computer.clone()
            },
        );
        assert!(ServiceType::ComputerNormalProject.is_enabled(&config));
        config.services.insert(
            "computer-agent-runner".to_string(),
            ServiceImageConfig {
                enabled: false,
                ..computer
            },
        );
        assert!(!ServiceType::ComputerNormalProject.is_enabled(&config));
    }

    #[test]
    fn missing_identifier_display_is_stable() {
        // 错误信息被各运行时 map_err 后透传，保持稳定便于排错
        assert_eq!(
            MissingIdentifier::UserId.to_string(),
            "user_id is required for ComputerAgentRunner",
        );
        assert_eq!(
            MissingIdentifier::ProjectId.to_string(),
            "project_id is required for WebAgentRunner/Userapp",
        );
    }

    // ---- 原 service_type 测试 ----

    fn create_test_config() -> MultiImageConfig {
        use crate::{
            GlobalImageDefaults, ImageCacheConfig, ImageSelectionStrategy, ServiceImageConfig,
            ServiceResourceLimits,
        };

        let mut services = HashMap::new();

        services.insert(
            "web-agent-runner".to_string(),
            ServiceImageConfig {
                service_type: ServiceType::WebAgentRunner,
                image: None,
                arm64_image: None,   // 从配置文件加载
                amd64_image: None,   // 从配置文件加载
                default_image: None, // 从配置文件加载
                image_tag_prefix: Some("web-agent-runner".to_string()),
                enabled: true,
                environment: HashMap::new(),
                mounts: vec![],
                command: vec![],
                entrypoint: None,
                resource_limits: ServiceResourceLimits::new(None, None, None, None, None),
                work_dir: "/app".to_string(),
                network_mode: "bridge".to_string(),
                container_path_template: "/app/project_workspace/{project_id}".to_string(),
                workspace_resolution_path: None,
                security: None,
            },
        );

        services.insert(
            "computer-agent-runner".to_string(),
            ServiceImageConfig {
                service_type: ServiceType::ComputerAgentRunner,
                image: None,
                arm64_image: None,   // 从配置文件加载
                amd64_image: None,   // 从配置文件加载
                default_image: None, // 从配置文件加载
                image_tag_prefix: Some("computer-agent-runner".to_string()),
                enabled: false, // 默认禁用
                environment: HashMap::new(),
                mounts: vec![],
                command: vec![],
                entrypoint: None,
                resource_limits: ServiceResourceLimits::new(None, None, None, None, None),
                work_dir: "/app".to_string(),
                network_mode: "bridge".to_string(),
                container_path_template: "/app/computer-project-workspace/{user_id}/{project_id}"
                    .to_string(),
                workspace_resolution_path: None,
                security: None,
            },
        );

        MultiImageConfig {
            global_defaults: GlobalImageDefaults {
                image: None,
                arm64_image: None,
                amd64_image: None,
                default_image: None,
                registry_prefix: None, // 从配置文件加载
            },
            services,
            selection_strategy: ImageSelectionStrategy::ServiceOnly,
            cache_config: ImageCacheConfig::default(),
        }
    }

    #[test]
    fn test_service_type_basic() {
        assert_eq!(ServiceType::WebAgentRunner.to_string(), "web-agent-runner");
        assert_eq!(
            ServiceType::ComputerAgentRunner.to_string(),
            "computer-agent-runner"
        );
        assert_eq!(ServiceType::Userapp.to_string(), "user-app");

        assert!(ServiceType::WebAgentRunner.description().contains("full"));
        assert!(
            ServiceType::ComputerAgentRunner
                .description()
                .contains("execution")
        );
        assert!(ServiceType::Userapp.description().contains("app_manager"));
    }

    #[test]
    fn test_service_type_from_str() {
        // 中划线格式（推荐）
        assert_eq!(
            "web-agent-runner".parse::<ServiceType>().unwrap(),
            ServiceType::WebAgentRunner
        );
        assert_eq!(
            "computer-agent-runner".parse::<ServiceType>().unwrap(),
            ServiceType::ComputerAgentRunner
        );

        // 大驼峰格式（兼容旧配置）
        assert_eq!(
            "WebAgentRunner".parse::<ServiceType>().unwrap(),
            ServiceType::WebAgentRunner
        );
        assert_eq!(
            "ComputerAgentRunner".parse::<ServiceType>().unwrap(),
            ServiceType::ComputerAgentRunner
        );

        // 旧枚举名（向后兼容）
        assert_eq!(
            "RCoder".parse::<ServiceType>().unwrap(),
            ServiceType::WebAgentRunner
        );
        assert_eq!(
            "rcoder".parse::<ServiceType>().unwrap(),
            ServiceType::WebAgentRunner
        );

        // Userapp 多格式
        assert_eq!(
            "user-app".parse::<ServiceType>().unwrap(),
            ServiceType::Userapp
        );
        assert_eq!(
            "userapp".parse::<ServiceType>().unwrap(),
            ServiceType::Userapp
        );
        assert_eq!(
            "Userapp".parse::<ServiceType>().unwrap(),
            ServiceType::Userapp
        );
        assert_eq!(
            "application".parse::<ServiceType>().unwrap(),
            ServiceType::Userapp
        );

        // UserappBuilder 多格式
        assert_eq!(
            "user-app-builder".parse::<ServiceType>().unwrap(),
            ServiceType::UserappBuilder
        );
        assert_eq!(
            "UserappBuilder".parse::<ServiceType>().unwrap(),
            ServiceType::UserappBuilder
        );

        // 未知类型应该返回错误
        assert!("unknown".parse::<ServiceType>().is_err());

        // 空字符串应该返回错误
        assert!("".parse::<ServiceType>().is_err());
        assert!("   ".parse::<ServiceType>().is_err());
    }

    #[test]
    fn test_service_type_enabled() {
        let config = create_test_config();

        // WebAgentRunner 应该启用
        assert!(ServiceType::WebAgentRunner.is_enabled(&config));

        // ComputerAgentRunner 应该禁用
        assert!(!ServiceType::ComputerAgentRunner.is_enabled(&config));
    }

    #[test]
    fn test_get_supported_service_types() {
        let types = get_supported_service_types();
        assert_eq!(types.len(), 5);
        assert!(types.contains(&"web-agent-runner".to_string()));
        assert!(types.contains(&"computer-agent-runner".to_string()));
        assert!(types.contains(&"computer-normal-project".to_string()));
        assert!(types.contains(&"user-app".to_string()));
        assert!(types.contains(&"user-app-builder".to_string()));
    }

    #[test]
    fn test_get_enabled_service_types() {
        let config = create_test_config();
        let enabled = get_enabled_service_types(&config);

        assert_eq!(enabled.len(), 1);
        assert!(enabled.contains(&"web-agent-runner".to_string()));
        assert!(!enabled.contains(&"computer-agent-runner".to_string()));
    }

    #[test]
    fn test_service_type_serialization() {
        let service = ServiceType::WebAgentRunner;
        let serialized = serde_json::to_string(&service).unwrap();
        let deserialized: ServiceType = serde_json::from_str(&serialized).unwrap();

        assert_eq!(service, deserialized);
    }

    #[test]
    fn test_service_type_hash() {
        use std::hash::{Hash, Hasher};

        let mut hasher1 = std::collections::hash_map::DefaultHasher::new();
        let mut hasher2 = std::collections::hash_map::DefaultHasher::new();

        ServiceType::WebAgentRunner.hash(&mut hasher1);
        ServiceType::ComputerAgentRunner.hash(&mut hasher2);

        assert_ne!(hasher1.finish(), hasher2.finish());
    }
}
