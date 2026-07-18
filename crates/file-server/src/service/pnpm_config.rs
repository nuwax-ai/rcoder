//! pnpm install 配置准备 (对齐 nuwax `ensurePnpmInstallConfig` + `sanitizePnpmBuiltDependenciesConfig`
//! + `createPnpmNpmrc`)。
//!
//! install 前调用 [`ensure_pnpm_install_config`]:
//! 1. 写 `.npmrc` 模板 (package-import-method=copy 等), 已最优则跳过;
//! 2. 清理与 `dangerously-allow-all-builds` 冲突的 pnpm 构建脚本互斥配置
//!    (package.json `pnpm.{never,only,ignored}BuiltDependencies` +
//!    pnpm-workspace.yaml 同名键 + .npmrc kebab-case 键);
//! 3. 向 `.npmrc` 追加 `dangerously-allow-all-builds=true` / `production=false` /
//!    `confirm-modules-purge=false` (缺失才补)。
//!
//! install 本身已带 `--config.dangerouslyAllowAllBuilds=true` 等 CLI 参数, 此处的
//! .npmrc 与 sanitize 是优化 (避免 JuiceFS/FUSE hardlink 失败 + 避免 never/only
//! built 互斥冲突), 故整个步骤对调用方为**尽力而为** (失败仅 warn, 不阻断 install)。

use serde_json::Value;
use std::path::Path;
use tokio::fs;

use crate::error::{AppError, AppResult};

/// package.json / pnpm-workspace.yaml 中与 build-script 互斥的 pnpm 配置键 (camelCase)。
const BUILT_DEPS_PACKAGE_JSON_KEYS: [&str; 3] = [
    "neverBuiltDependencies",
    "onlyBuiltDependencies",
    "ignoredBuiltDependencies",
];

/// `.npmrc` 中同名配置的 kebab-case 键。
const BUILT_DEPS_NPMRC_KEYS: [&str; 3] = [
    "never-built-dependencies",
    "only-built-dependencies",
    "ignored-built-dependencies",
];

// ── ensure 入口 ─────────────────────────────────────────────────────────────────

/// install 前准备 pnpm 配置 (对齐 nuwax ensurePnpmInstallConfig)。
///
/// 注意: 内部各步骤独立 best-effort —— 单步失败记 warn 后继续 (npmrc/sanitize 为优化,
/// 非正确性闸门, install 的 CLI 参数才是)。整函数恒返回 Ok。
pub async fn ensure_pnpm_install_config(project_dir: &Path) {
    if let Err(e) = create_pnpm_npmrc(project_dir).await {
        tracing::warn!(error = %e, dir = %project_dir.display(), "create .npmrc failed (non-blocking)");
    }
    if let Err(e) = sanitize_pnpm_built_dependencies_config(project_dir).await {
        tracing::warn!(error = %e, dir = %project_dir.display(), "sanitize built-deps config failed (non-blocking)");
    }
    if let Err(e) = append_install_lines(project_dir).await {
        tracing::warn!(error = %e, dir = %project_dir.display(), "append .npmrc install lines failed (non-blocking)");
    }
}

// ── createPnpmNpmrc ─────────────────────────────────────────────────────────────

/// 写 `.npmrc` 模板 (对齐 nuwax createPnpmNpmrc): 已最优 (package-import-method=copy
/// 且 store-dir 匹配) 则跳过, 否则整体覆盖为模板内容。
pub(crate) async fn create_pnpm_npmrc(project_dir: &Path) -> AppResult<()> {
    let npmrc_path = project_dir.join(".npmrc");
    let store_dir = std::env::var("npm_config_store_dir")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("PNPM_STORE_DIR")
                .ok()
                .filter(|s| !s.is_empty())
        });
    // 已存在且最优 → 跳过 (对齐 nuwax: method=copy 且 store-dir 匹配)
    if let Ok(existing) = fs::read_to_string(&npmrc_path).await
        && npmrc_optimal(&existing, store_dir.as_deref())
    {
        return Ok(());
    }
    let content = render_npmrc_template(store_dir.as_deref());
    fs::write(&npmrc_path, content)
        .await
        .map_err(|e| AppError::system(format!("write .npmrc {}: {e}", npmrc_path.display())))?;
    Ok(())
}

/// 渲染 .npmrc 模板 (对齐 nuwax; 注释含生成时间 + 文件系统类型)。
fn render_npmrc_template(store_dir: Option<&str>) -> String {
    let fs_type = detect_filesystem_type();
    let store_line = match store_dir {
        Some(s) if !s.is_empty() => format!("store-dir={s}\n"),
        _ => String::new(),
    };
    format!(
        "# pnpm 优化配置\n# 自动生成于 {}\n# 文件系统类型: {}\npackage-import-method=copy\nauto-install-peers=true\nregistry=https://registry.npmmirror.com\n{store_line}",
        cst_datetime_string(),
        fs_type,
    )
}

/// 现有 .npmrc 是否已最优: `package-import-method=copy` 且 (无需 store-dir 或 store-dir 匹配)。
fn npmrc_optimal(existing: &str, want_store_dir: Option<&str>) -> bool {
    let method = first_config_value(existing, "package-import-method");
    let store = first_config_value(existing, "store-dir");
    let method_ok = method.as_deref() == Some("copy");
    let store_ok = match want_store_dir {
        None => true,
        Some(w) => store.as_deref() == Some(w),
    };
    method_ok && store_ok
}

/// 从 .npmrc 文本取某配置键的值 (首个非注释匹配, 对齐 nuwax `^\s*key\s*=\s*(\S+)` + `m` 多行)。
fn first_config_value(npmrc: &str, key: &str) -> Option<String> {
    npmrc.lines().find_map(|line| {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            return None;
        }
        let (candidate, value) = trimmed.split_once('=')?;
        if candidate.trim() != key {
            return None;
        }
        value.split_whitespace().next().map(ToOwned::to_owned)
    })
}

// ── sanitize: 互斥 built-deps 配置清理 ──────────────────────────────────────────

/// 清理 package.json + pnpm-workspace.yaml + .npmrc 中与 build-script 互斥的配置
/// (对齐 nuwax sanitizePnpmBuiltDependenciesConfig)。
async fn sanitize_pnpm_built_dependencies_config(project_dir: &Path) -> AppResult<()> {
    sanitize_package_json_built_deps(project_dir).await?;
    sanitize_pnpm_workspace_built_deps(project_dir).await?;
    sanitize_npmrc_built_deps(&project_dir.join(".npmrc")).await?;
    Ok(())
}

/// 从 package.json 的 `pnpm` 对象移除互斥键 (对齐 nuwax sanitizePackageJsonBuiltDepsConfig)。
/// 非法 JSON / 无 pnpm 对象 → no-op。
async fn sanitize_package_json_built_deps(project_dir: &Path) -> AppResult<()> {
    let pkg_path = project_dir.join("package.json");
    let raw = match fs::read_to_string(&pkg_path).await {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let Ok(mut pkg) = serde_json::from_str::<Value>(&raw) else {
        tracing::warn!(path = %pkg_path.display(), "skip sanitize package.json: invalid JSON");
        return Ok(());
    };
    let Some(Value::Object(obj)) = pkg.get_mut("pnpm") else {
        return Ok(());
    };
    let removed: Vec<&str> = BUILT_DEPS_PACKAGE_JSON_KEYS
        .iter()
        .filter(|k| obj.contains_key(**k))
        .copied()
        .collect();
    if removed.is_empty() {
        return Ok(());
    }
    for k in &removed {
        obj.remove(*k);
    }
    if obj.is_empty()
        && let Some(top) = pkg.as_object_mut()
    {
        top.remove("pnpm");
    }
    let serialized = serde_json::to_string_pretty(&pkg)
        .map_err(|e| AppError::system(format!("serialize package.json: {e}")))?;
    fs::write(&pkg_path, format!("{serialized}\n"))
        .await
        .map_err(|e| AppError::system(format!("write package.json {}: {e}", pkg_path.display())))?;
    tracing::info!(path = %pkg_path.display(), ?removed, "removed conflicting pnpm built-deps from package.json");
    Ok(())
}

/// 从 pnpm-workspace.yaml 行级移除互斥键及其块值 (对齐 nuwax sanitizePnpmWorkspaceBuiltDepsConfig)。
/// 匹配 `^key:\s*(.*)$`; 值为空 / `|` / `>` 时连同其后更深缩进行一并删除。
async fn sanitize_pnpm_workspace_built_deps(project_dir: &Path) -> AppResult<()> {
    let yaml_path = project_dir.join("pnpm-workspace.yaml");
    let content = match fs::read_to_string(&yaml_path).await {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let mut result: Vec<&str> = Vec::new();
    let mut skip_until_indent: i64 = -1;
    let mut removed: Vec<&str> = Vec::new();
    for line in content.split('\n') {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if let Some(key) = match_built_deps_key(trimmed) {
            removed.push(key);
            let inline = trimmed[key.len() + 1..].trim();
            if inline.is_empty() || inline == "|" || inline == ">" {
                skip_until_indent = indent as i64;
            }
            continue;
        }
        if skip_until_indent >= 0 {
            if trimmed.is_empty() || (indent as i64) > skip_until_indent {
                continue;
            }
            skip_until_indent = -1;
        }
        result.push(line);
    }
    if removed.is_empty() {
        return Ok(());
    }
    fs::write(&yaml_path, result.join("\n"))
        .await
        .map_err(|e| {
            AppError::system(format!(
                "write pnpm-workspace.yaml {}: {e}",
                yaml_path.display()
            ))
        })?;
    tracing::info!(path = %yaml_path.display(), ?removed, "removed conflicting pnpm built-deps from pnpm-workspace.yaml");
    Ok(())
}

/// 行首是否匹配某 built-deps 键 (形如 `key:`), 返回该键。
fn match_built_deps_key(trimmed_line: &str) -> Option<&'static str> {
    for key in BUILT_DEPS_PACKAGE_JSON_KEYS {
        let bytes = trimmed_line.as_bytes();
        if bytes.starts_with(key.as_bytes()) && bytes.get(key.len()) == Some(&b':') {
            return Some(key);
        }
    }
    None
}

/// 从 .npmrc 移除 kebab-case built-deps 行 (对齐 nuwax sanitizeNpmrcBuiltDepsConfig)。
/// 返回过滤后内容; 文件不存在 → no-op。
async fn sanitize_npmrc_built_deps(npmrc_path: &Path) -> AppResult<String> {
    let content = match fs::read_to_string(npmrc_path).await {
        Ok(s) => s,
        Err(_) => return Ok(String::new()),
    };
    let filtered: String = content
        .split('\n')
        .filter(|line| !is_built_deps_npmrc_line(line))
        .collect::<Vec<_>>()
        .join("\n");
    if filtered != content {
        fs::write(npmrc_path, &filtered)
            .await
            .map_err(|e| AppError::system(format!("write .npmrc {}: {e}", npmrc_path.display())))?;
    }
    Ok(filtered)
}

// ── 追加 install 必需行 (ensurePnpmInstallConfig 末段) ───────────────────────────

/// 再次 sanitize .npmrc 后, 缺失才追加 dangerously-allow-all-builds / production /
/// confirm-modules-purge (对齐 nuwax ensurePnpmInstallConfig 末段)。
async fn append_install_lines(project_dir: &Path) -> AppResult<()> {
    let npmrc_path = project_dir.join(".npmrc");
    let mut content = sanitize_npmrc_built_deps(&npmrc_path).await?;
    let mut additions: Vec<&str> = Vec::new();
    if !contains_config_key(&content, "dangerously-allow-all-builds") {
        additions.push("dangerously-allow-all-builds=true");
    }
    if !contains_config_key(&content, "production") {
        additions.push("production=false");
    }
    if !contains_config_key(&content, "confirm-modules-purge") {
        additions.push("confirm-modules-purge=false");
    }
    if additions.is_empty() {
        return Ok(());
    }
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&additions.join("\n"));
    content.push('\n');
    fs::write(&npmrc_path, content)
        .await
        .map_err(|e| AppError::system(format!("write .npmrc {}: {e}", npmrc_path.display())))?;
    tracing::info!(dir = %project_dir.display(), ?additions, "updated .npmrc for pnpm install");
    Ok(())
}

/// .npmrc 是否已含某配置键 (对齐 nuwax `/key\s*=/` 测试; 仅看是否存在赋值行)。
fn contains_config_key(npmrc: &str, key: &str) -> bool {
    npmrc.lines().any(|line| {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            return false;
        }
        trimmed
            .split_once('=')
            .is_some_and(|(candidate, _)| candidate.trim() == key)
    })
}

fn is_built_deps_npmrc_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    BUILT_DEPS_NPMRC_KEYS.iter().any(|key| {
        trimmed.strip_prefix(key).is_some_and(|suffix| {
            suffix
                .chars()
                .next()
                .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
        })
    })
}

// ── 辅助: 文件系统类型 / CST 时间 (仅 .npmrc 注释, 非功能性) ────────────────────

/// 检测路径所在文件系统类型 (对齐 nuwax detectFilesystemType): 读 /proc/mounts 取最长
/// 匹配挂载点, fuse.* → "fuse", 否则 "local"; 读失败 → "local"。
fn detect_filesystem_type() -> &'static str {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return "local";
    };
    // 不依赖具体路径 (本函数仅用于注释), 取是否存在 fuse 挂载即可。
    let any_fuse = mounts
        .split('\n')
        .filter_map(|l| l.split_whitespace().nth(2))
        .any(|fs_type| fs_type.starts_with("fuse"));
    if any_fuse { "fuse" } else { "local" }
}

/// 当前东八区时间字符串 `YYYY-MM-DD HH:MM:SS` (对齐 nuwax getCSTDateTimeString)。
fn cst_datetime_string() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(8))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npmrc_optimal_detects_copy_and_store() {
        assert!(npmrc_optimal("package-import-method=copy\n", None));
        assert!(!npmrc_optimal("package-import-method=hardlink\n", None));
        assert!(npmrc_optimal(
            "package-import-method=copy\nstore-dir=/s\n",
            Some("/s")
        ));
        assert!(!npmrc_optimal("package-import-method=copy\n", Some("/s")));
    }

    #[test]
    fn first_config_value_skips_comments() {
        assert_eq!(
            first_config_value(
                "# package-import-method=hardlink\npackage-import-method=copy",
                "package-import-method"
            ),
            Some("copy".to_string())
        );
    }

    #[test]
    fn render_template_has_required_lines() {
        let t = render_npmrc_template(None);
        assert!(t.contains("package-import-method=copy"));
        assert!(t.contains("registry=https://registry.npmmirror.com"));
        assert!(!t.contains("store-dir="));
        let t2 = render_npmrc_template(Some("/store"));
        assert!(t2.contains("store-dir=/store"));
    }

    #[test]
    fn match_built_deps_key_recognizes_camel_keys() {
        assert_eq!(
            match_built_deps_key("onlyBuiltDependencies:"),
            Some("onlyBuiltDependencies")
        );
        assert_eq!(
            match_built_deps_key("neverBuiltDependencies: []"),
            Some("neverBuiltDependencies")
        );
        assert_eq!(
            match_built_deps_key("ignoredBuiltDependencies:"),
            Some("ignoredBuiltDependencies")
        );
        assert_eq!(match_built_deps_key("scripts:"), None);
    }

    #[test]
    fn sanitize_npmrc_filters_kebab_lines() {
        let input = "registry=https://x\nonly-built-dependencies=[\"esbuild\"]\nfoo=bar\n";
        let out = input
            .split('\n')
            .filter(|line| !is_built_deps_npmrc_line(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!out.contains("only-built-dependencies"));
        assert!(out.contains("foo=bar"));
    }

    #[test]
    fn contains_config_key_matches_assignment() {
        assert!(contains_config_key(
            "dangerously-allow-all-builds=true\n",
            "dangerously-allow-all-builds"
        ));
        assert!(!contains_config_key("registry=https://x\n", "production"));
    }
}
