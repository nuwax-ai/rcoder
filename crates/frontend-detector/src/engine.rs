//! 检测引擎：依赖信号匹配 + 清单序优先 + 反向排除（纯函数）。
//!
//! 冲突消解设计说明：Vercel 用显式 `supersedes` 覆盖图，本引擎采用更简单的
//! **清单序优先**单结果模型（元框架排在裸构建器之前，前者命中即返回，后者
//! 不再评估）——语义等价（Vercel 的清单顺序本身就是人工优先级）；`excluded`
//! 反向排除保留（防"未收录元框架依赖 vite"被兜底规则误报）。

use crate::package_json::PackageJsonMinimal;
use crate::rules::{BUILD_RULES, BuildRule, UI_RULES};
use crate::version::{self, ResolvedVersion, VersionSource};

/// 单维度检测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameworkHit {
    /// 稳定标识（nextjs/vite/react/vue3/vue2/vue/other）。
    pub name: &'static str,
    pub display_name: &'static str,
    /// package.json 声明原样（未声明为空串）。
    pub declared_range: String,
    pub version: Option<String>,
    pub source: VersionSource,
}

impl FrameworkHit {
    pub(crate) fn unresolved_other() -> Self {
        Self {
            name: "other",
            display_name: "Other",
            declared_range: String::new(),
            version: None,
            source: VersionSource::None,
        }
    }

    fn from_rule(
        rule_name: &'static str,
        display_name: &'static str,
        resolved: ResolvedVersion,
    ) -> Self {
        Self {
            name: rule_name,
            display_name,
            declared_range: resolved.declared_range,
            version: resolved.version,
            source: resolved.source,
        }
    }
}

/// build 维度：清单序逐条评估，首个通过（some 命中 + every 全过 + excluded
/// 无命中）的规则即结果；全不命中 → other。
pub fn detect_build(dir: &std::path::Path, pkg: &PackageJsonMinimal) -> FrameworkHit {
    for rule in BUILD_RULES {
        if matches_rule(rule, pkg) {
            let package = first_present_package(rule.packages_some, pkg).expect("some 已命中");
            let resolved = version::resolve(dir, package, pkg.dependency(package));
            return FrameworkHit::from_rule(rule.name, rule.display_name, resolved);
        }
    }
    FrameworkHit::unresolved_other()
}

/// ui 维度：清单序单结果；vue 命中时按版本候选序细分 vue3/vue2/vue
///（版本候选序在规则 packages_some 内：vue → vue-router → @vue/cli-service）。
pub fn detect_ui(dir: &std::path::Path, pkg: &PackageJsonMinimal) -> FrameworkHit {
    for rule in UI_RULES {
        if !pkg.has_any(rule.packages_some) {
            continue;
        }
        // 版本取规则内**首个可解析出版本的依赖**（vue 可能本身没声明或声明
        // 非 semver，回退 vue-router / cli-service——nuwax 语义）
        let mut hit: Option<FrameworkHit> = None;
        for package in rule.packages_some {
            let Some(declared) = pkg.dependency(package) else {
                continue;
            };
            let resolved = version::resolve(dir, package, Some(declared));
            if resolved.version.is_some() {
                hit = Some(FrameworkHit::from_rule(
                    rule.name,
                    rule.display_name,
                    resolved,
                ));
                break;
            }
        }
        // 有依赖命中但全部提不出版本（如 workspace:*）
        let mut hit = hit.unwrap_or_else(|| {
            let package = first_present_package(rule.packages_some, pkg).expect("some 已命中");
            let resolved = version::resolve(dir, package, pkg.dependency(package));
            FrameworkHit::from_rule(rule.name, rule.display_name, resolved)
        });
        // vue 细分：**仅 vue 本体**的可解析 major 决定 vue2/vue3。fallback 依赖
        //（vue-router / @vue/cli-service）只提供版本展示、不做细分——它们的
        // major 与 vue 主版本无可靠映射（nuwax 原逻辑拿 router major 判细分会
        // 产出 "vue4" 这类荒谬值，此处修正）。
        if rule.name == "vue"
            && let Some(declared) = pkg.dependency("vue")
            && let Some(major) = version::resolve(dir, "vue", Some(declared))
                .version
                .as_deref()
                .and_then(version::major_of)
        {
            match major {
                2 => {
                    hit.name = "vue2";
                    hit.display_name = "Vue 2";
                }
                3 => {
                    hit.name = "vue3";
                    hit.display_name = "Vue 3";
                }
                _ => {}
            }
        }
        return hit;
    }
    FrameworkHit::unresolved_other()
}

/// 包管理器：packageManager 字段（拆 @ 前 name，Corepack 权威）> lockfile
/// 存在性（清单序）> None（无 package.json 场景由调用方处理为 None）。
pub fn detect_package_manager(dir: &std::path::Path, pkg: &PackageJsonMinimal) -> Option<String> {
    if let Some(field) = &pkg.package_manager {
        let name = field.split('@').next().unwrap_or_default().trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    for (file, pm) in crate::rules::LOCKFILE_TO_PM {
        if dir.join(file).is_file() {
            return Some((*pm).to_string());
        }
    }
    None
}

/// typescript：合并依赖含 typescript ∨ tsconfig.json 存在。
pub fn detect_typescript(dir: &std::path::Path, pkg: &PackageJsonMinimal) -> bool {
    pkg.dependency("typescript").is_some() || dir.join("tsconfig.json").is_file()
}

fn matches_rule(rule: &BuildRule, pkg: &PackageJsonMinimal) -> bool {
    pkg.has_any(rule.packages_some)
        && (rule.require_all.is_empty() || pkg.has_all(rule.require_all))
        && !pkg.has_any(rule.excluded)
}

fn first_present_package<'a>(packages: &[&'a str], pkg: &PackageJsonMinimal) -> Option<&'a str> {
    packages
        .iter()
        .copied()
        .find(|package| pkg.dependency(package).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn pkg_with(deps: &[(&str, &str)]) -> PackageJsonMinimal {
        let mut dependencies = BTreeMap::new();
        for (name, range) in deps {
            dependencies.insert(name.to_string(), range.to_string());
        }
        PackageJsonMinimal {
            dependencies,
            ..Default::default()
        }
    }

    fn detect_build_only(deps: &[(&str, &str)]) -> FrameworkHit {
        let dir = tempfile::tempdir().expect("tempdir");
        detect_build(dir.path(), &pkg_with(deps))
    }

    // ── 规则表逐条 ─────────────────────────────────────────────

    #[test]
    fn each_build_rule_matches_its_primary_dependency() {
        let cases = [
            (&[("next", "14.0.0")][..], "nextjs"),
            (&[("nuxt", "3.9.0")][..], "nuxt"),
            (&[("nuxt3", "3.0.0")][..], "nuxt"),
            (&[("@remix-run/dev", "2.0.0")][..], "remix"),
            (&[("astro", "4.0.0")][..], "astro"),
            (&[("@sveltejs/kit", "2.0.0")][..], "sveltekit"),
            (&[("gatsby", "5.0.0")][..], "gatsby"),
            (&[("@docusaurus/core", "3.0.0")][..], "docusaurus"),
            (&[("@angular/cli", "17.0.0")][..], "angular"),
            (&[("react-scripts", "5.0.0")][..], "create-react-app"),
            (&[("@vue/cli-service", "5.0.0")][..], "vue-cli"),
            (&[("rsbuild", "1.0.0")][..], "rsbuild"),
            (&[("@rspack/cli", "0.5.0")][..], "rsbuild"),
            (&[("vite", "5.0.0")][..], "vite"),
            (&[("webpack", "5.90.0")][..], "webpack"),
        ];
        for (deps, expected) in cases {
            let hit = detect_build_only(deps);
            assert_eq!(hit.name, expected, "deps={deps:?}");
        }
    }

    #[test]
    fn empty_or_unknown_dependencies_fall_to_other() {
        assert_eq!(detect_build_only(&[]).name, "other");
        assert_eq!(detect_build_only(&[("lodash", "4.0.0")]).name, "other");
    }

    // ── 易错点专项 ─────────────────────────────────────────────

    /// supersedes 语义（清单序消解）：SvelteKit 项目带 vite → 报 sveltekit 不报 vite。
    #[test]
    fn meta_framework_supersedes_vite_by_ordering() {
        let hit = detect_build_only(&[("@sveltejs/kit", "2.5.0"), ("vite", "5.1.0")]);
        assert_eq!(hit.name, "sveltekit");
    }

    /// vite 排除表：未收录元框架（如 qwik）依赖 vite 时不误报 vite。
    #[test]
    fn vite_excluded_table_blocks_unlisted_meta_frameworks() {
        let hit = detect_build_only(&[("vite", "5.0.0"), ("@builder.io/qwik", "1.0.0")]);
        assert_eq!(hit.name, "other", "qwik 未收录，vite 被排除表否决");
    }

    /// solid-start 的 every：只有 @solidjs/start 无 solid-js → 不命中。
    #[test]
    fn solid_start_requires_both_dependencies() {
        assert_eq!(
            detect_build_only(&[("@solidjs/start", "1.0.0")]).name,
            "other"
        );
        assert_eq!(
            detect_build_only(&[("@solidjs/start", "1.0.0"), ("solid-js", "1.8.0")]).name,
            "solid-start"
        );
    }

    /// vue-cli 与 ui=vue 正交：@vue/cli-service 项目 ui 也应识别 vue 维度。
    #[test]
    fn vue_cli_build_and_vue_ui_are_orthogonal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pkg = pkg_with(&[("@vue/cli-service", "5.0.8"), ("vue", "^2.7.16")]);
        assert_eq!(detect_build(dir.path(), &pkg).name, "vue-cli");
        let ui = detect_ui(dir.path(), &pkg);
        assert_eq!(ui.name, "vue2", "vue ^2.7.16 → vue2 细分");
    }

    /// next 项目双维度同真：build=nextjs + ui=react。
    #[test]
    fn next_project_hits_both_dimensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pkg = pkg_with(&[("next", "16.2.12"), ("react", "19.2.4")]);
        assert_eq!(detect_build(dir.path(), &pkg).name, "nextjs");
        assert_eq!(detect_ui(dir.path(), &pkg).name, "react");
    }

    // ── ui 维度 ────────────────────────────────────────────────

    #[test]
    fn ui_vue_version_fallback_candidates() {
        // vue 本体非 semver，vue-router 只供版本展示不做细分（router major
        // 与 vue 主版本无可靠映射）
        let hit = detect_ui(
            tempfile::tempdir().expect("tempdir").path(),
            &pkg_with(&[("vue", "workspace:*"), ("vue-router", "^4.6.0")]),
        );
        assert_eq!(hit.name, "vue");
        assert_eq!(
            hit.version.as_deref(),
            Some("4.6.0"),
            "版本取自 router 声明"
        );
        // vue ^2 → vue2
        let hit = detect_ui(
            tempfile::tempdir().expect("tempdir").path(),
            &pkg_with(&[("vue", "^2.7.16")]),
        );
        assert_eq!(hit.name, "vue2");
        // 判不出版本 → vue
        let hit = detect_ui(
            tempfile::tempdir().expect("tempdir").path(),
            &pkg_with(&[("vue", "latest")]),
        );
        assert_eq!(hit.name, "vue");
    }

    #[test]
    fn ui_other_frameworks() {
        assert_eq!(detect_build_only(&[("svelte", "4.0.0")]).name, "other");
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            detect_ui(dir.path(), &pkg_with(&[("svelte", "4.2.0")])).name,
            "svelte"
        );
        assert_eq!(
            detect_ui(dir.path(), &pkg_with(&[("solid-js", "1.8.0")])).name,
            "solid"
        );
        assert_eq!(
            detect_ui(dir.path(), &pkg_with(&[("preact", "10.0.0")])).name,
            "preact"
        );
        assert_eq!(
            detect_ui(dir.path(), &pkg_with(&[("@angular/core", "17.0.0")])).name,
            "angular"
        );
        assert_eq!(detect_ui(dir.path(), &pkg_with(&[])).name, "other");
    }

    // ── 包管理器 / typescript ──────────────────────────────────

    #[test]
    fn package_manager_field_overrides_lockfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        let pkg = PackageJsonMinimal {
            package_manager: Some("pnpm@9.1.0".into()),
            ..Default::default()
        };
        assert_eq!(
            detect_package_manager(dir.path(), &pkg).as_deref(),
            Some("pnpm")
        );
    }

    #[test]
    fn package_manager_from_lockfiles() {
        for (file, expected) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("package-lock.json", "npm"),
            ("bun.lockb", "bun"),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(dir.path().join(file), "").unwrap();
            let pkg = PackageJsonMinimal::default();
            assert_eq!(
                detect_package_manager(dir.path(), &pkg).as_deref(),
                Some(expected),
                "lockfile={file}"
            );
        }
        // 无任何信号 → None
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            detect_package_manager(dir.path(), &PackageJsonMinimal::default()),
            None
        );
    }

    #[test]
    fn typescript_two_signals() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!detect_typescript(
            dir.path(),
            &PackageJsonMinimal::default()
        ));
        assert!(detect_typescript(
            dir.path(),
            &pkg_with(&[("typescript", "^5.2.2")])
        ));
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        assert!(detect_typescript(
            dir.path(),
            &PackageJsonMinimal::default()
        ));
    }
}
