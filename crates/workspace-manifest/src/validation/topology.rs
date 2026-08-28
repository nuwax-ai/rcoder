//! workspace 拓扑校验（依赖环检测 + 拓扑序 + 日志配置收集）。

use std::collections::{BTreeMap, BTreeSet};

use crate::{DiscoveredProject, LogFormat, LogSource, ManifestError};

use super::issue::{ValidationIssue, manifest_file_of};
use super::project::{is_dns1123_label, relative_path_issue};

/// 跨服务拓扑收集式校验：service_id 唯一性 / proxy 路由唯一性 / 依赖闭合与顺序。
///
/// 每条问题定位到声明方的 `<dir>/project.manifest.toml`；路由冲突类同时列出
/// 冲突双方，agent 可直接按提示改写。
pub fn collect_topology_issues(projects: &[DiscoveredProject]) -> Vec<ValidationIssue> {
    let enabled: Vec<_> = projects
        .iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();
    if enabled.is_empty() {
        return vec![ValidationIssue::new(
            "workspace has no enabled services (all projects disabled or none discovered)",
        )
        .at_file("workspace.manifest.toml")
        .with_hint(
            "check that each module dir has project.manifest.toml with [project] enabled = true (default)",
        )];
    }
    let mut issues = Vec::new();
    let mut ids: BTreeMap<String, &DiscoveredProject> = BTreeMap::new();
    let mut routes: BTreeMap<String, &DiscoveredProject> = BTreeMap::new();
    let mut catch_all_reported = false;
    for project in &enabled {
        let id = project.service_id();
        if let Some(previous) = ids.get(id) {
            issues.push(
                ValidationIssue::new(format!(
                    "duplicate service_id \"{id}\" declared by two modules: {} and {}",
                    previous.dir, project.dir
                ))
                .at_file(manifest_file_of(&project.dir))
                .at_service(id)
                .at_field("project.service_id")
                .with_hint(format!(
                    "service_id must be unique per workspace; rename one module's id, e.g. \"{id}-b\""
                )),
            );
        } else {
            ids.insert(id.to_owned(), project);
        }
        if let Some(proxy) = &project.manifest.proxy {
            if let Some(previous) = routes.get(proxy.path.as_str()) {
                issues.push(
                    ValidationIssue::new(format!(
                        "duplicate [proxy].path \"{}\" declared by services \"{}\" (dir {}) and \"{}\" (dir {})",
                        proxy.path,
                        previous.service_id(),
                        previous.dir,
                        project.service_id(),
                        project.dir
                    ))
                    .at_file(manifest_file_of(&project.dir))
                    .at_service(project.service_id())
                    .at_field("proxy.path")
                    .with_hint(format!(
                        "each web service needs a unique path prefix, e.g. \"{}\" in {} and \"{}\" in {}; pingap routes by path, so identical paths are ambiguous",
                        suggest_path(previous.service_id(), &proxy.path),
                        manifest_file_of(&previous.dir),
                        suggest_path(project.service_id(), &proxy.path),
                        manifest_file_of(&project.dir),
                    )),
                );
            } else {
                routes.insert(proxy.path.clone(), project);
            }
            // catch-all 冲突只报一次（在第二个声明方出现时）：按 catch_all_reported
            // 去重，N 个 "/" 服务不再产生 N 条几乎相同的报告。
            if proxy.path == "/"
                && !catch_all_reported
                && let Some(previous_catch_all) = enabled.iter().find(|other| {
                    other.service_id() != project.service_id()
                        && other
                            .manifest
                            .proxy
                            .as_ref()
                            .is_some_and(|proxy| proxy.path == "/")
                })
            {
                catch_all_reported = true;
                issues.push(
                    ValidationIssue::new(format!(
                        "multiple catch-all routes (proxy.path = \"/\"): \"{}\" (dir {}) and \"{}\" (dir {})",
                        previous_catch_all.service_id(),
                        previous_catch_all.dir,
                        project.service_id(),
                        project.dir
                    ))
                    .at_file(manifest_file_of(&project.dir))
                    .at_field("proxy.path")
                    .with_hint(
                        "only one service may own the root path; give the other a prefix like \"/app/\"",
                    ),
                );
            }
        }
    }
    for project in &enabled {
        for dependency in &project.manifest.run.depends_on {
            if dependency == project.service_id() {
                issues.push(
                    ValidationIssue::new(format!(
                        "service \"{}\" depends on itself",
                        project.service_id()
                    ))
                    .at_file(manifest_file_of(&project.dir))
                    .at_service(project.service_id())
                    .at_field("run.depends_on")
                    .with_hint("remove the self reference"),
                );
            } else if !ids.contains_key(dependency) {
                issues.push(
                    ValidationIssue::new(format!(
                        "service \"{}\" depends on missing or disabled service \"{dependency}\"",
                        project.service_id()
                    ))
                    .at_file(manifest_file_of(&project.dir))
                    .at_service(project.service_id())
                    .at_field("run.depends_on")
                    .with_hint(format!(
                        "check {dependency}: it may be disabled, missing, or have its own \
                         validation errors listed above (fix those first — do NOT remove this \
                         dependency to silence the error)"
                    )),
                );
            }
        }
    }
    issues.extend(cycle_issues(&enabled));
    issues
}

/// 兼容入口：fast-fail 版拓扑校验（取第一个 issue）。
pub fn validate_topology(projects: &[DiscoveredProject]) -> Result<Vec<String>, ManifestError> {
    let issues = collect_topology_issues(projects);
    if let Some(issue) = issues.into_iter().next() {
        return Err(ManifestError::Validation(issue.to_string()));
    }
    let enabled: Vec<_> = projects
        .iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();
    // collect_topology_issues 已含环检测（cycle_issues→topo_sort），issues 为空
    // 则图必无环——topo_sort 必 Ok（Err 集仅在有环/缺依赖时非空，均已被上方拦截）。
    match topo_sort(&enabled) {
        Ok(order) => Ok(order),
        Err(_) => Err(ManifestError::Validation(
            "internal: topology passed validation but sort found a cycle".into(),
        )),
    }
}

/// 依赖环检测：报出无法出队的服务（真环成员 + 被环阻塞的下游）及各自声明文件。
fn cycle_issues(enabled: &[&DiscoveredProject]) -> Vec<ValidationIssue> {
    match topo_sort(enabled) {
        Ok(_) => Vec::new(),
        Err(cycle_ids) => {
            let files: Vec<String> = enabled
                .iter()
                .filter(|project| cycle_ids.contains(&project.service_id().to_owned()))
                .map(|project| manifest_file_of(&project.dir))
                .collect();
            vec![
                ValidationIssue::new(format!(
                    "service dependency cycle detected (or blocked by a cycle): {}",
                    cycle_ids.join(", ")
                ))
                .at_field("run.depends_on")
                .with_hint(format!(
                    "break the cycle in one of: {} (depends_on must form a DAG; \
                     non-cycle members in the list are blocked downstream — fix \
                     the cycle and they resolve themselves)",
                    files.join(", ")
                )),
            ]
        }
    }
}

/// Kahn 拓扑排序内核（cycle_issues 与 validate_topology 的单一实现）：
/// `Ok(order)` = 拓扑序；`Err(cycle_ids)` = 无法出队的服务 id。
///
/// 构图时过滤指向 enabled 集之外的依赖——缺依赖已由上方产出专属 issue
/// （含修复指引），不过滤会被归入 Err 集，在 lenient 全量呈现里制造
/// "cycle detected" 伪环误报（缺依赖的服务自身无环）。真环的下游无辜
/// 节点（依赖环成员）仍会留在 Err 集，报文措辞 accordingly。
fn topo_sort(enabled: &[&DiscoveredProject]) -> Result<Vec<String>, Vec<String>> {
    let ids: std::collections::HashSet<&str> =
        enabled.iter().map(|project| project.service_id()).collect();
    let dependencies: BTreeMap<String, BTreeSet<String>> = enabled
        .iter()
        .map(|project| {
            (
                project.service_id().to_owned(),
                project
                    .manifest
                    .run
                    .depends_on
                    .iter()
                    .filter(|dependency| ids.contains(dependency.as_str()))
                    .cloned()
                    .collect(),
            )
        })
        .collect();
    let mut remaining = dependencies;
    let mut result = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|(_, dependencies)| {
                dependencies
                    .iter()
                    .all(|dependency| result.contains(dependency))
            })
            .map(|(id, _)| id.clone())
            .collect();
        if ready.is_empty() {
            return Err(remaining.keys().cloned().collect());
        }
        for id in ready {
            remaining.remove(&id);
            result.push(id);
        }
    }
    Ok(result)
}

/// 路由冲突时的建议路径：按 service_id 生成唯一前缀（与冲突原值无关——
/// 冲突本身说明原值不可共用，建议值必须直接可抄）。
fn suggest_path(service_id: &str, _conflicting: &str) -> String {
    format!("/{service_id}/")
}

pub(super) fn collect_log_issues(sources: &[LogSource]) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let mut ids = BTreeSet::new();
    for source in sources {
        if !is_dns1123_label(&source.id) {
            issues.push(
                ValidationIssue::new(format!(
                    "logs source id must be a DNS-1123 label: {}",
                    source.id
                ))
                .at_field(format!("logs.sources[id={}].id", source.id)),
            );
        }
        if !ids.insert(&source.id) {
            issues.push(
                ValidationIssue::new(format!("duplicate logs source id: {}", source.id))
                    .at_field("logs.sources.id"),
            );
        }
        if let Some(issue) = relative_path_issue(&source.glob, "logs.sources.glob") {
            issues.push(issue.at_field("logs.sources.glob"));
        }
        if source.glob.contains('/') || source.glob.contains('\\') {
            issues.push(
                ValidationIssue::new(format!(
                    "logs.sources.glob must be relative to the service log directory: {}",
                    source.glob
                ))
                .at_field("logs.sources.glob"),
            );
        }
        if source.format == LogFormat::Jsonl && source.multiline_start_pattern.is_some() {
            issues.push(
                ValidationIssue::new("multiline_start_pattern is only valid for text logs")
                    .at_field("logs.sources.multiline_start_pattern")
                    .with_hint("jsonl entries are single-line; remove the pattern or use format = \"text\""),
            );
        }
    }
    issues
}
