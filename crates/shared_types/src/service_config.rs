//! 服务镜像配置
//!
//! 定义了每个服务类型的镜像配置、环境变量、挂载点等信息。

use crate::service_type::ServiceType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 容器路径模板的默认值
fn default_container_path_template() -> String {
    std::path::PathBuf::from(crate::paths::WORKSPACE_ROOT)
        .join("{project_id}")
        .to_string_lossy()
        .into_owned()
}

/// Computer Agent Runner 容器路径模板的默认值
fn default_computer_agent_runner_container_path_template() -> String {
    std::path::PathBuf::from(crate::paths::COMPUTER_WORKSPACE_ROOT)
        .join("{user_id}")
        .join("{project_id}")
        .to_string_lossy()
        .into_owned()
}

/// 容器工作目录的默认值
fn default_work_dir() -> String {
    "/app".to_string()
}

/// 容器网络模式的默认值
fn default_network_mode() -> String {
    "bridge".to_string()
}

/// 服务镜像配置
///
/// 定义了每个服务类型的详细配置，包括镜像选择、环境变量和挂载点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceImageConfig {
    /// 服务类型
    pub service_type: ServiceType,
    /// 通用镜像（优先级最高，如果指定则忽略架构特定镜像）
    pub image: Option<String>,
    /// ARM64 架构专用镜像
    pub arm64_image: Option<String>,
    /// AMD64 架构专用镜像
    pub amd64_image: Option<String>,
    /// 默认回退镜像
    pub default_image: Option<String>,
    /// 镜像标签前缀（用于自动构建镜像名称）
    pub image_tag_prefix: Option<String>,
    /// 是否启用该服务类型
    pub enabled: bool,
    /// 服务特定的环境变量
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// 服务特定的挂载点
    #[serde(default)]
    pub mounts: Vec<ServiceMountConfig>,
    /// 容器启动命令
    #[serde(default)]
    pub command: Vec<String>,
    /// 容器入口点
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// 容器资源限制配置
    #[serde(default)]
    pub resource_limits: ServiceResourceLimits,
    /// 容器工作目录
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
    /// 容器网络模式
    #[serde(default = "default_network_mode")]
    pub network_mode: String,
    /// 容器内挂载路径模板（支持变量替换）
    /// 默认值: "/app/project_workspace/{project_id}"
    /// 支持变量: {project_id}, {user_id}, {service_type}
    #[serde(default = "default_container_path_template")]
    pub container_path_template: String,
    /// rcoder 容器内用于反向解析宿主机路径的基准路径
    ///
    /// DockerManager 通过此路径调用 Docker API 解析出宿主机绝对路径，用于构建挂载。
    /// 未配置时自动从 container_path_template 截取 `{` 前缀推导：
    ///   - RCoder: "/app/project_workspace/{project_id}" → "/app/project_workspace"
    ///   - ComputerAgentRunner: "/app/computer-project-workspace/{user_id}/{project_id}" → "/app/computer-project-workspace"
    #[serde(default)]
    pub workspace_resolution_path: Option<String>,
    /// 容器安全配置（可选）。仅 Docker 部署模式透传到 bollard HostConfig；
    /// 未配置（None）时走代码默认（privileged=false + cap_drop=[NET_RAW,NET_ADMIN]）。
    #[serde(default)]
    pub security: Option<ServiceSecurityConfig>,
}

/// 服务挂载点配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceMountConfig {
    /// 容器内路径
    pub container_path: String,
    /// 宿主机路径（支持变量替换）
    /// 可使用的变量：
    /// - {resolved_path}: 从 resolve_from 解析后的宿主机基础路径
    /// - {project_id}: 项目 ID
    /// - {user_id}: 用户 ID
    /// - {container_name}: 容器名称
    /// - {timestamp}: 时间戳（YYYYMMDDHHMMSS 格式）
    /// - {log_dir_name}: 日志目录名（container_name-timestamp）
    ///
    /// 示例: "{resolved_path}/{log_dir_name}" => "/host/logs/computer-agent-runner-user_123-20241212160000"
    pub host_path: String,
    /// 是否只读
    pub read_only: bool,
    /// 挂载类型（bind/volume）
    pub mount_type: String,
    /// 动态路径解析源（可选）
    /// 当 host_path 包含 {resolved_path} 变量时，指定从哪个容器内路径解析宿主机基础路径
    /// 例如：resolve_from: "/app/logs" 会将容器内的 /app/logs 解析为宿主机绝对路径
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolve_from: Option<String>,
}

/// 服务资源限制配置
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServiceResourceLimits {
    /// 内存限制（字节，支持浮点数输入）
    ///
    /// 注：字段名 `memory`（对齐 /computer/pod/ensure 基准）；serde alias `memory_limit`
    /// 兼容旧 config.yml 键名与旧 HTTP 请求，反序列化两种写法都接受。
    #[serde(alias = "memory_limit")]
    pub memory: Option<f64>,
    /// CPU 限制（核心数）。alias `cpu_limit` 兼容旧命名。
    #[serde(alias = "cpu_limit")]
    pub cpu: Option<f64>,
    /// 交换空间限制（字节，支持浮点数输入）。alias `swap_limit` 兼容旧命名。
    #[serde(alias = "swap_limit")]
    pub swap: Option<f64>,
    /// PVC 存储空间大小（仅 K8s 模式生效，Docker 模式忽略）
    ///
    /// 格式：`<数字><单位>`，支持 Mi/Gi/Ti（二进制）和 M/G/T（十进制）
    /// 范围：最小 1Gi，最大 100Ti，默认 10Gi
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_size: Option<String>,
    /// 临时存储限制（overlay 可写层，仅 K8s 模式生效）
    ///
    /// 限制容器根文件系统可写层 + emptyDir 等临时存储的写入量（区别于 storage_size 管 PVC）。
    /// 与 storage_size 是两个独立配额，不会合并；格式同 storage_size；未指定时回退到 storage_size 的值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_storage_limit: Option<String>,
}

/// 服务容器的安全配置（可选）。仅 Docker 部署模式透传到 bollard HostConfig。
///
/// 字段语义与 Docker `HostConfig` / docker-compose.yml 一致，运维可直接照搬 compose 写法。
/// 合并语义（在 docker_manager 的 `build_host_config` 中应用）：
/// - `ServiceImageConfig.security = None`（未配置 security 块）→ 完全走代码默认逻辑
///   （`privileged=false` + `cap_drop=[NET_RAW,NET_ADMIN]`，受 `ebpf-debug` feature 影响）。
/// - `security = Some`（配置了 security 块）→ 该配置覆盖一切（含 `ebpf-debug`）；
///   块内每个字段 `Some(x)` 用 x，字段未写（`None`）回退到该字段的内置默认。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServiceSecurityConfig {
    /// 是否以特权模式运行（默认 false）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,
    /// 要添加的内核 capabilities，如 ["SYS_PTRACE"]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap_add: Option<Vec<String>>,
    /// 要移除的内核 capabilities；显式配置则整体覆盖默认 ["NET_RAW","NET_ADMIN"]（写 `[]` 表示不 drop 任何 cap）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap_drop: Option<Vec<String>>,
    /// Docker security_opt，如 ["seccomp=unconfined","apparmor=unconfined"]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_opt: Option<Vec<String>>,
    /// 进程数限制；`0` 或 `-1` 表示无限制
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<i64>,
    /// 是否在容器内运行 init 进程（转发信号 + 回收僵尸进程）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init: Option<bool>,
}

impl ServiceResourceLimits {
    /// 构造资源限制配置
    ///
    /// 所有参数均为 `Option`，未限制的资源传 `None`。
    /// - K8s 模式下 `storage_size` 管 PVC，`ephemeral_storage_limit` 管 overlay 可写层（未指定时回退到 `storage_size`）
    /// - Docker 模式忽略 `storage_size` / `ephemeral_storage_limit`
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory: Option<f64>,
        cpu: Option<f64>,
        swap: Option<f64>,
        storage_size: Option<String>,
        ephemeral_storage_limit: Option<String>,
    ) -> Self {
        Self {
            memory,
            cpu,
            swap,
            storage_size,
            ephemeral_storage_limit,
        }
    }

    /// 验证资源限制的合理性
    pub fn validate(&self) -> Result<(), String> {
        // 内存限制（bytes）。阈值用十进制 MB/GB（与 Docker/K8s quantity 习惯一致，
        // 非 MiB/GiB；512×10⁶ 而非 512×2²⁰）：512MB ~ 64GB
        const MIN_MEMORY_BYTES: f64 = 512_000_000.0;
        const MAX_MEMORY_BYTES: f64 = 64_000_000_000.0;
        if let Some(memory) = self.memory {
            if memory < MIN_MEMORY_BYTES {
                return Err("memory_limit must be at least 512MB".to_string());
            }
            if memory > MAX_MEMORY_BYTES {
                return Err("memory_limit cannot exceed 64GB".to_string());
            }
        }

        // CPU 限制：0.5 ~ 32 核
        if let Some(cpu) = self.cpu {
            if cpu < 0.5 {
                return Err("cpu_limit must be at least 0.5 cores".to_string());
            }
            if cpu > 32.0 {
                return Err("cpu_limit cannot exceed 32 cores".to_string());
            }
        }

        // 注:swap 与 memory 的关系校验已移除——改为在 resolve 阶段由
        // [`ServiceResourceLimits::normalize_swap`] 自动规整(swap < memory 时
        // 上调到 memory × 2),避免上游误传 swap<memory 直接阻塞业务。
        // 详见该函数文档。

        Ok(())
    }

    /// 合并资源限制（override_limits 覆盖 self 中的字段）
    pub fn merge_with(&self, override_limits: &ServiceResourceLimits) -> Self {
        Self {
            memory: override_limits.memory.or(self.memory),
            cpu: override_limits.cpu.or(self.cpu),
            swap: override_limits.swap.or(self.swap),
            storage_size: override_limits
                .storage_size
                .clone()
                .or_else(|| self.storage_size.clone()),
            ephemeral_storage_limit: override_limits
                .ephemeral_storage_limit
                .clone()
                .or_else(|| self.ephemeral_storage_limit.clone()),
        }
    }

    /// 规整 swap 上限:若 `swap < memory`,自动上调到 `memory × 2`。
    ///
    /// # 背景
    /// cgroup `memory.memsw.limit`(Docker `--memory-swap`、K8s 同义)是
    /// **memory + swap 的总和**,语义上必须 ≥ memory。上游(如 Backend)偶尔会误传
    /// `swap < memory`(典型场景:把 swap 按核数 `perUserCpuCores × 1GiB` 估算,
    /// 而 memory 按 `perUserMemoryGB × 1GiB` 估算,当核数 < 内存 GB 数时 swap 反而更小)。
    /// 与其在 validate 阶段硬性拒绝阻塞业务,这里按 `memory × 2` 兜底——既满足
    /// cgroup 约束,又留出 1×memory 的交换空间。
    ///
    /// 仅在 memory 与 swap 均 `Some` 且 `swap < memory` 时生效;其余情况原样返回。
    ///
    /// # 返回
    /// `(规整后的 Self, 是否发生了修正)` —— 调用方据 `bool` 决定是否打 warn 日志。
    pub fn normalize_swap(mut self) -> (Self, bool) {
        if let (Some(memory), Some(swap)) = (self.memory, self.swap)
            && swap < memory
        {
            self.swap = Some(memory * 2.0);
            return (self, true);
        }
        (self, false)
    }
}

/// 验证结果
#[derive(Debug)]
pub enum ConfigValidationResult {
    Valid,
    Warning(String),
    Error(String),
}

impl ServiceImageConfig {
    /// 验证服务镜像配置的有效性
    pub fn validate(&self) -> ConfigValidationResult {
        // 验证至少有一个镜像配置
        if self.image.is_none()
            && self.arm64_image.is_none()
            && self.amd64_image.is_none()
            && self.default_image.is_none()
        {
            tracing::error!(
                "[CONFIG_VALIDATION] Service type {} has no image configured! \
                 image={:?}, arm64_image={:?}, amd64_image={:?}, default_image={:?}, \
                 image_tag_prefix={:?}, enabled={}",
                self.service_type,
                self.image,
                self.arm64_image,
                self.amd64_image,
                self.default_image,
                self.image_tag_prefix,
                self.enabled
            );
            return ConfigValidationResult::Error(format!(
                "Service type {} must have at least one image configured (image={:?}, arm64={:?}, amd64={:?}, default={:?})",
                self.service_type,
                self.image,
                self.arm64_image,
                self.amd64_image,
                self.default_image
            ));
        }

        // 验证镜像名称格式
        for image in [
            &self.image,
            &self.arm64_image,
            &self.amd64_image,
            &self.default_image,
        ]
        .into_iter()
        .flatten()
        {
            if image.trim().is_empty() {
                return ConfigValidationResult::Warning(format!(
                    "Service type {} has empty image name",
                    self.service_type
                ));
            }

            // 验证镜像名称格式（简单的格式检查）
            if !image
                .chars()
                .all(|c: char| c.is_alphanumeric() || "/:.-_".contains(c))
            {
                return ConfigValidationResult::Warning(format!(
                    "Service type {} image name '{}' may contain invalid characters",
                    self.service_type, image
                ));
            }
        }

        // 验证挂载点配置
        for mount in &self.mounts {
            if mount.container_path.trim().is_empty() {
                return ConfigValidationResult::Error(format!(
                    "Service type {} has empty container mount path",
                    self.service_type
                ));
            }

            if mount.host_path.trim().is_empty() {
                return ConfigValidationResult::Error(format!(
                    "Service type {} has empty host mount path",
                    self.service_type
                ));
            }

            // 验证挂载类型
            if mount.mount_type != "bind" && mount.mount_type != "volume" {
                return ConfigValidationResult::Warning(format!(
                    "Service type {} has unsupported mount type '{}'",
                    self.service_type, mount.mount_type
                ));
            }
        }

        ConfigValidationResult::Valid
    }

    /// 根据当前平台选择合适的镜像
    pub fn get_image_for_platform(&self, platform: &str) -> Option<String> {
        // 优先使用通用镜像
        if let Some(ref image) = self.image {
            return Some(image.clone());
        }

        // 根据平台选择架构特定镜像
        match platform {
            "linux/arm64" => self
                .arm64_image
                .clone()
                .or_else(|| self.default_image.clone()),
            "linux/amd64" => self
                .amd64_image
                .clone()
                .or_else(|| self.default_image.clone()),
            _ => {
                tracing::warn!("Unknown platform '{}', using default image", platform);
                self.default_image.clone()
            }
        }
    }

    /// 合并环境变量（基础环境 + 服务特定环境）
    pub fn merge_environment(&self, base_env: &HashMap<String, String>) -> HashMap<String, String> {
        let mut merged = base_env.clone();
        merged.extend(self.environment.clone());
        merged
    }

    /// 获取挂载点的字符串表示
    pub fn get_mounts_description(&self) -> String {
        if self.mounts.is_empty() {
            return "No mount points".to_string();
        }

        self.mounts
            .iter()
            .map(|mount| {
                format!(
                    "{} -> {} ({})",
                    mount.host_path, mount.container_path, mount.mount_type
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 获取配置摘要
    pub fn get_summary(&self) -> String {
        format!(
            "ServiceType: {}, Enabled: {}, Image: {:?}, Mounts: {}",
            self.service_type,
            self.enabled,
            self.image
                .as_ref()
                .or(self.arm64_image.as_ref())
                .or(self.amd64_image.as_ref())
                .or(self.default_image.as_ref()),
            self.get_mounts_description()
        )
    }

    /// 获取容器名称前缀
    ///
    /// 优先使用配置的 image_tag_prefix，否则使用 service_type 的默认前缀。
    /// 这确保了容器创建和清理时使用一致的前缀。
    ///
    /// # Returns
    ///
    /// 容器名称前缀字符串
    pub fn container_prefix(&self) -> &str {
        self.image_tag_prefix
            .as_deref()
            .unwrap_or_else(|| self.service_type.container_prefix())
    }

    /// 获取 workspace 解析路径（rcoder 容器内路径）
    ///
    /// 优先使用显式配置的 workspace_resolution_path，
    /// 未配置时根据 service_type 使用默认值。
    pub fn effective_workspace_resolution_path(&self) -> String {
        self.workspace_resolution_path
            .clone()
            .unwrap_or_else(|| match self.service_type {
                ServiceType::WebAgentRunner => crate::paths::WORKSPACE_ROOT.to_string(),
                ServiceType::ComputerAgentRunner => {
                    crate::paths::COMPUTER_WORKSPACE_ROOT.to_string()
                }
                // UserApp 复用 rcoder-workspace PVC 的 apps subPath（部署侧挂到 /app/app-workspace）
                ServiceType::UserApp => "/app/app-workspace".to_string(),
                // UserAppBuilder: per-app PVC(`rcoder-app-{app_id}-workspace`)挂载点
                ServiceType::UserAppBuilder => "/app/userapp-workspace".to_string(),
            })
    }

    /// 获取 workspace 在 sub-container 内的挂载路径
    ///
    /// 从环境变量 `PROJECT_WORKSPACE_BASE` 读取（config.yml 已配置），
    /// 回退到 effective_workspace_resolution_path()。
    ///
    /// - RCoder: `PROJECT_WORKSPACE_BASE="/app/project_workspace"`
    /// - ComputerAgentRunner: `PROJECT_WORKSPACE_BASE="/home/user"`
    pub fn workspace_container_path(&self) -> String {
        self.environment
            .get("PROJECT_WORKSPACE_BASE")
            .cloned()
            .unwrap_or_else(|| self.effective_workspace_resolution_path())
    }

    /// 解析容器路径模板，进行变量替换
    ///
    /// 支持的变量:
    /// - {project_id}: 项目ID
    /// - {user_id}: 用户ID
    /// - {service_type}: 服务类型
    ///
    /// # Arguments
    /// * `variables` - 包含变量名和值的 HashMap
    ///
    /// # Returns
    /// 解析后的容器路径字符串
    ///
    /// # Example
    ///
    /// 替换模板中的变量占位符（如 `{project_id}`）为实际值：
    ///
    /// - 输入模板: `/app/project_workspace/{project_id}`
    /// - 变量: `{"project_id": "123"}`
    /// - 输出: `/app/project_workspace/123`
    ///
    pub fn resolve_container_path(
        &self,
        variables: &std::collections::HashMap<String, String>,
    ) -> String {
        let mut resolved = self.container_path_template.clone();
        for (key, value) in variables {
            resolved = resolved.replace(&format!("{{{}}}", key), value);
        }
        resolved
    }
}

impl ServiceMountConfig {
    /// 验证挂载点配置
    pub fn validate(&self) -> ConfigValidationResult {
        if self.container_path.trim().is_empty() {
            return ConfigValidationResult::Error(
                "Container mount path cannot be empty".to_string(),
            );
        }

        if self.host_path.trim().is_empty() {
            return ConfigValidationResult::Error("Host mount path cannot be empty".to_string());
        }

        // 验证路径格式
        if self.container_path.starts_with('/')
            && !self
                .container_path
                .chars()
                .all(|c: char| c.is_alphanumeric() || "/-_.".contains(c))
        {
            return ConfigValidationResult::Warning(format!(
                "Container mount path '{}' may contain invalid characters",
                self.container_path
            ));
        }

        if self.mount_type != "bind" && self.mount_type != "volume" {
            return ConfigValidationResult::Error(format!(
                "Unsupported mount type '{}', must be 'bind' or 'volume'",
                self.mount_type
            ));
        }

        ConfigValidationResult::Valid
    }

    /// 解析宿主机路径中的变量
    /// 支持的变量：
    /// - {project_id}: 项目ID
    /// - {workspace_dir}: 工作目录
    pub fn resolve_host_path(&self, variables: &HashMap<String, String>) -> String {
        let mut resolved = self.host_path.clone();

        for (key, value) in variables {
            resolved = resolved.replace(&format!("{{{}}}", key), value);
        }

        resolved
    }
}

/// 创建默认的 RCoder 服务配置
pub fn default_rcoder_service_config() -> ServiceImageConfig {
    let mut environment = HashMap::new();
    environment.insert("RUST_LOG".to_string(), "info".to_string());
    environment.insert("SERVICE_MODE".to_string(), "full".to_string());
    environment.insert("API_PORT".to_string(), "8086".to_string());

    // 🔥 默认不提供挂载配置，让配置文件控制
    let mounts = vec![];

    // 默认启动命令
    let command = vec![
        "/app/bin/agent_runner".to_string(),
        "--port".to_string(),
        "8086".to_string(),
    ];

    // 默认资源限制
    let resource_limits = ServiceResourceLimits::new(
        Some(2_000_000_000.0), // 2GB
        Some(2.0),             // 2 核
        Some(4_000_000_000.0), // 4GB
        None, // storage_size: 由 k8s_pvc.rs DEFAULT_PVC_STORAGE_SIZE 兜底(当前 10Gi)
        None, // ephemeral_storage_limit: 回退到 storage_size
    );

    ServiceImageConfig {
        service_type: ServiceType::WebAgentRunner,
        image: None,         // 使用架构特定镜像
        arm64_image: None,   // 从配置文件加载
        amd64_image: None,   // 从配置文件加载
        default_image: None, // 从配置文件加载
        image_tag_prefix: Some("web-agent-runner".to_string()),
        enabled: true, // 当前启用
        environment,
        mounts,
        command,
        entrypoint: None, // 使用镜像默认入口点
        resource_limits,
        work_dir: "/app".to_string(),
        network_mode: "bridge".to_string(),
        container_path_template: default_container_path_template(),
        workspace_resolution_path: None,
        security: None,
    }
}

/// 创建默认的 Computer Agent Runner 服务配置
pub fn default_agent_runner_service_config() -> ServiceImageConfig {
    let mut environment = HashMap::new();
    environment.insert("RUST_LOG".to_string(), "debug".to_string());
    environment.insert("SERVICE_MODE".to_string(), "agent-only".to_string());
    environment.insert("AGENT_PORT".to_string(), "8086".to_string());
    environment.insert(
        "PROJECT_WORKSPACE_BASE".to_string(),
        "/home/user".to_string(),
    );

    // 🔥 Agent 清理配置（通过环境变量控制）
    // 设置为 3600 秒（1小时），用户可以在 docker/config.yml 中覆盖此值
    environment.insert(
        "RCODER_AGENT_IDLE_TIMEOUT_SECS".to_string(),
        "3600".to_string(),
    ); // 1 小时

    let mounts = vec![];

    // 默认启动命令
    let command = vec![
        "/app/bin/agent_runner".to_string(),
        "--port".to_string(),
        "8086".to_string(),
    ];

    // 默认资源限制（ComputerAgentRunner 可能需要更多资源）
    let resource_limits = ServiceResourceLimits::new(
        Some(4_000_000_000.0), // 4GB
        Some(3.0),             // 3 核
        Some(8_000_000_000.0), // 8GB
        None, // storage_size: 由 k8s_pvc.rs DEFAULT_PVC_STORAGE_SIZE 兜底(当前 10Gi)
        None, // ephemeral_storage_limit: 回退到 storage_size
    );

    ServiceImageConfig {
        service_type: ServiceType::ComputerAgentRunner,
        image: None,         // 使用架构特定镜像
        arm64_image: None,   // 从配置文件加载
        amd64_image: None,   // 从配置文件加载
        default_image: None, // 从配置文件加载
        image_tag_prefix: Some("computer-agent-runner".to_string()),
        enabled: true, // 当前启用
        environment,
        mounts,
        command,
        entrypoint: None, // 使用镜像默认入口点
        resource_limits,
        work_dir: "/app".to_string(),
        network_mode: "bridge".to_string(),
        container_path_template: default_computer_agent_runner_container_path_template(),
        workspace_resolution_path: None,
        security: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ServiceSecurityConfig 反序列化：未配置字段应为 None
    #[test]
    fn test_security_config_deserialize() {
        let json = r#"{"privileged":true,"cap_add":["SYS_PTRACE"],"security_opt":["seccomp=unconfined"],"pids_limit":200}"#;
        let sec: ServiceSecurityConfig = serde_json::from_str(json).unwrap();
        assert_eq!(sec.privileged, Some(true));
        assert_eq!(sec.cap_add, Some(vec!["SYS_PTRACE".to_string()]));
        assert_eq!(
            sec.security_opt,
            Some(vec!["seccomp=unconfined".to_string()])
        );
        assert_eq!(sec.pids_limit, Some(200));
        assert_eq!(sec.cap_drop, None);
        assert_eq!(sec.init, None);
    }

    /// 默认服务配置（代码内构造）security 必须为 None —— 不改变现有默认行为
    #[test]
    fn test_default_service_config_security_is_none() {
        assert!(default_agent_runner_service_config().security.is_none());
        assert!(default_rcoder_service_config().security.is_none());
    }

    /// ServiceImageConfig 带 security 的 round-trip：验证 security 字段 serde 贯通
    #[test]
    fn test_service_image_config_security_roundtrip() {
        let mut cfg = default_agent_runner_service_config();
        cfg.security = Some(ServiceSecurityConfig {
            privileged: Some(false),
            cap_add: Some(vec!["SYS_PTRACE".to_string()]),
            cap_drop: None,
            security_opt: Some(vec!["seccomp=unconfined".to_string()]),
            pids_limit: None,
            init: None,
        });
        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: ServiceImageConfig = serde_json::from_str(&json).unwrap();
        let sec = cfg2.security.expect("security preserved after roundtrip");
        assert_eq!(sec.cap_add, Some(vec!["SYS_PTRACE".to_string()]));
        assert_eq!(
            sec.security_opt,
            Some(vec!["seccomp=unconfined".to_string()])
        );
    }

    /// serde alias 兼容：旧字段名（memory_limit/cpu_limit/swap_limit）经 alias 反序列化到
    /// 新字段（memory/cpu/swap）。保证 config.yml 旧键名 + 旧 HTTP 请求不破坏。
    #[test]
    fn test_resource_limits_serde_alias() {
        // 旧字段名（config.yml 现状）经 alias 解析到新字段
        let json_old = r#"{"memory_limit":1e9,"cpu_limit":2.0,"swap_limit":2e9}"#;
        let limits_old: ServiceResourceLimits = serde_json::from_str(json_old).unwrap();
        assert_eq!(limits_old.memory, Some(1e9));
        assert_eq!(limits_old.cpu, Some(2.0));
        assert_eq!(limits_old.swap, Some(2e9));

        // 新字段名直接解析
        let json_new = r#"{"memory":1e9,"cpu":2.0,"swap":2e9}"#;
        let limits_new: ServiceResourceLimits = serde_json::from_str(json_new).unwrap();
        assert_eq!(limits_new.memory, Some(1e9));
        assert_eq!(limits_new.cpu, Some(2.0));
        assert_eq!(limits_new.swap, Some(2e9));

        // 序列化用新字段名（不带 _limit）
        let s = serde_json::to_string(&limits_old).unwrap();
        assert!(
            s.contains("\"memory\""),
            "serialized should use new field name: {s}"
        );
        assert!(
            !s.contains("memory_limit"),
            "serialized should not use alias: {s}"
        );
    }

    #[test]
    fn test_config_validation() {
        let mut config = default_rcoder_service_config();

        // 为测试设置镜像配置
        config.arm64_image = Some("test-image:arm64".to_string());
        config.amd64_image = Some("test-image:amd64".to_string());

        // 有效配置
        assert!(matches!(config.validate(), ConfigValidationResult::Valid));

        // 无效配置：所有镜像为空
        let mut invalid_config = config.clone();
        invalid_config.image = None;
        invalid_config.arm64_image = None;
        invalid_config.amd64_image = None;
        invalid_config.default_image = None;
        assert!(matches!(
            invalid_config.validate(),
            ConfigValidationResult::Error(_)
        ));
    }

    #[test]
    fn test_environment_merge() {
        let config = default_rcoder_service_config();

        let mut base_env = HashMap::new();
        base_env.insert("BASE_VAR".to_string(), "base_value".to_string());
        base_env.insert("RUST_LOG".to_string(), "debug".to_string()); // 重叠

        let merged = config.merge_environment(&base_env);

        assert_eq!(merged.get("BASE_VAR"), Some(&"base_value".to_string()));
        // 服务特定环境变量应该覆盖基础变量
        assert_eq!(merged.get("RUST_LOG"), Some(&"info".to_string())); // RCoder 配置是 info
        assert_eq!(merged.get("SERVICE_MODE"), Some(&"full".to_string()));
    }

    #[test]
    fn test_mount_validation() {
        // 创建一个有挂载点的配置用于测试
        let config_with_mounts = ServiceImageConfig {
            service_type: ServiceType::WebAgentRunner,
            image: None,
            arm64_image: Some("test-image:arm64".to_string()),
            amd64_image: Some("test-image:amd64".to_string()),
            default_image: Some("test-image:latest".to_string()),
            image_tag_prefix: None,
            enabled: true,
            environment: HashMap::new(),
            mounts: vec![ServiceMountConfig {
                container_path: "/app/workspace".to_string(),
                host_path: "/host/workspace".to_string(),
                read_only: false,
                mount_type: "bind".to_string(),
                resolve_from: None,
            }],
            command: vec![],
            entrypoint: None,
            resource_limits: ServiceResourceLimits::new(None, None, None, None, None),
            work_dir: "/app".to_string(),
            network_mode: "bridge".to_string(),
            container_path_template: "/app/project_workspace/{project_id}".to_string(),
            workspace_resolution_path: None,
            security: None,
        };

        for mount in &config_with_mounts.mounts {
            assert!(matches!(mount.validate(), ConfigValidationResult::Valid));
        }

        // 测试无效挂载
        let mut invalid_mount = config_with_mounts.mounts[0].clone();
        invalid_mount.container_path = "".to_string();
        assert!(matches!(
            invalid_mount.validate(),
            ConfigValidationResult::Error(_)
        ));
    }

    #[test]
    fn test_mount_path_resolution() {
        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-project-123".to_string());
        variables.insert("workspace_dir".to_string(), "/app/workspace".to_string());

        let mount = ServiceMountConfig {
            container_path: "/app/workspace/{project_id}".to_string(),
            host_path: "{workspace_dir}/projects/{project_id}".to_string(),
            read_only: false,
            mount_type: "bind".to_string(),
            resolve_from: None,
        };

        let resolved = mount.resolve_host_path(&variables);
        assert_eq!(resolved, "/app/workspace/projects/test-project-123");
    }

    #[test]
    fn test_get_summary() {
        let config = default_rcoder_service_config();
        let summary = config.get_summary();

        assert!(summary.contains("web-agent-runner"));
        assert!(summary.contains("Enabled: true"));
        // 镜像配置为空时，summary 不包含镜像地址
    }

    #[test]
    fn test_container_prefix_with_image_tag_prefix() {
        // 测试使用 image_tag_prefix 的情况
        let config = default_agent_runner_service_config();
        assert_eq!(config.container_prefix(), "computer-agent-runner");
    }

    #[test]
    fn test_container_prefix_fallback_to_service_type() {
        // 测试没有 image_tag_prefix 时回退到 service_type 默认值
        let mut config = default_rcoder_service_config();
        config.image_tag_prefix = None;
        assert_eq!(config.container_prefix(), "web-agent-runner");
    }

    #[test]
    fn test_container_prefix_rcoder() {
        // WebAgentRunner 配置使用 web-agent-runner 前缀
        let config = default_rcoder_service_config();
        assert_eq!(config.container_prefix(), "web-agent-runner");
    }

    /// 测试 ServiceType::container_prefix() 与 ServiceConfig::container_prefix() 的差异
    ///
    /// 这是导致 VNC 状态查询返回 CONTAINER_NOT_FOUND 的根因：
    /// - ServiceType::container_prefix() 返回硬编码的 "computer-agent-runner"
    /// - ServiceConfig::container_prefix() 读取配置的 image_tag_prefix "computer-agent-runner"
    /// - 容器创建使用后者，而错误的查询代码使用前者，导致名称不匹配
    #[test]
    fn test_container_prefix_difference_causes_container_not_found() {
        // 硬编码的 ServiceType 前缀（错误的查询方式）
        let service_type_prefix = ServiceType::ComputerAgentRunner.container_prefix();
        assert_eq!(service_type_prefix, "computer-agent-runner");

        // 配置化的 ServiceConfig 前缀（正确的创建方式）
        let config = default_agent_runner_service_config();
        let config_prefix = config.container_prefix();
        assert_eq!(config_prefix, "computer-agent-runner");

        // 两者应该相同
        assert_eq!(
            service_type_prefix, config_prefix,
            "ServiceType::container_prefix() 与 ServiceConfig::container_prefix() 应该相同"
        );

        // 展示如果用错误的前缀构造容器名会导致什么问题
        let user_id = "1743762321";
        let container_name = format!("{}-{}", service_type_prefix, user_id);

        assert_eq!(container_name, "computer-agent-runner-1743762321");
    }

    #[test]
    fn test_resource_limits_validation_valid() {
        let valid = ServiceResourceLimits {
            memory: Some(1_000_000_000.0), // 1GB
            cpu: Some(2.0),
            swap: Some(2_000_000_000.0), // 2GB
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn test_resource_limits_validation_invalid_memory_too_small() {
        let invalid = ServiceResourceLimits {
            memory: Some(256_000_000.0), // 256MB - 太小
            cpu: None,
            swap: None,
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(invalid.validate().unwrap_err().contains("at least 512MB"));
    }

    #[test]
    fn test_resource_limits_validation_invalid_memory_too_large() {
        let invalid = ServiceResourceLimits {
            memory: Some(100_000_000_000.0), // 100GB - 太大
            cpu: None,
            swap: None,
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("cannot exceed 64GB")
        );
    }

    #[test]
    fn test_resource_limits_validation_invalid_cpu_too_small() {
        let invalid = ServiceResourceLimits {
            memory: None,
            cpu: Some(0.1), // 太小
            swap: None,
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("at least 0.5 cores")
        );
    }

    #[test]
    fn test_resource_limits_normalize_swap_less_than_memory() {
        // swap < memory 不再导致 validate 失败(已改为自动规整)
        let rl = ServiceResourceLimits {
            memory: Some(2_000_000_000.0), // 2GB
            cpu: None,
            swap: Some(1_000_000_000.0), // 1GB < memory
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(rl.validate().is_ok());

        // normalize_swap:swap < memory → swap = memory × 2
        let (fixed, changed) = rl.normalize_swap();
        assert!(changed);
        assert_eq!(fixed.swap, Some(4_000_000_000.0));
        assert_eq!(fixed.memory, Some(2_000_000_000.0)); // memory 不变

        // swap >= memory 时不修正
        let ok = ServiceResourceLimits {
            memory: Some(2_000_000_000.0),
            cpu: None,
            swap: Some(4_000_000_000.0),
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        let (same, changed2) = ok.normalize_swap();
        assert!(!changed2);
        assert_eq!(same.swap, Some(4_000_000_000.0));
    }

    #[test]
    fn test_resource_limits_merge() {
        let default_limits = ServiceResourceLimits {
            memory: Some(2_000_000_000.0), // 2GB
            cpu: Some(2.0),
            swap: Some(4_000_000_000.0), // 4GB
            storage_size: None,
            ephemeral_storage_limit: None,
        };

        let override_limits = ServiceResourceLimits {
            memory: Some(4_000_000_000.0), // 覆盖：4GB
            cpu: None,                     // 不覆盖
            swap: Some(8_000_000_000.0),   // 覆盖：8GB
            storage_size: Some("20Gi".to_string()),
            ephemeral_storage_limit: None,
        };

        let merged = default_limits.merge_with(&override_limits);
        assert_eq!(merged.memory, Some(4_000_000_000.0));
        assert_eq!(merged.cpu, Some(2.0)); // 保留默认
        assert_eq!(merged.swap, Some(8_000_000_000.0));
        assert_eq!(merged.storage_size, Some("20Gi".to_string()));
    }

    #[test]
    fn test_resource_limits_merge_all_none() {
        let default_limits = ServiceResourceLimits {
            memory: Some(2_000_000_000.0), // 2GB
            cpu: Some(2.0),
            swap: Some(4_000_000_000.0), // 4GB
            storage_size: Some("10Gi".to_string()),
            ephemeral_storage_limit: None,
        };

        let override_limits = ServiceResourceLimits {
            memory: None,
            cpu: None,
            swap: None,
            storage_size: None,
            ephemeral_storage_limit: None,
        };

        let merged = default_limits.merge_with(&override_limits);
        // 所有字段都应该保留默认值
        assert_eq!(merged.memory, Some(2_000_000_000.0));
        assert_eq!(merged.cpu, Some(2.0));
        assert_eq!(merged.swap, Some(4_000_000_000.0));
        assert_eq!(merged.storage_size, Some("10Gi".to_string()));
    }

    #[test]
    fn test_workspace_resolution_path_rcoder() {
        let config = default_rcoder_service_config();
        // 未显式配置时，从 container_path_template 推导
        assert_eq!(
            config.effective_workspace_resolution_path(),
            "/app/project_workspace"
        );
    }

    #[test]
    fn test_workspace_resolution_path_computer_agent_runner() {
        let config = default_agent_runner_service_config();
        // 未显式配置时，从 container_path_template 推导
        assert_eq!(
            config.effective_workspace_resolution_path(),
            "/app/computer-project-workspace"
        );
    }

    #[test]
    fn test_workspace_resolution_path_explicit_override() {
        let mut config = default_rcoder_service_config();
        config.workspace_resolution_path = Some("/custom/path".to_string());
        assert_eq!(config.effective_workspace_resolution_path(), "/custom/path");
    }

    #[test]
    fn test_workspace_container_path_rcoder() {
        let config = default_rcoder_service_config();
        // RCoder: PROJECT_WORKSPACE_BASE="/app/project_workspace"
        assert_eq!(config.workspace_container_path(), "/app/project_workspace");
    }

    #[test]
    fn test_workspace_container_path_computer_agent_runner() {
        let config = default_agent_runner_service_config();
        // ComputerAgentRunner: PROJECT_WORKSPACE_BASE="/home/user"
        assert_eq!(config.workspace_container_path(), "/home/user");
    }

    #[test]
    fn test_workspace_container_path_fallback() {
        let mut config = default_rcoder_service_config();
        config.environment.remove("PROJECT_WORKSPACE_BASE");
        // 无环境变量时回退到 effective_workspace_resolution_path
        assert_eq!(
            config.workspace_container_path(),
            config.effective_workspace_resolution_path()
        );
    }
}
