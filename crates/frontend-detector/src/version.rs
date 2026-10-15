//! 框架版本三级提取（Netlify `VersionAccuracy` 口径，精确度递减）：
//!
//! 1. `installed`：`node_modules/{pkg}/package.json` 的 `version` 字段（实际
//!    安装版本，最准——Vercel 安装后同款做法）
//! 2. `declared_pinned`：package.json 声明值本身是精确三段版本（如 "16.2.12"）
//! 3. `declared_range`：从 semver range 提取首个 x.y.z（如 "^5.4.21" → 5.4.21，
//!    仅最低位可信——`^` 下实际可能装到更高 patch/minor）
//!
//! 怪异声明（git tag / workspace:* / file: / dist-tag）提取不到 → `version`
//! 置空、`declared_range` 原样保留（信息不丢）。

/// 版本来源（精确度标注，随结果返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionSource {
    Installed,
    DeclaredPinned,
    DeclaredRange,
    None,
}

impl VersionSource {
    /// wire 名（snake_case）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::DeclaredPinned => "declared_pinned",
            Self::DeclaredRange => "declared_range",
            Self::None => "none",
        }
    }
}

/// 版本解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVersion {
    /// package.json 声明原样（range 或精确版本；未声明为空串）。
    pub declared_range: String,
    /// best effort 版本（三级口径；提取不到为 None）。
    pub version: Option<String>,
    pub source: VersionSource,
}

/// 三级提取入口。
pub fn resolve(dir: &std::path::Path, package: &str, declared: Option<&str>) -> ResolvedVersion {
    // 1. installed：node_modules 实测（部分安装时逐包 miss 即降级——天然容错）
    if let Some(installed) = read_installed_version(dir, package) {
        return ResolvedVersion {
            declared_range: declared.unwrap_or_default().to_string(),
            version: Some(installed),
            source: VersionSource::Installed,
        };
    }
    let Some(declared) = declared else {
        return ResolvedVersion {
            declared_range: String::new(),
            version: None,
            source: VersionSource::None,
        };
    };
    // 2. 精确三段版本（pinned）
    if let Some(pinned) = extract_exact(declared) {
        return ResolvedVersion {
            declared_range: declared.to_string(),
            version: Some(pinned),
            source: VersionSource::DeclaredPinned,
        };
    }
    // 3. range 提取（^/~/>=/v 前缀、npm: alias 兼容）
    if let Some(from_range) = extract_from_range(declared) {
        return ResolvedVersion {
            declared_range: declared.to_string(),
            version: Some(from_range),
            source: VersionSource::DeclaredRange,
        };
    }
    ResolvedVersion {
        declared_range: declared.to_string(),
        version: None,
        source: VersionSource::None,
    }
}

/// `node_modules/{pkg}/package.json` 的 version 字段（不存在/损坏 → None）。
fn read_installed_version(dir: &std::path::Path, package: &str) -> Option<String> {
    // 包名含 / 的（@sveltejs/kit）——node_modules 目录按 scope 布局，直接拼即可
    let path = dir.join("node_modules").join(package).join("package.json");
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value
        .get("version")?
        .as_str()
        .map(str::to_string)
        .filter(|version| !version.is_empty())
}

/// 精确三段版本判定（"16.2.12"，允许 v 前缀）。
fn extract_exact(declared: &str) -> Option<String> {
    let trimmed = declared.trim();
    let candidate = trimmed.strip_prefix('v').unwrap_or(trimmed);
    if is_exact_semver(candidate) {
        Some(candidate.to_string())
    } else {
        None
    }
}

fn is_exact_semver(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// 从 range 提取首个 x.y.z（泛化正则，nuwax parseVueMajorVersion 的三段扩展）：
/// 兼容 `^1.2.3` / `~1.2.3` / `>=1.2.3` / `1.x` / `v1.2.0` / `npm:vue@^3.4.0`。
/// 非 semver 形态（git tag / workspace:* / file: / latest）返回 None。
fn extract_from_range(declared: &str) -> Option<String> {
    let mut normalized = declared.trim();
    // npm alias："npm:vue@^3.4.0" → 取 @ 后的版本段
    if let Some(rest) = normalized.strip_prefix("npm:")
        && let Some((_, version)) = rest.rsplit_once('@')
    {
        normalized = version;
    }
    // 显式非 semver 协议前缀直接放弃
    if normalized
        .trim_start_matches(['^', '~', '>', '<', '=', 'v', ' '])
        .starts_with("workspace:")
        || normalized.starts_with("git")
        || normalized.starts_with("file:")
        || normalized.starts_with("http")
        || normalized.starts_with("latest")
    {
        return None;
    }
    // 首个 x.y.z 捕获（x 段也接受——"2.x" 补 .0）
    let mut start = 0;
    let bytes = normalized.as_bytes();
    while start < bytes.len() && !bytes[start].is_ascii_digit() {
        start += 1;
    }
    let rest = &normalized[start..];
    let mut end = 0;
    let mut dots = 0;
    for (offset, ch) in rest.char_indices() {
        if ch.is_ascii_digit() {
            end = offset + ch.len_utf8();
        } else if ch == '.' && dots < 2 {
            dots += 1;
        } else {
            break;
        }
    }
    let candidate = &rest[..end];
    if candidate.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = candidate
        .split('.')
        .map(|part| {
            if part == "x" || part == "X" || part == "*" {
                "0".to_string()
            } else {
                part.to_string()
            }
        })
        .collect();
    while parts.len() < 3 {
        parts.push("0".to_string());
    }
    Some(parts.join("."))
}

/// 主版本号提取（vue2/vue3 细分用；提取不到 → None）。
pub fn major_of(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_without_node_modules() -> std::path::PathBuf {
        tempfile::tempdir().expect("tempdir").keep()
    }

    #[test]
    fn three_tier_fallback_chain() {
        let dir = dir_without_node_modules();
        // declared_range：^ 前缀提取
        let r = resolve(&dir, "vite", Some("^5.4.21"));
        assert_eq!(r.version.as_deref(), Some("5.4.21"));
        assert_eq!(r.source, VersionSource::DeclaredRange);
        assert_eq!(r.declared_range, "^5.4.21");
        // declared_pinned：精确三段
        let r = resolve(&dir, "next", Some("16.2.12"));
        assert_eq!(r.version.as_deref(), Some("16.2.12"));
        assert_eq!(r.source, VersionSource::DeclaredPinned);
        // installed 优先于声明
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("node_modules/vite")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/vite/package.json"),
            r#"{"version": "5.4.11"}"#,
        )
        .unwrap();
        let r = resolve(dir.path(), "vite", Some("^5.9.0"));
        assert_eq!(r.version.as_deref(), Some("5.4.11"));
        assert_eq!(r.source, VersionSource::Installed);
        assert_eq!(r.declared_range, "^5.9.0");
    }

    #[test]
    fn exotic_ranges_keep_declared_but_no_version() {
        let dir = dir_without_node_modules();
        for exotic in [
            "workspace:*",
            "github:user/repo#main",
            "file:../local",
            "latest",
        ] {
            let r = resolve(&dir, "x", Some(exotic));
            assert_eq!(r.version, None, "{exotic}");
            assert_eq!(r.source, VersionSource::None, "{exotic}");
            assert_eq!(r.declared_range, exotic, "声明原样保留");
        }
    }

    #[test]
    fn range_forms_supported() {
        let dir = dir_without_node_modules();
        let cases = [
            ("^5.4.21", "5.4.21"),
            ("~1.2.3", "1.2.3"),
            (">=3", "3.0.0"),
            ("2.x", "2.0.0"),
            ("v3.2.0", "3.2.0"),
            ("npm:vue@^3.4.0", "3.4.0"),
            ("18.2.0", "18.2.0"),
        ];
        for (declared, expected) in cases {
            let r = resolve(&dir, "x", Some(declared));
            assert_eq!(r.version.as_deref(), Some(expected), "declared={declared}");
        }
    }

    #[test]
    fn scoped_package_installed_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("node_modules/@sveltejs/kit")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/@sveltejs/kit/package.json"),
            r#"{"version": "2.5.0"}"#,
        )
        .unwrap();
        let r = resolve(dir.path(), "@sveltejs/kit", Some("^2.0.0"));
        assert_eq!(r.version.as_deref(), Some("2.5.0"));
        assert_eq!(r.source, VersionSource::Installed);
    }

    #[test]
    fn major_of_variants() {
        assert_eq!(major_of("3.4.0"), Some(3));
        assert_eq!(major_of("2.0.0"), Some(2));
        assert_eq!(major_of("latest"), None);
    }
}
