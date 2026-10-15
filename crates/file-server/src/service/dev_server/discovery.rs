//! 只读目标发现（DEV-1 §3.1 / 复核 DEV-R3）：定位当前项目可能的状态根与
//! 监督目标。
//!
//! 事故锚点（nuwax-k8s-test app 211）：spawn 的环境过滤使子编排器走
//! standalone registry 段（`p-…`），而停止路径只按平台环境根解析——
//! 停到了旧实例残留记录上，活编排器毫发无损。本模块把"项目当前有哪些
//! 状态根、哪些还活着"变成一个无副作用的查询：不创建状态根、不启动
//! 管理进程、不提交任何控制请求。停止/启动/复用共用同一份发现结果。
//!
//! DEV-R3：读取失败不再经 `.ok()` 静默变成"无候选"——根存在但
//! supervisor.json 不可读按**观察失败**上报（调用方不得把它当作
//! "确实没有目标"）；binding 校验补齐 component（仅 `app-cli` 的
//! userapp/agent 编排目标，其余组件的记录不属于本域）。

use std::path::{Path, PathBuf};

use runtime_state_layout::resolve_state_root;
use runtime_supervisor::{Binding, Snapshot, last_snapshot};

/// dev 编排目标的 binding component（app-cli run/serve 均用该值）。
pub(crate) const ORCHESTRATOR_COMPONENT: &str = "app-cli";

/// 发现的一个候选监督目标（supervisor.json 可读且 binding 完整属于本项目）。
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

/// 只读发现的结果分类：已核验目标与观察失败分开，不存在第三种
/// "假装没有目标"的折叠（DEV-R3）。
#[derive(Debug, Default)]
pub(crate) struct DiscoveryReport {
    /// binding 完整匹配（`app-cli` + 本项目 origin）的目标。
    pub targets: Vec<DiscoveredTarget>,
    /// 根存在但记录不可读（I/O/解码失败）——观察失败，不是"无目标"。
    pub unreadable: Vec<(PathBuf, String)>,
}

impl DiscoveryReport {
    /// 非终止态目标（可能仍在执行）。
    pub fn live_targets(&self) -> impl Iterator<Item = &DiscoveredTarget> {
        self.targets
            .iter()
            .filter(|target| target.snapshot.phase != runtime_supervisor::Phase::Stopped)
    }

    /// 是否存在任何本地痕迹（目标或观察失败）——用于判断"确实无执行"
    /// 与"观察受阻"。
    pub fn has_local_presence(&self) -> bool {
        !self.targets.is_empty() || !self.unreadable.is_empty()
    }
}

/// 本项目状态根候选（去重）：
/// 1. 平台/显式根——与 file-server 自身环境同一份解析（per-app builder
///    容器里即注入的 `APP_CLI_STATE_ROOT`）；
/// 2. standalone registry 根——**不带平台覆盖**的只读解析（读 registry.json
///    映射，不改动其 first-wins 语义；无映射时无候选）；
/// 3. 调用方捕获的额外根（本次 launch 的实际根、持久停止记录的目标根）——
///    实际根已在内存/持久层捕获时不得因重推导缺项而漏停（DEV-R3）。
///
/// 父进程显式根只有在平台身份与本项目一致时才纳入（多项目代理进程不能
/// 把所有项目绑到同一个父根）；无平台环境（桌面 standalone）时候选 1
/// 自然缺席，候选 2 即其本根。
pub(crate) fn project_state_root_candidates(
    workspace: &Path,
    extra_roots: &[PathBuf],
) -> Vec<PathBuf> {
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
    if let Some(root) = explicit_for_project
        && !roots.contains(&root)
    {
        roots.push(root);
    }
    // 只读 registry 查询：显式不带平台覆盖值，保持 resolver 权威语义不变。
    if let Ok(Some(registry_root)) = resolve_state_root(workspace, None, None)
        && !roots.contains(&registry_root)
    {
        roots.push(registry_root);
    }
    for root in extra_roots {
        if !roots.contains(root) {
            roots.push(root.clone());
        }
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

/// 汇总（无额外根）：本项目的全部候选目标。
pub(crate) fn discover_targets(workspace: &Path) -> DiscoveryReport {
    discover_targets_with(workspace, &[])
}

/// 汇总（带捕获根）：逐候选读取监督记录；binding 不完整属于本项目的根
/// 跳过（不是错误——其他项目/组件的记录不该挡住本项目停止），根存在但
/// 不可读记为观察失败。
pub(crate) fn discover_targets_with(workspace: &Path, extra_roots: &[PathBuf]) -> DiscoveryReport {
    let mut report = DiscoveryReport::default();
    let origin = match runtime_state_layout::resolve_project_origin(workspace) {
        Ok(origin) => origin,
        Err(error) => {
            report.unreadable.push((
                workspace.to_path_buf(),
                format!("resolve project origin: {error:#}"),
            ));
            return report;
        }
    };
    for root in project_state_root_candidates(workspace, extra_roots) {
        if !root.join("supervisor.json").try_exists().unwrap_or(false) {
            continue;
        }
        match last_snapshot(&root) {
            Ok(snapshot) => {
                if snapshot.binding.component == ORCHESTRATOR_COMPONENT
                    && snapshot.binding.resource == origin
                {
                    report.targets.push(DiscoveredTarget {
                        state_root: root,
                        snapshot,
                    });
                }
                // 其他项目/其他组件的记录：与本域无关，静默跳过。
            }
            Err(error) => report
                .unreadable
                .push((root, format!("read supervisor record: {error:#}"))),
        }
    }
    report
}
