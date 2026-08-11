//! 前端/构建框架检测 (对齐 nuwax `frameworkDetectorUtils`)。
//!
//! 从 `tree` 模块拆出: 框架检测与文件遍历是正交职责, 独立成模块便于维护。

use std::path::Path;

use tokio::fs;

use crate::error::AppResult;

/// 框架检测 (对齐 nuwax frameworkDetectorUtils):
/// - frontend: react 优先; vue/vue-router/@vue/cli-service → 按版本号 vue2/vue3/vue
/// - devFramework: 只看配置文件, nextjs 优先于 vite
pub(super) async fn detect_framework(project_path: &Path) -> AppResult<(String, String)> {
    let frontend = detect_frontend_framework(project_path).await;
    let dev = detect_dev_framework(project_path).await?;
    Ok((frontend, dev))
}

/// frontend 检测 (对齐 nuwax detectFrontendFramework):
/// react/react-dom → "react"; vue 系 → parse_vue_major_version 取首个可解析版本 → "vue2"/"vue3"/"vue"; 否则 "other"。
async fn detect_frontend_framework(project_path: &Path) -> String {
    let pkg_path = project_path.join("package.json");
    let text = match fs::read_to_string(&pkg_path).await {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %pkg_path.display(), %error, "detect frontend framework failed");
            }
            return "other".to_string();
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(path = %pkg_path.display(), %error, "detect frontend framework: invalid package.json");
            return "other".to_string();
        }
    };
    // 合并 dependencies + devDependencies (对齐 nuwax)
    let merged = merge_dep_map(&v);
    // react 优先
    if merged.contains_key("react") || merged.contains_key("react-dom") {
        return "react".to_string();
    }
    // vue 系: 依次对 vue / vue-router / @vue/cli-service 取首个可解析主版本
    if merged.contains_key("vue")
        || merged.contains_key("vue-router")
        || merged.contains_key("@vue/cli-service")
    {
        for key in ["vue", "vue-router", "@vue/cli-service"] {
            if let Some(ver) = merged.get(key)
                && let Some(major) = parse_vue_major_version(ver)
            {
                return format!("vue{major}");
            }
        }
        return "vue".to_string();
    }
    "other".to_string()
}

/// devFramework 检测 (对齐 nuwax detectDevFramework): 只看配置文件, nextjs 优先。
async fn detect_dev_framework(project_path: &Path) -> AppResult<String> {
    for f in [
        "next.config.js",
        "next.config.ts",
        "next.config.mjs",
        "next.config.cjs",
    ] {
        if fs::try_exists(project_path.join(f)).await? {
            return Ok("nextjs".to_string());
        }
    }
    for f in [
        "vite.config.js",
        "vite.config.ts",
        "vite.config.mjs",
        "vite.config.cjs",
    ] {
        if fs::try_exists(project_path.join(f)).await? {
            return Ok("vite".to_string());
        }
    }
    Ok("other".to_string())
}

/// 合并 dependencies + devDependencies 为 {name: version} 映射 (对齐 nuwax 合并范围)。
fn merge_dep_map(pkg: &serde_json::Value) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(obj) = pkg.get(key).and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    map.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    map
}

/// 解析 Vue 依赖的 npm 主版本:
/// - 处理 npm alias: `npm:vue@^3.4.0` → `^3.4.0`
/// - 使用 node-semver 完整解析 npm range，避免把任意首个数字误判成版本
/// - 非标准 (workspace/file/git/url) 返回 None
fn parse_vue_major_version(raw: &str) -> Option<u32> {
    let spec = raw.trim();
    if spec.is_empty() {
        return None;
    }

    let spec = if let Some(alias) = spec.strip_prefix("npm:") {
        // rsplit_once 同时支持 npm:vue@^3 与 npm:@vue/compat@^3。
        alias.rsplit_once('@')?.1
    } else {
        spec
    };
    let normalized = spec.to_ascii_lowercase();
    if ["workspace:", "file:", "git:", "git+", "http:", "https:"]
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
    {
        return None;
    }
    let range = node_semver::Range::parse(spec).ok()?;
    u32::try_from(range.min_version()?.major).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vue_major_version_handles_caret_tilde() {
        assert_eq!(parse_vue_major_version("^3.4.0"), Some(3));
        assert_eq!(parse_vue_major_version("~2.7.16"), Some(2));
        assert_eq!(parse_vue_major_version("3.0.0"), Some(3));
        assert_eq!(parse_vue_major_version("v3.2.0"), Some(3));
    }

    #[test]
    fn parse_vue_major_version_handles_x_and_plain() {
        assert_eq!(parse_vue_major_version("2.x"), Some(2));
        assert_eq!(parse_vue_major_version("3"), Some(3));
    }

    #[test]
    fn parse_vue_major_version_handles_npm_alias() {
        assert_eq!(parse_vue_major_version("npm:vue@^3.4.0"), Some(3));
        assert_eq!(parse_vue_major_version("npm:vue@~2.7.0"), Some(2));
        assert_eq!(parse_vue_major_version("npm:@vue/compat@^3.5.0"), Some(3));
    }

    #[test]
    fn parse_vue_major_version_handles_npm_ranges() {
        assert_eq!(parse_vue_major_version(">=2.7 <3"), Some(2));
        assert_eq!(parse_vue_major_version("2.7 - 3.4"), Some(2));
        assert_eq!(parse_vue_major_version("^3.4 || ^2.7"), Some(2));
    }

    #[test]
    fn parse_vue_major_version_rejects_nonstandard() {
        assert_eq!(parse_vue_major_version("workspace:*"), None);
        assert_eq!(parse_vue_major_version("file:../vue"), None);
        assert_eq!(parse_vue_major_version("git+https://x"), None);
        assert_eq!(parse_vue_major_version("release-3"), None);
        assert_eq!(parse_vue_major_version(""), None);
    }
}
