//! package.json 最小解析（通行做法：手写 serde struct 只取所需字段——
//! turborepo/Zed 等 Rust JS 工具链同款，不引第三方类型库）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// package.json 探测所需的最小字段集。
#[derive(Debug, Default, Deserialize)]
pub struct PackageJsonMinimal {
    #[serde(default, rename = "name")]
    pub name: Option<String>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    pub dev_dependencies: BTreeMap<String, String>,
    /// `"packageManager": "pnpm@9.1.0"`（Corepack 字段）。
    #[serde(default, rename = "packageManager")]
    pub package_manager: Option<String>,
}

impl PackageJsonMinimal {
    /// 读 `{dir}/package.json`：文件不存在或 JSON 损坏均返回 `None`
    /// （探测是尽力而为的观察通道，损坏降级由调用方记 warn）。
    pub fn read(dir: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(dir.join("package.json")).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// 合并 dependencies + devDependencies（框架可能在任一段声明）。
    pub fn merged_dependencies(&self) -> impl Iterator<Item = (&str, &str)> {
        self.dependencies
            .iter()
            .chain(self.dev_dependencies.iter())
            .map(|(name, range)| (name.as_str(), range.as_str()))
    }

    /// 查某依赖的声明版本（deps 优先于 devDeps）。
    pub fn dependency(&self, package: &str) -> Option<&str> {
        self.dependencies
            .get(package)
            .or_else(|| self.dev_dependencies.get(package))
            .map(String::as_str)
    }

    /// 任一依赖名命中（some 语义）。
    pub fn has_any(&self, packages: &[&str]) -> bool {
        packages
            .iter()
            .any(|package| self.dependency(package).is_some())
    }

    /// 全部依赖名命中（every 语义）。
    pub fn has_all(&self, packages: &[&str]) -> bool {
        packages
            .iter()
            .all(|package| self.dependency(package).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> PackageJsonMinimal {
        serde_json::from_str(content).expect("parse package.json")
    }

    #[test]
    fn parses_minimal_fields_and_merges_dependencies() {
        let pkg = parse(
            r#"{
                "name": "demo",
                "dependencies": { "vue": "^3.4.0" },
                "devDependencies": { "vite": "^5.0.0" },
                "packageManager": "pnpm@9.1.0"
            }"#,
        );
        assert_eq!(pkg.name.as_deref(), Some("demo"));
        assert_eq!(pkg.dependency("vue"), Some("^3.4.0"));
        assert_eq!(pkg.dependency("vite"), Some("^5.0.0"));
        assert_eq!(pkg.package_manager.as_deref(), Some("pnpm@9.1.0"));
        assert!(pkg.has_any(&["vite", "webpack"]));
        assert!(pkg.has_all(&["vue", "vite"]));
        assert!(!pkg.has_all(&["vue", "webpack"]));
        assert_eq!(pkg.merged_dependencies().count(), 2);
    }

    #[test]
    fn missing_fields_default_to_empty() {
        let pkg = parse(r#"{"name": "bare"}"#);
        assert!(pkg.dependencies.is_empty());
        assert!(pkg.dev_dependencies.is_empty());
        assert!(pkg.package_manager.is_none());
    }

    #[test]
    fn read_returns_none_for_missing_or_broken_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(PackageJsonMinimal::read(dir.path()).is_none());
        std::fs::write(dir.path().join("package.json"), "{broken").unwrap();
        assert!(PackageJsonMinimal::read(dir.path()).is_none());
    }
}
