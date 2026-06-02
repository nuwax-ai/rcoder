//! Agent Management API 类型定义 (P0-1)
//!
//! 这些类型用于 Agent 管理 API(列出已安装/上传/卸载/检查),由 rcoder 和 agent_runner 共享。
//! 详见 `docs/acp-agent-management-api.md` 附录 A。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 默认安装目录常量
pub const DEFAULT_ACP_AGENT_INSTALL_DIR: &str = "/home/user/acp-agent";

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
        let os = std::env::consts::OS.to_string();
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64".to_string(),
            "aarch64" => "arm64".to_string(),
            other => other.to_string(),
        };
        let platform = format!("{}/{}", os, arch);
        Self { os, arch, platform }
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
    /// 是否包含内置 agent(默认 true)
    #[serde(default = "default_true")]
    pub include_builtin: bool,
}

fn default_true() -> bool {
    true
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckAgentRequest {
    /// Agent ID
    pub agent_id: String,
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

/// URL 安装 Agent 的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstallFromUrlRequest {
    /// 下载 URL(http/https)
    pub url: String,
    /// Agent ID
    pub agent_id: String,
    /// 入口可执行文件名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// 启动参数
    #[serde(default)]
    pub args: Vec<String>,
    /// 校验和(SHA256,hex)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// 包管理器安装 Agent 的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstallFromPackageManagerRequest {
    /// Agent ID
    pub agent_id: String,
    /// npm 包名
    pub package: String,
    /// 版本约束(可选,缺省 latest)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 入口命令(可选,缺省用包名)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
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
}

/// 卸载 Agent 的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UninstallAgentRequest {
    /// Agent ID
    pub agent_id: String,
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
}

/// 列出默认 Agent 的响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListDefaultAgentsResponse {
    /// 默认 agent 列表
    pub default_agents: Vec<DefaultAgentInfo>,
}

/// 默认 Agent 信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DefaultAgentInfo {
    /// Agent ID
    pub agent_id: String,
    /// 显示名
    pub display_name: String,
    /// 描述
    pub description: String,
    /// 安装类型
    pub install_type: InstallType,
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
}
