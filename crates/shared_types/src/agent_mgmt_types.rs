//! Agent Management API 类型定义 (P0-1)
//!
//! 这些类型用于 Agent 管理 API(列出已安装/上传/卸载/检查),由 rcoder 和 agent_runner 共享。
//! 详见 `docs/acp-agent-management-api.md` 附录 A。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 多租户容器路由参数
///
/// 所有 `/agent-mgmt/*` 端点共享此参数，用于定位目标容器。
/// agent-runner 独立运行时忽略这些字段（`#[serde(default)]`）。
/// rcoder 通过这些字段确定转发到哪个容器。
///
/// ## 路由模式
///
/// **模式 A: project_id（向后兼容）**
/// ```json
/// { "project_id": "demo-project-001" }
/// ```
///
/// **模式 B: 多租户（pod_id + tenant/space）**
/// ```json
/// {
///   "pod_id": "pod-abc123",
///   "tenant_id": "tenant-001",
///   "space_id": "space-001",
///   "isolation_type": "tenant"
/// }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct RoutingParams {
    /// 项目 ID（与 user_id/pod_id 二选一）
    #[serde(default)]
    #[schema(example = "demo-project-001")]
    pub project_id: Option<String>,
    /// 用户 ID（ComputerAgentRunner 模式，定位容器）
    #[serde(default)]
    pub user_id: Option<String>,
    /// 容器复用标识（有值时覆盖 user_id 作为容器标识）
    #[serde(default)]
    pub pod_id: Option<String>,
    /// 租户 ID（pod_id 有值时必填，同时接受字符串和数字）
    #[serde(default, deserialize_with = "crate::flexible_string::flexible_string")]
    pub tenant_id: Option<String>,
    /// 空间 ID（pod_id 有值时必填，同时接受字符串和数字）
    #[serde(default, deserialize_with = "crate::flexible_string::flexible_string")]
    pub space_id: Option<String>,
    /// 隔离类型：tenant / space / project（pod_id 有值时必填）
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// 默认安装目录常量
pub const DEFAULT_ACP_AGENT_INSTALL_DIR: &str = "/home/user/acp-agent";

/// Agent 缓存目录常量（rcoder 统一下载缓存）
pub const AGENT_CACHE_DIR: &str = "/app/agent-cache";

/// 二进制上传时单 chunk 大小(1 MB)
pub const UPLOAD_CHUNK_SIZE: usize = 1024 * 1024;

/// 最大允许的二进制文件大小(1 GB)
pub const MAX_BINARY_SIZE: u64 = 1024 * 1024 * 1024;

/// 解压后累计字节上限(1 GB,防 zip bomb)
pub const MAX_EXTRACTED_SIZE: u64 = 1024 * 1024 * 1024;

/// URL 下载超时(10 分钟)
pub const URL_DOWNLOAD_TIMEOUT_SECS: u64 = 600;

/// 系统平台信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SystemInfo {
    /// 操作系统(如 "linux", "darwin", "windows")
    pub os: String,
    /// CPU 架构(如 "amd64", "arm64")
    pub arch: String,
    /// 平台标识(格式 "{os}/{arch}")
    pub platform: String,
}

impl SystemInfo {
    /// 从当前运行环境获取系统信息
    pub fn current() -> Self {
        let key = crate::version_util::PlatformKey::current();
        Self {
            os: key.os.to_string(),
            arch: key.arch.to_string(),
            platform: format!("{}/{}", key.os, key.arch),
        }
    }

    /// 转换为 PlatformKey 结构体
    pub fn to_platform_key(&self) -> Option<crate::version_util::PlatformKey> {
        crate::version_util::PlatformKey::new(&self.os, &self.arch)
    }
}

impl Default for SystemInfo {
    /// Fallback: agent_runner 未返回 system_info 时使用空对象(不应发生,仅 fail-safe)
    fn default() -> Self {
        Self {
            os: String::new(),
            arch: String::new(),
            platform: String::new(),
        }
    }
}

/// Agent 安装类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallType {
    /// 内置 agent(由镜像提供,不可卸载)
    Builtin,
    /// 用户上传的二进制文件(含 tar.gz/zip 解压)
    Binary,
    /// 通过 npm 全局安装
    Npm,
    /// 通过 URL 下载
    Url,
    /// Fallback: proto 转换失败时使用(fail-safe)
    #[default]
    Unknown,
}

/// Agent 安装状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentInstallStatus {
    /// 可用(已安装且可执行)
    Available,
    /// 损坏(文件丢失或不可执行)
    Broken,
    /// 未安装
    NotInstalled,
    /// Fallback: proto 转换失败时使用(fail-safe)
    #[default]
    Unknown,
}

/// Agent 注册表条目
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentInfo {
    /// Agent ID
    pub agent_id: String,
    /// 安装类型
    pub install_type: InstallType,
    /// 状态
    pub status: AgentInstallStatus,
    /// 版本(可选,部分 agent 无版本概念)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 二进制路径(可选)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// 安装时间(Unix timestamp 秒)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<i64>,
}

/// 列出已安装 Agent 的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct ListAgentsRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
}

/// 列出已安装 Agent 的响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListAgentsResponse {
    /// 系统平台信息
    pub system_info: SystemInfo,
    /// 已安装 agent 列表
    pub agents: Vec<AgentInfo>,
    /// 总数
    pub total: usize,
    /// 安装目录
    pub install_dir: String,
}

/// 静态检查结果
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct StaticCheckResult {
    /// 二进制文件存在
    pub file_exists: bool,
    /// 文件有可执行权限
    pub executable: bool,
    /// 目录在 PATH 中
    pub in_path: bool,
}

/// Agent 详情(包含静态检查)
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct AgentDetailInfo {
    /// Agent ID
    pub agent_id: String,
    /// 安装类型
    pub install_type: InstallType,
    /// 是否已安装
    pub installed: bool,
    /// 状态
    pub status: AgentInstallStatus,
    /// 版本(可选)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 是否支持版本检查
    pub version_check_supported: bool,
    /// 静态检查结果
    pub static_checks: StaticCheckResult,
}

/// 检查指定 Agent 状态的请求
///
/// ## 示例
///
/// ```json
/// {
///   "project_id": "demo-project-001",
///   "agent_id": "codex-acp"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct CheckAgentRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    /// Agent ID
    #[schema(example = "codex-acp")]
    pub agent_id: String,
    /// 可选版本号，不传则返回最新版本
    #[serde(default)]
    #[schema(example = "1.0.0")]
    pub version: Option<String>,
}

/// 查询单个 Agent 详情的请求
///
/// ## 示例
///
/// ```json
/// {
///   "project_id": "demo-project-001",
///   "agent_id": "codex-acp"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct GetAgentRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    /// Agent ID
    #[schema(example = "codex-acp")]
    pub agent_id: String,
    /// 可选版本号，不传则返回最新版本
    #[serde(default)]
    #[schema(example = "1.0.0")]
    pub version: Option<String>,
}

/// 检查指定 Agent 状态的响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckAgentResponse {
    /// 系统平台信息
    pub system_info: SystemInfo,
    /// Agent 详情
    pub agent: AgentDetailInfo,
}

/// 上传二进制 Agent 的请求 metadata
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstallBinaryRequest {
    /// Agent ID(可选,缺省从文件名推断)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// 入口可执行文件名(可选,缺省用 agent_id 或首个可执行文件)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// 启动参数(可选)
    #[serde(default)]
    pub args: Vec<String>,
    /// 校验和(SHA256,hex)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Agent 身份信息(所有安装端点共享)
///
/// 描述 agent 的标识、启动命令和版本，作为嵌套 `"agent"` 对象用于安装请求 JSON。
///
/// ## 示例
///
/// ```json
/// {
///   "agent_id": "codex-acp",
///   "command": "codex-acp",
///   "args": ["--serve", "--port", "7091"],
///   "version": "1.2.0"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct AgentIdentity {
    /// Agent ID(容器内唯一标识,如 "codex-acp", "kimi-cli")
    #[schema(example = "codex-acp")]
    pub agent_id: String,
    /// 入口可执行文件名(安装后可直接调用的命令,如 "codex-acp")
    #[schema(example = "codex-acp")]
    pub command: String,
    /// 启动参数(可选,传递给 agent 进程,默认空)
    #[serde(default)]
    #[schema(example = json!(["--serve", "--port", "7091"]))]
    pub args: Vec<String>,
    /// 期望安装的版本号(semver 格式,如 "1.2.0",可选)
    #[serde(default)]
    #[schema(example = "1.2.0")]
    pub version: Option<String>,
}

/// URL 安装 Agent 的请求(多平台 + 版本管理)
///
/// agent-runner 独立运行时直接使用此类型（路由字段忽略）。
/// rcoder 通过 `routing` 字段定位目标容器。
///
/// ## 多平台示例
///
/// ```json
/// {
///   "project_id": "demo-project-001",
///   "agent": {
///     "agent_id": "codex-acp",
///     "command": "codex-acp",
///     "args": ["--serve"],
///     "version": "1.2.0"
///   },
///   "platforms": {
///     "linux-x86_64": {
///       "url": "https://cdn.example.com/releases/codex-acp/1.2.0/codex-acp-linux-amd64.tar.gz",
///       "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
///       "size": 52428800
///     },
///     "linux-arm64": {
///       "url": "https://cdn.example.com/releases/codex-acp/1.2.0/codex-acp-linux-arm64.tar.gz",
///       "sha256": "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a",
///       "size": 49283072
///     },
///     "darwin-arm64": {
///       "url": "https://cdn.example.com/releases/codex-acp/1.2.0/codex-acp-darwin-arm64.tar.gz",
///       "sha256": "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592",
///       "size": 47185920
///     },
///     "windows-x86_64": {
///       "url": "https://cdn.example.com/releases/codex-acp/1.2.0/codex-acp-windows-amd64.zip",
///       "sha256": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae",
///       "size": 55574528
///     }
///   }
/// }
/// ```
///
/// ## 幂等行为
///
/// agent-runner 安装时自动判断:
/// - 已安装版本 >= 请求版本 → 返回 `action: "skipped"`
/// - 已安装版本 < 请求版本 → 下载更新,返回 `action: "updated"`
/// - 首次安装 → 返回 `action: "installed"`
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct InstallFromUrlRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    /// Agent 身份信息(agent_id, command, args, version)
    #[schema(example = json!({
        "agent_id": "codex-acp",
        "command": "codex-acp",
        "args": ["--serve", "--port", "7091"],
        "version": "1.2.0"
    }))]
    pub agent: AgentIdentity,
    /// 多平台下载信息映射
    ///
    /// key 为 `{os}-{arch}` 格式(如 `linux-x86_64`, `darwin-arm64`),
    /// value 包含该平台的下载 URL、SHA-256 校验和、文件大小。
    /// agent-runner 根据容器系统自动选择匹配的平台。
    ///
    /// 常用平台 key:
    /// - `linux-x86_64` — Linux AMD64 服务器
    /// - `linux-arm64` — Linux ARM64 (AWS Graviton)
    /// - `darwin-arm64` — macOS Apple Silicon
    /// - `darwin-x86_64` — macOS Intel
    /// - `windows-x86_64` — Windows AMD64
    #[schema(example = json!({
        "linux-x86_64": {
            "url": "https://cdn.example.com/agent/1.0.0/agent-linux-amd64.tar.gz",
            "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "size": 52428800
        },
        "linux-arm64": {
            "url": "https://cdn.example.com/agent/1.0.0/agent-linux-arm64.tar.gz",
            "sha256": "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a",
            "size": 49283072
        },
        "darwin-arm64": {
            "url": "https://cdn.example.com/agent/1.0.0/agent-darwin-arm64.tar.gz",
            "size": 47185920
        }
    }))]
    pub platforms: std::collections::HashMap<String, PlatformEntry>,
    /// 强制重新安装(取消正在进行的安装，重新开始)
    #[serde(default)]
    pub force: bool,
}

/// 包管理器安装 Agent 的请求
///
/// agent_runner 端调用 `npm install -g <package>` 全局安装，
/// 适用于官方 npm 发布的 agent（如 `@anthropic-ai/claude-code-acp`）。
///
/// ## 示例
///
/// ```json
/// {
///   "project_id": "demo-project-001",
///   "agent": {
///     "agent_id": "claude-code-acp",
///     "command": "claude-code-acp"
///   },
///   "package": "@anthropic-ai/claude-code-acp"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct InstallFromPackageManagerRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    /// Agent 身份信息(agent_id, command; args 默认空)
    pub agent: AgentIdentity,
    /// npm 包名(含 scope,如 `@anthropic-ai/claude-code-acp`)
    #[schema(example = "@anthropic-ai/claude-code-acp")]
    pub package: String,
}

/// 安装响应(上传/URL/npm 通用)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstallAgentResponse {
    /// Agent ID
    pub agent_id: String,
    /// 安装状态
    pub status: AgentInstallStatus,
    /// 二进制路径
    pub binary_path: String,
    /// 文件类型("executable" / "tar.gz" / "zip")
    pub file_type: String,
    /// 文件数量(压缩包解压后)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count: Option<usize>,
    /// 文件大小(字节)
    pub file_size: u64,
    /// 版本(若可检测)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 源 URL(URL 安装时)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    // === 多平台版本管理字段 ===
    /// 本次操作类型
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<InstallAction>,
    /// 本次是否实际执行了下载安装
    #[serde(default)]
    pub installed: bool,
    /// 更新前的版本号(首次安装为 None)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    /// 实际匹配的平台 key(如 "linux-x86_64")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// 平台下载信息(platforms map 的值)
///
/// 每个 `PlatformEntry` 描述特定 OS + CPU 架构组合的下载信息。
/// 平台 key 格式为 `{os}-{arch}`，常见值:
///
/// | Key | OS | CPU 架构 | 说明 |
/// |-----|-----|---------|------|
/// | `linux-x86_64` | Linux | x86_64/AMD64 | 服务器主流 |
/// | `linux-arm64` | Linux | ARM64/AArch64 | AWS Graviton、M1/M2 Docker |
/// | `darwin-arm64` | macOS | ARM64/AArch64 | Apple Silicon (M1/M2/M3) |
/// | `darwin-x86_64` | macOS | Intel | Intel Mac |
/// | `windows-x86_64` | Windows | x86_64/AMD64 | Windows 桌面 |
///
/// 安装时 agent-runner 根据容器系统自动匹配 key，未匹配到则返回
/// `ERR_AGENT_MGMT_PLATFORM_NOT_FOUND` 错误。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PlatformEntry {
    /// 下载 URL(http/https)
    #[schema(example = "https://cdn.example.com/agent-linux-amd64.tar.gz")]
    pub url: String,
    /// SHA-256 校验和(hex,可选,提供时安装后校验文件完整性)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2")]
    pub sha256: Option<String>,
    /// 文件大小(字节,可选,用于磁盘空间预检查)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 52428800)]
    pub size: Option<u64>,
}

/// 安装操作类型
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallAction {
    /// 首次安装
    Installed,
    /// 从旧版本升级
    Updated,
    /// 跳过(已安装版本 >= 请求版本)
    Skipped,
}

impl InstallAction {
    /// 转换为字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Updated => "updated",
            Self::Skipped => "skipped",
        }
    }
}

impl std::str::FromStr for InstallAction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "installed" => Ok(Self::Installed),
            "updated" => Ok(Self::Updated),
            "skipped" => Ok(Self::Skipped),
            other => Err(format!("unknown InstallAction: {other:?}")),
        }
    }
}

/// 卸载 Agent 的请求
///
/// ## 示例
///
/// ```json
/// {
///   "project_id": "demo-project-001",
///   "agent_id": "codex-acp"
/// }
/// ```
///
/// ## 注意
///
/// 内置 agent（`default-agents` 列表中）受保护，卸载会返回
/// `403 ERR_AGENT_MGMT_BUILTIN_PROTECTED`。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct UninstallAgentRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    /// Agent ID(要卸载的 agent 标识)
    #[schema(example = "codex-acp")]
    pub agent_id: String,
    /// 可选版本号，不传则卸载全部版本
    #[serde(default)]
    #[schema(example = "1.0.0")]
    pub version: Option<String>,
}

/// 卸载 Agent 的响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UninstallAgentResponse {
    /// 是否已卸载
    pub uninstalled: bool,
    /// 被卸载的安装类型
    pub install_type: InstallType,
    /// 被卸载的 agent_id
    pub agent_id: String,
    /// 被卸载的版本列表
    #[serde(default)]
    pub removed_versions: Vec<String>,
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_info_current_is_consistent() {
        let info = SystemInfo::current();
        assert!(!info.os.is_empty());
        assert!(!info.arch.is_empty());
        assert_eq!(info.platform, format!("{}/{}", info.os, info.arch));
    }

    #[test]
    fn system_info_serde_round_trip() {
        let info = SystemInfo {
            os: "linux".to_string(),
            arch: "arm64".to_string(),
            platform: "linux/arm64".to_string(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: SystemInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.os, info.os);
        assert_eq!(parsed.arch, info.arch);
        assert_eq!(parsed.platform, info.platform);
    }

    #[test]
    fn install_type_serde_uses_snake_case() {
        let json = serde_json::to_string(&InstallType::Npm).unwrap();
        assert_eq!(json, "\"npm\"");
    }

    #[test]
    fn agent_status_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&AgentInstallStatus::NotInstalled).unwrap(),
            "\"not_installed\""
        );
    }

    #[test]
    fn agent_identity_serde_round_trip() {
        let identity = AgentIdentity {
            agent_id: "codex-acp".to_string(),
            command: "codex-acp".to_string(),
            args: vec!["--serve".to_string()],
            version: Some("1.2.0".to_string()),
        };
        let json = serde_json::to_string(&identity).unwrap();
        let parsed: AgentIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, identity.agent_id);
        assert_eq!(parsed.command, identity.command);
        assert_eq!(parsed.args, identity.args);
        assert_eq!(parsed.version, identity.version);
    }

    #[test]
    fn agent_identity_defaults_args_and_version() {
        let identity: AgentIdentity =
            serde_json::from_str(r#"{"agent_id":"x","command":"x"}"#).unwrap();
        assert!(identity.args.is_empty());
        assert!(identity.version.is_none());
    }
}
