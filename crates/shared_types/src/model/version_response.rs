//! 版本信息响应类型
//!
//! `/version` 端点的统一响应结构：服务版本号 + 跨平台系统信息
//! （编译期 os/arch + 运行时 OS 版本/内核版本，基于 sysinfo 采集）。

use utoipa::ToSchema;

use crate::SystemInfo;

/// 版本信息响应结构
///
/// 供 `/version` 端点返回服务版本与系统基础信息。
///
/// 运行时字段的平台语义：
/// - `os_version` / `os_release`：用户可感知的系统版本（Linux 为发行版版本 /
///   "Ubuntu 22.04 LTS"；macOS 为 "15.1" / "macOS 15.1 Sequoia"；Windows 为
///   "10.0.x" / "Windows 10 Pro"）
/// - `kernel_version`：Linux/macOS 为内核版本（容器内为**宿主内核**，容器共享
///   内核）；Windows 为 NT 内核 build 号
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ToSchema)]
pub struct VersionResponse {
    /// 服务名称
    #[schema(example = "rcoder-ai-service")]
    pub service: String,

    /// 服务版本号（Cargo 包版本）
    #[schema(example = "0.1.3")]
    pub version: String,

    /// 编译期系统平台信息（os/arch）
    pub system_info: SystemInfo,

    /// 操作系统版本（用户可感知，如 "22.04" / "15.1" / "10.0.19045"）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "22.04")]
    pub os_version: Option<String>,

    /// 操作系统长版本名（如 "Ubuntu 22.04 LTS" / "macOS 15.1 Sequoia"）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "Ubuntu 22.04 LTS")]
    pub os_release: Option<String>,

    /// 内核版本（Linux 容器内为宿主内核；Windows 为 NT build 号）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "6.8.0-45-generic")]
    pub kernel_version: Option<String>,
}

impl VersionResponse {
    /// 构造版本信息响应
    ///
    /// `version` 由调用方传入（如 rcoder 的 `env!("CARGO_PKG_VERSION")`，
    /// 语义上服务版本属于服务自身），系统信息在构造时经 sysinfo 跨平台采集。
    pub fn new(service: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            version: version.into(),
            system_info: SystemInfo::current(),
            os_version: sysinfo::System::os_version(),
            os_release: sysinfo::System::long_os_version(),
            kernel_version: sysinfo::System::kernel_version(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_passes_service_and_version_through() {
        let resp = VersionResponse::new("rcoder-ai-service", "0.1.3");
        assert_eq!(resp.service, "rcoder-ai-service");
        assert_eq!(resp.version, "0.1.3");
    }

    #[test]
    fn new_collects_compile_time_platform() {
        let resp = VersionResponse::new("svc", "1.0.0");
        assert!(!resp.system_info.os.is_empty());
        assert!(!resp.system_info.arch.is_empty());
        assert_eq!(
            resp.system_info.platform,
            format!("{}/{}", resp.system_info.os, resp.system_info.arch)
        );
    }

    #[test]
    fn new_collects_kernel_version_on_unix() {
        // sysinfo 静态采集在主流桌面/服务器环境返回 Some；极简 CI 容器可能
        // None，仅对 Linux/macOS 断言，Windows 行为留待有环境时验证。
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let resp = VersionResponse::new("svc", "1.0.0");
            assert!(
                resp.kernel_version.is_some(),
                "kernel_version expected on unix platform"
            );
        }
    }

    #[test]
    fn serde_round_trip() {
        let resp = VersionResponse {
            service: "rcoder-ai-service".to_string(),
            version: "0.1.3".to_string(),
            system_info: SystemInfo {
                os: "linux".to_string(),
                arch: "arm64".to_string(),
                platform: "linux/arm64".to_string(),
            },
            os_version: Some("22.04".to_string()),
            os_release: Some("Ubuntu 22.04 LTS".to_string()),
            kernel_version: Some("6.8.0-45-generic".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: VersionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.service, resp.service);
        assert_eq!(parsed.version, resp.version);
        assert_eq!(parsed.system_info.platform, resp.system_info.platform);
        assert_eq!(parsed.os_version, resp.os_version);
        assert_eq!(parsed.os_release, resp.os_release);
        assert_eq!(parsed.kernel_version, resp.kernel_version);
    }

    #[test]
    fn serde_omits_none_runtime_fields() {
        let resp = VersionResponse {
            service: "svc".to_string(),
            version: "1.0.0".to_string(),
            system_info: SystemInfo::default(),
            os_version: None,
            os_release: None,
            kernel_version: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("os_version"));
        assert!(!json.contains("os_release"));
        assert!(!json.contains("kernel_version"));
    }
}
