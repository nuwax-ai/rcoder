//! 只读目标发现（DEV-1 §3.1）：定位当前项目可能的状态根与监督目标。
//!
//! 事故锚点（nuwax-k8s-test app 211）：spawn 的环境过滤使子编排器走
//! standalone registry 段（`p-…`），而停止路径只按平台环境根解析——
//! 停到了旧实例残留记录上，活编排器毫发无损。本模块把"项目当前有哪些
//! 状态根、哪些还活着"变成一个无副作用的查询：不创建状态根、不启动
//! 管理进程、不提交任何控制请求。停止/启动/复用共用同一份发现结果。

use std::path::{Path, PathBuf};

use runtime_state_layout::resolve_state_root;
use runtime_supervisor::{Binding, Snapshot, last_snapshot};

/// 发现的一个候选监督目标（supervisor.json 可读且 binding 属于本项目）。
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredTarget {
    pub state_root: PathBuf,
    pub snapshot: Snapshot,
}

impl DiscoveredTarget {
    pub fn binding(&self) -> &Binding {
        &self.snapshot.binding
    }
}

/// 本项目状态根候选（去重、存在优先级排序）：
/// 1. 平台/显式根——与 file-server 自身环境同一份解析（per-app builder
///    容器里即注入的 `APP_CLI_STATE_ROOT`）；
/// 2. standalone registry 根——**不带平台覆盖**的只读解析（读 registry.json
///    映射，不改动其 first-wins 语义；无映射时返回 None）。
///
/// 父进程显式根只有在平台身份与本项目一致时才纳入（多项目代理进程不能
/// 把所有项目绑到同一个父根）；无平台环境（桌面 standalone）时候选 1
/// 自然缺席，候选 2 即其本根。
pub(crate) fn project_state_root_candidates(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let explicit_for_project = std::env::var_os("PROJECT_ID")
        .filter(|value| !value.is_empty())
        .filter(|project| platform_identity_matches(workspace, project))
        .and_then(|_| {
            resolve_state_root(
                workspace,
                std::env::var_os("APP_CLI_STATE_ROOT").as_deref(),
                std::env::var_os("PROJECT_ID").as_deref(),
            )
            .ok()
            .flatten()
        });
    if let Some(root) = explicit_for_project {
        roots.push(root);
    }
    // 只读 registry 查询：显式不带平台覆盖值，保持 resolver 权威语义不变。
    if let Ok(Some(registry_root)) = resolve_state_root(workspace, None, None)
        && !roots.contains(&registry_root)
    {
        roots.push(registry_root);
    }
    roots
}

/// 平台身份是否指向本项目：项目 origin 的 basename 与 PROJECT_ID 一致
/// （per-app 容器布局 `{volume}/{app_id}` 的约定）。
fn platform_identity_matches(workspace: &Path, project: &std::ffi::OsStr) -> bool {
    runtime_state_layout::resolve_project_origin(workspace)
        .ok()
        .and_then(|origin| origin.file_name().map(|name| name == project))
        .unwrap_or(false)
}

/// 旧运行（spawn 无显式根）的迁移回执共享目录：`{workspace 父}/migration-receipts`。
/// 仅作为物理位置连续性锚点返回；调用方不得复制/归属其内容到其他应用。
pub(crate) fn legacy_migration_receipts_dir(workspace: &Path) -> Option<String> {
    let dir = workspace.parent()?.join("migration-receipts");
    dir.is_dir().then(|| dir.display().to_string())
}

/// 读取某状态根下的监督目标；binding 不属于本项目的根返回 None
/// （不是错误——其他项目/残留根不该挡住本项目停止）。
pub(crate) fn read_target(root: &Path, workspace: &Path) -> Option<DiscoveredTarget> {
    let snapshot = last_snapshot(root).ok()?;
    let origin = runtime_state_layout::resolve_project_origin(workspace).ok()?;
    if snapshot.binding.resource != origin {
        return None;
    }
    Some(DiscoveredTarget {
        state_root: root.to_path_buf(),
        snapshot,
    })
}

/// 汇总：本项目的全部候选目标（含已 Stopped 的历史根——调用方按
/// phase/锁状态分类 cleaned_stale / 待停止）。
pub(crate) fn discover_targets(workspace: &Path) -> Vec<DiscoveredTarget> {
    project_state_root_candidates(workspace)
        .iter()
        .filter_map(|root| read_target(root, workspace))
        .collect()
}
