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

/// 写/维护 `.npmrc`（D1, FS-10）: **增量维护明确平台优化键**
/// (`package-import-method=copy`、env 提供时的 `store-dir`), 已有文件的
/// registry/scope/auth、未识别键、注释与行序原样保留; 文件不存在时才写平台
/// 缺省模板（含 registry=npmmirror, 一次写入后续不覆盖）。
/// TS v1.5.8 原版为整段覆盖（用户私有配置丢失）——数据保护不回退对齐,
/// 行为差异单独归因（见 verification）。
pub(crate) async fn create_pnpm_npmrc(project_dir: &Path) -> AppResult<()> {
    let store_dir = std::env::var("npm_config_store_dir")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("PNPM_STORE_DIR")
                .ok()
                .filter(|s| !s.is_empty())
        });
    create_pnpm_npmrc_with_store(project_dir, store_dir.as_deref()).await
}

async fn create_pnpm_npmrc_with_store(
    project_dir: &Path,
    store_dir: Option<&str>,
) -> AppResult<()> {
    let npmrc_path = project_dir.join(".npmrc");
    match read_optional_text(&npmrc_path).await? {
        None => {
            let content = render_npmrc_template(store_dir);
            fs::write(&npmrc_path, content).await.map_err(|e| {
                AppError::system(format!("write .npmrc {}: {e}", npmrc_path.display()))
            })?;
            Ok(())
        }
        Some(existing) => {
            if npmrc_optimal(&existing, store_dir) {
                return Ok(());
            }
            let updated = upsert_config_line(&existing, "package-import-method", "copy");
            let updated = match store_dir {
                Some(dir) => upsert_config_line(&updated, "store-dir", dir),
                None => updated,
            };
            fs::write(&npmrc_path, updated).await.map_err(|e| {
                AppError::system(format!("write .npmrc {}: {e}", npmrc_path.display()))
            })?;
            Ok(())
        }
    }
}

/// 行级 upsert: 首个非注释 `key=` 行替换为 `key=value`; 无则追加到末尾。
/// 其余行（注释/顺序/未识别键）原样保留。
fn upsert_config_line(content: &str, key: &str, value: &str) -> String {
    let target = format!("{key}={value}");
    let mut replaced = false;
    let lines: Vec<String> = content
        .split('\n')
        .map(|line| {
            let trimmed = line.trim_start();
            if !replaced
                && !trimmed.starts_with('#')
                && trimmed
                    .split_once('=')
                    .is_some_and(|(candidate, _)| candidate.trim() == key)
            {
                replaced = true;
                target.clone()
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        let mut joined = lines.join("\n");
        if !joined.is_empty() && !joined.ends_with('\n') {
            joined.push('\n');
        }
        joined.push_str(&target);
        joined.push('\n');
        joined
    } else {
        lines.join("\n")
    }
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
pub(crate) async fn sanitize_pnpm_built_dependencies_config(project_dir: &Path) -> AppResult<()> {
    sanitize_package_json_built_deps(project_dir).await?;
    sanitize_pnpm_workspace_built_deps(project_dir).await?;
    sanitize_npmrc_built_deps(&project_dir.join(".npmrc")).await?;
    Ok(())
}

/// 从 package.json 的 `pnpm` 对象移除互斥键 (对齐 nuwax sanitizePackageJsonBuiltDepsConfig)。
/// 非法 JSON / 无 pnpm 对象 → no-op。
async fn sanitize_package_json_built_deps(project_dir: &Path) -> AppResult<()> {
    let pkg_path = project_dir.join("package.json");
    let Some(raw) = read_optional_text(&pkg_path).await? else {
        return Ok(());
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

/// 清理 pnpm-workspace.yaml 顶层互斥键（FS-07: 结构层判定 + 解析器验证）:
/// 1. 原文必须能被 YAML 解析——非法源**不写回**（保留原文件, 只记录）;
/// 2. 结构层确认顶层存在目标键才动手（quoted key / flow 值 / anchor 一致覆盖）;
/// 3. 优先采用行级删除结果（保留注释与格式）, 但必须通过解析器语义等价验证,
///    否则回退为结构化重写——indentless sequence、行内注释等行级算法处理不了
///    的形态不再留下孤立列表项。
async fn sanitize_pnpm_workspace_built_deps(project_dir: &Path) -> AppResult<()> {
    let yaml_path = project_dir.join("pnpm-workspace.yaml");
    let Some(content) = read_optional_text(&yaml_path).await? else {
        return Ok(());
    };
    let Ok(original) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
        tracing::warn!(
            path = %yaml_path.display(),
            "pnpm-workspace.yaml is not valid YAML; skip sanitize (source untouched)"
        );
        return Ok(());
    };
    // 结构层: 顶层 mapping 中移除目标键, 得到期望语义与"是否存在目标键"判定。
    let Some(expected) = mapping_without_built_deps(&original) else {
        return Ok(()); // 顶层无目标键（含非 mapping 文档）→ no-op
    };
    // 行级删除优先（保注释）; 结果必须解析成功且与期望语义等价才采用。
    let adopted = {
        let line_removed = remove_built_deps_lines(&content);
        match serde_yaml::from_str::<serde_yaml::Value>(&line_removed) {
            Ok(parsed) if parsed == expected => line_removed,
            _ => serde_yaml::to_string(&expected)
                .map_err(|e| AppError::system(format!("serialize pnpm-workspace.yaml: {e}")))?,
        }
    };
    fs::write(&yaml_path, adopted).await.map_err(|e| {
        AppError::system(format!(
            "write pnpm-workspace.yaml {}: {e}",
            yaml_path.display()
        ))
    })?;
    tracing::info!(path = %yaml_path.display(), "removed conflicting pnpm built-deps from pnpm-workspace.yaml");
    Ok(())
}

/// 顶层 mapping 拷贝并移除目标键; 顶层无目标键或文档非 mapping → None。
fn mapping_without_built_deps(original: &serde_yaml::Value) -> Option<serde_yaml::Value> {
    let mapping = original.as_mapping()?;
    let targets: Vec<serde_yaml::Value> = BUILT_DEPS_PACKAGE_JSON_KEYS
        .iter()
        .map(|key| serde_yaml::Value::String((*key).to_string()))
        .collect();
    if !targets.iter().any(|key| mapping.contains_key(key)) {
        return None;
    }
    let mut stripped = mapping.clone();
    for key in targets {
        stripped.remove(&key);
    }
    Some(serde_yaml::Value::Mapping(stripped))
}

/// 行级删除顶层互斥键及其块值（保注释的尽力路径; 正确性由上层解析器验证兜底）。
fn remove_built_deps_lines(content: &str) -> String {
    let mut result: Vec<&str> = Vec::new();
    let mut skip_until_indent: Option<usize> = None;
    for line in content.split('\n') {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent == 0
            && let Some(key) = match_built_deps_key(trimmed)
        {
            let inline = trimmed[key.len() + 1..].trim();
            if inline.is_empty() || inline == "|" || inline == ">" {
                skip_until_indent = Some(indent);
            }
            continue;
        }
        if let Some(parent_indent) = skip_until_indent {
            if trimmed.is_empty() || indent > parent_indent {
                continue;
            }
            skip_until_indent = None;
        }
        result.push(line);
    }
    result.join("\n")
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
    let Some(content) = read_optional_text(npmrc_path).await? else {
        return Ok(String::new());
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

/// 读取可选配置文件：不存在是正常分支，其他 I/O 错误交给上层记录或处理。
async fn read_optional_text(path: &Path) -> AppResult<Option<String>> {
    match fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::system(format!(
            "read config file {}: {error}",
            path.display()
        ))),
    }
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

    /// FS-10 反例 (D1): 已有 .npmrc 的 registry/scope/auth 及未识别键必须原样
    /// 保留, 只增量维护平台优化键。修复前整段模板覆盖把私有配置全部丢失
    /// （TS v1.5.8 原版同病, 数据保护不回退对齐——差异单独归因）。
    #[tokio::test]
    async fn existing_user_npmrc_settings_survive_optimization() {
        let dir = tempfile::tempdir().expect("fixture");
        let npmrc = dir.path().join(".npmrc");
        let original = "# my company registry\n\
             registry=https://npm.company.internal/\n\
             @corp:registry=https://npm.company.internal/\n\
             //npm.company.internal/:_authToken=secret-token\n\
             fund=false\n\
             package-import-method=hardlink\n";
        fs::write(&npmrc, original).await.expect("write user npmrc");

        create_pnpm_npmrc_with_store(dir.path(), None)
            .await
            .expect("optimize npmrc");

        let updated = fs::read_to_string(&npmrc).await.expect("read back");
        assert!(
            updated.contains("registry=https://npm.company.internal/"),
            "user registry must survive: {updated}"
        );
        assert!(
            updated.contains("@corp:registry=https://npm.company.internal/"),
            "scoped registry must survive: {updated}"
        );
        assert!(
            updated.contains("//npm.company.internal/:_authToken=secret-token"),
            "auth token must survive: {updated}"
        );
        assert!(
            updated.contains("fund=false"),
            "unmanaged keys must survive"
        );
        assert!(
            updated.contains("# my company registry"),
            "user comments must survive"
        );
        assert!(
            updated.contains("package-import-method=copy"),
            "platform optimization key must be enforced"
        );
        assert!(
            !updated.contains("registry=https://registry.npmmirror.com"),
            "platform default registry must not override a user value"
        );
    }

    /// FS-10: store-dir 只更新自身行; 用户其余内容不动。
    #[tokio::test]
    async fn store_dir_from_env_updates_single_line_only() {
        let dir = tempfile::tempdir().expect("fixture");
        let npmrc = dir.path().join(".npmrc");
        fs::write(
            &npmrc,
            "registry=https://npm.company.internal/\npackage-import-method=copy\nstore-dir=/old-store\n",
        )
        .await
        .expect("write");

        create_pnpm_npmrc_with_store(dir.path(), Some("/new-store"))
            .await
            .expect("optimize");

        let updated = fs::read_to_string(&npmrc).await.expect("read back");
        assert!(updated.contains("store-dir=/new-store"));
        assert!(!updated.contains("/old-store"));
        assert!(
            updated.contains("registry=https://npm.company.internal/"),
            "registry untouched: {updated}"
        );
        assert_eq!(
            updated.lines().count(),
            3,
            "no extra lines beyond the single-key update: {updated}"
        );
    }

    #[test]
    fn package_json_round_trip_preserves_insertion_order() {
        // serde_json preserve_order 特性的行为锁：sanitize_pnpm_built_deps 对
        // package.json 是读-改-写回（from_str → remove 键 → to_string_pretty），
        // 键序必须保持插入序——feature 丢失时 serde_json 静默退化为 BTreeMap
        // 字母序，用户 version control 里的 package.json 会产生全量无关 diff。
        // 刻意用非字母序键序：无此特性时本测试立即失败。
        let raw = r#"{
  "zzz": 1,
  "name": "app",
  "aaa": { "y": 1, "x": 2 },
  "version": "0.1.0"
}"#;
        let parsed: Value = serde_json::from_str(raw).expect("合法 JSON");
        let keys: Vec<&String> = parsed.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            ["zzz", "name", "aaa", "version"],
            "解析必须保持插入序（preserve_order）"
        );

        let round_tripped: Value =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        let keys_after: Vec<&String> = round_tripped.as_object().unwrap().keys().collect();
        assert_eq!(keys_after, keys, "序列化 round-trip 后键序必须不变");

        let nested: Vec<&String> = round_tripped["aaa"].as_object().unwrap().keys().collect();
        assert_eq!(nested, ["y", "x"], "嵌套对象同样保序");
    }

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

    /// FS-07 反例: indentless sequence 与行内注释形态的顶层键, 行级删除后
    /// 必须仍是 pnpm 可解析的合法 YAML。修复前留下孤立列表项。
    #[tokio::test]
    async fn workspace_sanitize_handles_indentless_and_inline_comment() {
        for (label, content) in [
            (
                "indentless sequence",
                "packages:\n  - apps/*\nonlyBuiltDependencies:\n- esbuild\n",
            ),
            (
                "inline comment",
                "packages:\n  - apps/*\nonlyBuiltDependencies: # keep list\n  - esbuild\n",
            ),
        ] {
            let dir = tempfile::tempdir().expect("fixture");
            let path = dir.path().join("pnpm-workspace.yaml");
            fs::write(&path, content).await.expect("write");

            sanitize_pnpm_workspace_built_deps(dir.path())
                .await
                .expect("sanitize");

            let output = fs::read_to_string(&path).await.expect("read");
            let parsed: serde_yaml::Value = serde_yaml::from_str(&output).unwrap_or_else(|error| {
                panic!("{label}: result must stay valid YAML, got {error}\n{output}")
            });
            let keys = parsed.as_mapping().expect("top-level mapping");
            assert!(
                !keys.contains_key(serde_yaml::Value::String("onlyBuiltDependencies".into())),
                "{label}: target key must be gone:\n{output}"
            );
            assert!(
                !output.contains("- esbuild"),
                "{label}: list items must not be orphaned:\n{output}"
            );
            assert!(
                keys.contains_key(serde_yaml::Value::String("packages".into())),
                "{label}: unrelated keys must survive:\n{output}"
            );
        }
    }

    /// FS-07: quoted key 与 flow 值形态的目标键也要被删除（结构层判定）。
    #[tokio::test]
    async fn workspace_sanitize_handles_quoted_and_flow_keys() {
        for (label, content) in [
            (
                "quoted key",
                "packages:\n  - apps/*\n\"onlyBuiltDependencies\":\n  - esbuild\n",
            ),
            (
                "flow value",
                "packages:\n  - apps/*\nneverBuiltDependencies: [sharp, esbuild]\n",
            ),
        ] {
            let dir = tempfile::tempdir().expect("fixture");
            let path = dir.path().join("pnpm-workspace.yaml");
            fs::write(&path, content).await.expect("write");

            sanitize_pnpm_workspace_built_deps(dir.path())
                .await
                .expect("sanitize");

            let output = fs::read_to_string(&path).await.expect("read");
            let parsed: serde_yaml::Value = serde_yaml::from_str(&output)
                .unwrap_or_else(|error| panic!("{label}: valid YAML, got {error}"));
            let keys = parsed.as_mapping().expect("mapping");
            for key in BUILT_DEPS_PACKAGE_JSON_KEYS {
                assert!(
                    !keys.contains_key(serde_yaml::Value::String(key.to_string())),
                    "{label}: {key} must be gone:\n{output}"
                );
            }
            assert!(output.contains("packages"), "{label}: unrelated intact");
        }
    }

    /// FS-07: 标准缩进块删除保留注释（行级路径优先）; 非法源不写回。
    #[tokio::test]
    async fn workspace_sanitize_preserves_comments_and_never_rewrites_invalid_source() {
        let dir = tempfile::tempdir().expect("fixture");
        let path = dir.path().join("pnpm-workspace.yaml");
        fs::write(
            &path,
            "# workspace comment\npackages:\n  - apps/*\n# keep this\nignoredBuiltDependencies:\n  - sharp\n",
        )
        .await
        .expect("write");
        sanitize_pnpm_workspace_built_deps(dir.path())
            .await
            .expect("sanitize");
        let output = fs::read_to_string(&path).await.expect("read");
        assert!(output.contains("# workspace comment"), "{output}");
        assert!(output.contains("# keep this"), "{output}");
        assert!(!output.contains("ignoredBuiltDependencies"), "{output}");

        let invalid = tempfile::tempdir().expect("fixture");
        let invalid_path = invalid.path().join("pnpm-workspace.yaml");
        let broken = "packages: [unclosed\n  - : :\nnot: valid: yaml:\n";
        fs::write(&invalid_path, broken)
            .await
            .expect("write broken");
        sanitize_pnpm_workspace_built_deps(invalid.path())
            .await
            .expect("sanitize skips invalid source");
        assert_eq!(
            fs::read_to_string(&invalid_path).await.expect("untouched"),
            broken,
            "invalid source must not be rewritten"
        );
    }

    #[tokio::test]
    async fn workspace_sanitize_only_removes_top_level_keys() {
        let dir = tempfile::tempdir().expect("create test directory");
        let path = dir.path().join("pnpm-workspace.yaml");
        fs::write(
            &path,
            "packages:\n  - apps/*\nmetadata:\n  onlyBuiltDependencies:\n    - keep-me\nonlyBuiltDependencies:\n  - esbuild\nignoredBuiltDependencies: [sharp]\n",
        )
        .await
        .expect("write test workspace file");

        sanitize_pnpm_workspace_built_deps(dir.path())
            .await
            .expect("sanitize workspace file");

        let output = fs::read_to_string(path)
            .await
            .expect("read sanitized workspace file");
        assert!(output.contains("  onlyBuiltDependencies:\n    - keep-me"));
        assert!(!output.contains("\nonlyBuiltDependencies:"));
        assert!(!output.contains("ignoredBuiltDependencies:"));
    }
}
