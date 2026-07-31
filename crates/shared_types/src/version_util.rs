//! 版本号和平台工具函数
//!
//! - `normalize_version` / `parse_semver` / `compare_versions`: semver 版本处理
//! - `PlatformKey`: 平台标识符结构体，winnow 解析 + 归一化

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use semver::Version;
use winnow::ascii::alpha1;
use winnow::prelude::*;

// =============================================================================
// 版本号处理
// =============================================================================

/// 版本号解析错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionParseError {
    pub input: String,
}

impl fmt::Display for VersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid semver version: '{}' (expected format: X.Y.Z, e.g. 1.0.0)",
            self.input
        )
    }
}

impl std::error::Error for VersionParseError {}

/// 版本字符串归一化：去 v/V 前缀 + trim + semver 标准化
///
/// 通过 `semver::Version` 结构体获取标准形式，确保
/// "v1.0.0"、"V1.0.0"、" 1.0.0 " 都归一化为 "1.0.0"。
///
/// 非法版本号返回 `VersionParseError`（Fail Fast）。
pub fn normalize_version(version: &str) -> Result<String, VersionParseError> {
    parse_semver(version)
        .map(|v| v.to_string())
        .ok_or_else(|| VersionParseError {
            input: version.to_string(),
        })
}

/// 解析版本字符串为 semver 结构体，支持 "v"/"V" 前缀
///
/// 返回 None 表示版本格式无效或为空。
pub fn parse_semver(version: &str) -> Option<Version> {
    let v = version.trim();
    let v = v
        .strip_prefix('v')
        .or_else(|| v.strip_prefix('V'))
        .unwrap_or(v);
    Version::parse(v).ok()
}

/// 比较两个版本（semver 结构体比较）
///
/// 如果任一版本不是合法的 semver 格式，返回 `Err(VersionParseError)`。
/// 调用方应处理错误，或使用 `unwrap_or(Ordering::Equal)` 做兜底。
pub fn compare_versions(a: &str, b: &str) -> Result<Ordering, VersionParseError> {
    let a_ver = parse_semver(a).ok_or_else(|| VersionParseError {
        input: a.to_string(),
    })?;
    let b_ver = parse_semver(b).ok_or_else(|| VersionParseError {
        input: b.to_string(),
    })?;
    Ok(a_ver.cmp(&b_ver))
}

// =============================================================================
// PlatformKey — 平台标识符结构体
// =============================================================================

/// 支持的操作系统
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Linux,
    Darwin,
    Windows,
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Os::Linux => write!(f, "linux"),
            Os::Darwin => write!(f, "darwin"),
            Os::Windows => write!(f, "windows"),
        }
    }
}

/// 支持的 CPU 架构（归一化后只有 x86_64 和 arm64）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    X86_64,
    Arm64,
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arch::X86_64 => write!(f, "x86_64"),
            Arch::Arm64 => write!(f, "arm64"),
        }
    }
}

/// 平台标识符：`{os}-{arch}`
///
/// 支持的 6 种平台：
/// - `linux-x86_64` — Linux AMD64 服务器
/// - `linux-arm64` — Linux ARM64 (AWS Graviton)
/// - `darwin-arm64` — macOS Apple Silicon
/// - `darwin-x86_64` — macOS Intel
/// - `windows-x86_64` — Windows AMD64
/// - `windows-arm64` — Windows ARM64
///
/// 归一化规则：
/// - `amd64` → `x86_64`，`aarch64` → `arm64`
/// - 大小写不敏感（`Linux` → `linux`，`AMD64` → `x86_64`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlatformKey {
    pub os: Os,
    pub arch: Arch,
}

impl PlatformKey {
    /// 从 os 和 arch 字符串构造（自动归一化）
    pub fn new(os: &str, arch: &str) -> Option<Self> {
        let os = match os.to_ascii_lowercase().as_str() {
            "linux" => Os::Linux,
            "darwin" | "macos" => Os::Darwin, // macOS 有时报告为 "macos"
            "windows" => Os::Windows,
            _ => return None,
        };
        let arch = match arch.to_ascii_lowercase().as_str() {
            "x86_64" | "amd64" => Arch::X86_64,
            "arm64" | "aarch64" => Arch::Arm64,
            _ => return None,
        };
        Some(Self { os, arch })
    }

    /// 从当前运行环境构造
    ///
    /// `std::env::consts::OS/ARCH` 是编译期常量，对受支持的 6 个平台组合
    /// (linux/darwin/windows × x86_64/aarch64) `new()` 恒成功；此处 `.expect`
    /// 保留是因为它对受支持目标在运行时不可能失败，且无合理的兜底平台可回退。
    pub fn current() -> Self {
        Self::new(std::env::consts::OS, std::env::consts::ARCH)
            .expect("current platform should always be valid")
    }

    /// 作为 HashMap key 使用的字符串（归一化形式）
    pub fn as_key(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for PlatformKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.os, self.arch)
    }
}

impl FromStr for PlatformKey {
    type Err = PlatformParseError;

    /// 从字符串解析平台 key（如 "linux-x86_64"、"darwin-arm64"）
    ///
    /// 使用 winnow parser combinator 解析，支持归一化。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        platform_key_parser.parse(s).map_err(|_| PlatformParseError)
    }
}

/// 平台 key 解析错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformParseError;

impl fmt::Display for PlatformParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid platform key, expected format: {{os}}-{{arch}} (e.g. linux-x86_64)"
        )
    }
}

impl std::error::Error for PlatformParseError {}

/// winnow parser: 解析 `{os}-{arch}` 格式的平台 key
///
/// 输入格式: "linux-x86_64", "darwin-arm64", "windows-amd64" 等
/// 归一化: amd64 → x86_64, aarch64 → arm64, 大小写不敏感
fn platform_key_parser(input: &mut &str) -> winnow::ModalResult<PlatformKey> {
    // 1. 解析 os 部分（纯字母，大小写不敏感）
    let os_raw: &str = alpha1.parse_next(input)?;
    let os_lower = os_raw.to_ascii_lowercase();
    // 2. 解析分隔符
    '-'.parse_next(input)?;
    // 3. 解析 arch 部分（字母+数字+下划线，如 x86_64，大小写不敏感）
    let arch_raw: &str = winnow::combinator::repeat::<_, _, String, _, _>(
        1..,
        winnow::token::one_of(|c: char| c.is_ascii_alphanumeric() || c == '_'),
    )
    .take()
    .parse_next(input)?;
    let arch_lower = arch_raw.to_ascii_lowercase();

    // 4. 归一化 os
    let os = match os_lower.as_str() {
        "linux" => Os::Linux,
        "darwin" | "macos" => Os::Darwin,
        "windows" => Os::Windows,
        _ => {
            return Err(winnow::error::ErrMode::Backtrack(
                winnow::error::ContextError::new(),
            ));
        }
    };

    // 5. 归一化 arch
    let arch = match arch_lower.as_str() {
        "x86_64" | "amd64" => Arch::X86_64,
        "arm64" | "aarch64" => Arch::Arm64,
        _ => {
            return Err(winnow::error::ErrMode::Backtrack(
                winnow::error::ContextError::new(),
            ));
        }
    };

    Ok(PlatformKey { os, arch })
}

/// 归一化平台 key 字符串（便捷函数）
///
/// 等价于 `PlatformKey::new(os, arch).map(|k| k.to_string())`。
/// 如果 os 或 arch 不识别，回退到原始拼接。
pub fn normalize_platform_key(os: &str, arch: &str) -> String {
    PlatformKey::new(os, arch)
        .map(|k| k.to_string())
        .unwrap_or_else(|| format!("{}-{}", os, arch))
}

// =============================================================================
// 测试
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- version tests ---

    #[test]
    fn normalize_strips_v_prefix() {
        assert_eq!(normalize_version("v1.0.0").unwrap(), "1.0.0");
        assert_eq!(normalize_version("V2.0.0").unwrap(), "2.0.0");
    }

    #[test]
    fn normalize_trims_whitespace() {
        assert_eq!(normalize_version(" 1.0.0 ").unwrap(), "1.0.0");
        assert_eq!(normalize_version("  v1.0.0  ").unwrap(), "1.0.0");
    }

    #[test]
    fn normalize_passthrough() {
        assert_eq!(normalize_version("1.0.0").unwrap(), "1.0.0");
        assert_eq!(normalize_version("1.2.3-beta").unwrap(), "1.2.3-beta");
    }

    #[test]
    fn normalize_rejects_invalid() {
        assert!(normalize_version("vabc").is_err());
        assert!(normalize_version("").is_err());
        assert!(normalize_version("latest").is_err());
        assert!(normalize_version("abc").is_err());
    }

    #[test]
    fn parse_semver_valid() {
        assert!(parse_semver("1.0.0").is_some());
        assert!(parse_semver("v1.0.0").is_some());
        assert!(parse_semver("V1.0.0").is_some());
        assert!(parse_semver("1.2.3-beta").is_some());
    }

    #[test]
    fn parse_semver_invalid() {
        assert!(parse_semver("").is_none());
        assert!(parse_semver("abc").is_none());
    }

    #[test]
    fn compare_ordering() {
        assert_eq!(compare_versions("1.0.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "1.0.1").unwrap(), Ordering::Less);
        assert_eq!(
            compare_versions("1.0.1", "1.0.0").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("v1.0.0", "1.0.0").unwrap(),
            Ordering::Equal
        );
    }

    #[test]
    fn compare_invalid_returns_err() {
        assert!(compare_versions("invalid", "0.0.0").is_err());
        assert!(compare_versions("1.0.0", "not-a-version").is_err());
        assert!(compare_versions("", "").is_err());
    }

    // --- PlatformKey tests ---

    #[test]
    fn platform_key_parse_basic() {
        let key: PlatformKey = "linux-x86_64".parse().unwrap();
        assert_eq!(key.os, Os::Linux);
        assert_eq!(key.arch, Arch::X86_64);
        assert_eq!(key.to_string(), "linux-x86_64");
    }

    #[test]
    fn platform_key_parse_all_6_platforms() {
        for s in &[
            "linux-x86_64",
            "linux-arm64",
            "darwin-arm64",
            "darwin-x86_64",
            "windows-x86_64",
            "windows-arm64",
        ] {
            let key: PlatformKey = s.parse().unwrap();
            assert_eq!(key.to_string(), *s);
        }
    }

    #[test]
    fn platform_key_normalizes_amd64() {
        let key: PlatformKey = "linux-amd64".parse().unwrap();
        assert_eq!(key.arch, Arch::X86_64);
        assert_eq!(key.to_string(), "linux-x86_64");
    }

    #[test]
    fn platform_key_normalizes_aarch64() {
        let key: PlatformKey = "darwin-aarch64".parse().unwrap();
        assert_eq!(key.arch, Arch::Arm64);
        assert_eq!(key.to_string(), "darwin-arm64");
    }

    #[test]
    fn platform_key_case_insensitive() {
        let key: PlatformKey = "Linux-AMD64".parse().unwrap();
        assert_eq!(key.to_string(), "linux-x86_64");
    }

    #[test]
    fn platform_key_invalid_os() {
        assert!("freebsd-x86_64".parse::<PlatformKey>().is_err());
    }

    #[test]
    fn platform_key_macos_alias() {
        // "macos" 是 "darwin" 的别名（std::env::consts::OS 在 macOS 上返回 "macos"）
        let key: PlatformKey = "macos-arm64".parse().unwrap();
        assert_eq!(key.os, Os::Darwin);
        assert_eq!(key.to_string(), "darwin-arm64");
    }

    #[test]
    fn platform_key_invalid_arch() {
        assert!("linux-riscv64".parse::<PlatformKey>().is_err());
    }

    #[test]
    fn platform_key_from_system_info() {
        let key = PlatformKey::new("linux", "amd64").unwrap();
        assert_eq!(key.os, Os::Linux);
        assert_eq!(key.arch, Arch::X86_64);
    }

    #[test]
    fn platform_key_as_hashmap_key() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        let k1 = PlatformKey::new("linux", "amd64").unwrap();
        let k2 = PlatformKey::new("linux", "x86_64").unwrap();
        map.insert(k1, "value1");
        // 同一平台不同输入 → 相同 key
        assert_eq!(map.get(&k2), Some(&"value1"));
    }

    #[test]
    fn normalize_platform_key_fallback() {
        // 未知平台回退到原始拼接
        assert_eq!(
            normalize_platform_key("freebsd", "x86_64"),
            "freebsd-x86_64"
        );
    }
}
