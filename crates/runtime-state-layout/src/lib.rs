//! 运行态状态根解析契约（R09：app-cli 与平台消费者同一解析规则）。
//!
//! 问题（review R09）：默认锁根按 `workspace.parent` 推导 + 缺省
//! `unknown-app` 段——兄弟项目共用一个状态根互相阻塞；同项目 source 与
//! `.run` 的 parent 不同又分裂锁域；平台凭据查找靠父目录/当前目录各猜一次。
//!
//! 契约（显式优先，缺省按项目隔离）：
//! 1. `APP_CLI_STATE_ROOT` env —— 平台（rcoder）注入的显式按应用根；
//!    source 根、`.run` 别名、任何入口都指向同一目录（唯一锁域，B04）。
//! 2. `PROJECT_ID` env 非空 —— 容器/托管形态按应用段隔离：
//!    `{卷根}/.app-cli-state/{project_id}`。
//! 3. 均缺省（standalone/桌面）—— 本地项目登记表
//!    `{卷根}/.app-cli-state/registry.json`：规范化项目根 → 稳定生成的
//!    项目段（首次登记经跨进程互斥写入）。兄弟项目各自独立段；
//!    同一项目经 symlink/junction 规范化后同段。
//!
//! 项目根别名规则：workspace 基名恰为 `.run`（产物态运行目录）时，项目根
//! 取其父目录（源码根）——source 与产物态共用锁域、token、journal。
//! 这不是 parent 盲猜：`.run` 是部署协议的固定目录名，语义明确。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// 状态目录名（与 app-cli `STATE_DIR_NAME` 同值，单一契约）。
pub const STATE_DIR_NAME: &str = ".app-cli-state";

/// 规范化项目根：canonicalize（解析 symlink/junction；不存在时用字面路径）
/// + `.run` 别名折叠。
pub fn canonical_project_root(workspace: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    if canonical.file_name().is_some_and(|name| name == ".run")
        && let Some(parent) = canonical.parent()
    {
        return parent.to_path_buf();
    }
    canonical
}

/// 卷根（状态根的父目录）：项目根的父目录。
fn volume_root(project_root: &Path) -> Result<&Path> {
    project_root
        .parent()
        .context("project root has no volume parent for runtime state")
}

/// 解析状态根（读侧——不创建登记；standalone 未登记时返回 None 由
/// [`ensure_state_root`] 补登记）。
pub fn resolve_state_root(
    workspace: &Path,
    explicit_root: Option<&std::ffi::OsStr>,
    project_id_env: Option<&std::ffi::OsStr>,
) -> Result<Option<PathBuf>> {
    if let Some(explicit) = explicit_root.filter(|value| !value.is_empty()) {
        return Ok(Some(PathBuf::from(explicit)));
    }
    let project_root = canonical_project_root(workspace);
    let volume = volume_root(&project_root)?;
    let state_base = volume.join(STATE_DIR_NAME);
    if let Some(app_id) = project_id_env.filter(|value| !value.is_empty()) {
        return Ok(Some(state_base.join(app_id)));
    }
    // standalone：读登记表（存在即用；损坏 fail-fast——不能静默换段分裂锁域）
    let registry = state_base.join("registry.json");
    if !registry.exists() {
        return Ok(None);
    }
    let map = read_registry(&registry)?;
    let key = project_root.to_string_lossy().into_owned();
    Ok(map.get(&key).map(|segment| state_base.join(segment)))
}

/// 解析并**确保**状态根（写侧——首次登记经跨进程互斥创建）。
pub fn ensure_state_root(
    workspace: &Path,
    explicit_root: Option<&std::ffi::OsStr>,
    project_id_env: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(root) = resolve_state_root(workspace, explicit_root, project_id_env)? {
        return Ok(root);
    }
    let project_root = canonical_project_root(workspace);
    let state_base = volume_root(&project_root)?.join(STATE_DIR_NAME);
    std::fs::create_dir_all(&state_base).context("create runtime state base dir")?;
    // 跨进程互斥登记（锁文件随进程退出释放；std 文件锁语义与 OwnerGuard 同源）
    let lock_path = state_base.join("registry.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .context("open registry lock")?;
    lock_file
        .try_lock()
        .context("lock runtime state registry for first registration")?;
    // 持锁重读（并发首登都走同一段临界区）
    let registry = state_base.join("registry.json");
    let mut map = if registry.exists() {
        read_registry(&registry)?
    } else {
        Default::default()
    };
    let key = project_root.to_string_lossy().into_owned();
    if let Some(segment) = map.get(&key) {
        return Ok(state_base.join(segment));
    }
    let segment = format!("p-{}", uuid_v7_like());
    write_json_atomic(
        &registry,
        map_insert(&mut map, project_root, segment.clone()),
    )?;
    Ok(state_base.join(segment))
}

fn map_insert(
    map: &mut std::collections::BTreeMap<String, String>,
    root: PathBuf,
    segment: String,
) -> &std::collections::BTreeMap<String, String> {
    map.insert(root.to_string_lossy().into_owned(), segment);
    map
}

fn read_registry(path: &Path) -> Result<std::collections::BTreeMap<String, String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read runtime state registry {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("decode runtime state registry {} (corrupt file must be resolved by an operator, not silently re-keyed)", path.display()))
}

fn write_json_atomic(path: &Path, map: &std::collections::BTreeMap<String, String>) -> Result<()> {
    let json = serde_json::to_string_pretty(map).context("encode registry")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)
        .and_then(|_| std::fs::rename(&tmp, path))
        .with_context(|| format!("write runtime state registry {}", path.display()))
}

/// 无 uuid 依赖的稳定新段（时间有序：毫秒 + 进程 id + 计数器）。
fn uuid_v7_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{millis:x}-{seq:x}-{}", std::process::id())
}

/// 校验登记表条目与实际项目根一致（诊断用）。
pub fn registered_roots(workspace: &Path) -> Result<Vec<PathBuf>> {
    let project_root = canonical_project_root(workspace);
    let Some(volume) = project_root.parent() else {
        bail!("project root has no volume parent");
    };
    let registry = volume.join(STATE_DIR_NAME).join("registry.json");
    if !registry.exists() {
        return Ok(Vec::new());
    }
    Ok(read_registry(&registry)?
        .keys()
        .filter_map(|key| PathBuf::from(key).canonicalize().ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R09 反例：兄弟项目（无 PROJECT_ID）各自独立状态段——不再共用
    /// unknown-app 互相阻塞。
    #[test]
    fn sibling_projects_get_distinct_state_roots() {
        let volume = tempfile::tempdir().unwrap();
        let ws_a = volume.path().join("project-a");
        let ws_b = volume.path().join("project-b");
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();
        let root_a = ensure_state_root(&ws_a, None, None).unwrap();
        let root_b = ensure_state_root(&ws_b, None, None).unwrap();
        assert_ne!(root_a, root_b, "siblings must not share a state root");
        // 幂等：同项目重复解析同根
        assert_eq!(root_a, ensure_state_root(&ws_a, None, None).unwrap());
    }

    /// R09 反例：同项目 source 与 .run 共锁域（别名折叠到源码根）。
    #[test]
    fn source_and_run_dir_share_state_root() {
        let volume = tempfile::tempdir().unwrap();
        let ws = volume.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(ws.join(".run")).unwrap();
        let source_root = ensure_state_root(&ws, None, None).unwrap();
        let run_root = ensure_state_root(&ws.join(".run"), None, None).unwrap();
        assert_eq!(
            source_root, run_root,
            "artifact .run must share the source project lock domain"
        );
    }

    /// symlink 别名同段（canonicalize 折叠）。
    #[cfg(unix)]
    #[test]
    fn symlinked_workspace_shares_state_root() {
        let volume = tempfile::tempdir().unwrap();
        let ws = volume.path().join("real");
        std::fs::create_dir_all(&ws).unwrap();
        let link = volume.path().join("alias");
        std::os::unix::fs::symlink(&ws, &link).unwrap();
        assert_eq!(
            ensure_state_root(&ws, None, None).unwrap(),
            ensure_state_root(&link, None, None).unwrap()
        );
    }

    /// 显式 env 优先（managed 注入唯一权威）；PROJECT_ID 段隔离保持容器行为。
    #[test]
    fn explicit_and_project_id_precedence() {
        let volume = tempfile::tempdir().unwrap();
        let ws = volume.path().join("c");
        std::fs::create_dir_all(&ws).unwrap();
        let explicit = std::ffi::OsString::from("/explicit/root");
        assert_eq!(
            ensure_state_root(&ws, Some(&explicit), None).unwrap(),
            PathBuf::from("/explicit/root")
        );
        let project_id = std::ffi::OsString::from("app-42");
        // macOS /var → /private/var：canonicalize 后比较
        assert_eq!(
            ensure_state_root(&ws, None, Some(&project_id)).unwrap(),
            std::fs::canonicalize(volume.path())
                .unwrap()
                .join(STATE_DIR_NAME)
                .join("app-42")
        );
    }

    /// 登记表损坏 → fail-fast（不静默换段分裂既有锁域）。
    #[test]
    fn corrupt_registry_fails_closed() {
        let volume = tempfile::tempdir().unwrap();
        let ws = volume.path().join("p");
        std::fs::create_dir_all(&ws).unwrap();
        let base = volume.path().join(STATE_DIR_NAME);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("registry.json"), "not json").unwrap();
        let error = ensure_state_root(&ws, None, None).unwrap_err();
        assert!(
            error.to_string().contains("registry"),
            "diagnostic: {error:#}"
        );
    }
}
