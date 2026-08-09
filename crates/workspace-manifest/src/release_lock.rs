use std::collections::{BTreeMap, BTreeSet};

use crate::{
    DiscoveredProject, INTERNAL_PORT_MAX, INTERNAL_PORT_MIN, LoadError, LockedPingap,
    LockedService, ManifestError, ReleaseLock, SCHEMA_VERSION, WorkspaceManifest,
    validate_topology, validate_workspace,
};

const RESERVED_RUNTIME_PORTS: [u16; 2] = [5432, 7681];

pub struct ReleaseMetadata<'a> {
    pub release_id: &'a str,
    pub pingap_version: &'a str,
    pub pingap_commit: &'a str,
    pub minimum_app_cli_version: &'a str,
    /// Manifest v1 compatibility field containing a versioned runtime image reference.
    pub runtime_image_digest: &'a str,
}

pub fn build_release_lock(
    workspace: &WorkspaceManifest,
    projects: &[DiscoveredProject],
    metadata: ReleaseMetadata<'_>,
) -> Result<ReleaseLock, ManifestError> {
    validate_workspace(workspace)?;
    let dependency_order = validate_topology(projects)?;
    let ports = allocate_ports(projects)?;
    let by_id: BTreeMap<_, _> = projects
        .iter()
        .map(|project| (project.service_id(), project))
        .collect();
    let services = dependency_order
        .into_iter()
        .map(|id| {
            let project = by_id.get(id.as_str()).ok_or_else(|| {
                ManifestError::Validation(format!("service disappeared while locking: {id}"))
            })?;
            let manifest = &project.manifest;
            let port = ports.get(&id).copied().ok_or_else(|| {
                ManifestError::Validation(format!("port missing while locking: {id}"))
            })?;
            Ok(LockedService {
                service_id: id,
                name: manifest.project.name.clone(),
                dir: project.dir.clone(),
                r#type: manifest.project.r#type.clone(),
                kind: manifest.project.kind.clone(),
                enabled: manifest.project.enabled,
                port,
                run: manifest.run.clone(),
                health: manifest.health.clone(),
                proxy: manifest.proxy.clone(),
                logs: manifest.logs.sources.clone(),
                env: manifest.env.clone(),
            })
        })
        .collect::<Result<Vec<_>, ManifestError>>()?;
    // 校验 workspace 级 [health].bridge_service(若声明)指向真实存在的 service_id(Fail Fast)。
    let bridge_service = workspace.health.bridge_service.clone();
    if let Some(ref bridge_id) = bridge_service
        && !services
            .iter()
            .any(|service| &service.service_id == bridge_id)
    {
        return Err(ManifestError::Validation(format!(
            "[health].bridge_service references unknown service_id '{bridge_id}'"
        )));
    }
    Ok(ReleaseLock {
        schema_version: SCHEMA_VERSION,
        release_id: metadata.release_id.to_owned(),
        workspace_name: workspace.workspace.name.clone(),
        pingap: LockedPingap {
            mode: workspace.pingap.mode.clone(),
            config: workspace.pingap.config.clone(),
            version: metadata.pingap_version.to_owned(),
            commit: metadata.pingap_commit.to_owned(),
        },
        minimum_app_cli_version: metadata.minimum_app_cli_version.to_owned(),
        runtime_image_digest: metadata.runtime_image_digest.to_owned(),
        services,
        bridge_service,
    })
}

fn allocate_ports(projects: &[DiscoveredProject]) -> Result<BTreeMap<String, u16>, ManifestError> {
    let range_size = usize::from(INTERNAL_PORT_MAX - INTERNAL_PORT_MIN + 1);
    let capacity = range_size - RESERVED_RUNTIME_PORTS.len();
    let mut enabled: Vec<_> = projects
        .iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();
    enabled.sort_by(|left, right| left.service_id().cmp(right.service_id()));
    if enabled.len() > capacity {
        return Err(ManifestError::Validation(
            "too many enabled services".into(),
        ));
    }
    let mut used = BTreeSet::new();
    let mut result = BTreeMap::new();
    for project in enabled {
        let hash = project
            .service_id()
            .bytes()
            .fold(2_166_136_261_u32, |accumulator, byte| {
                accumulator.wrapping_mul(16_777_619) ^ u32::from(byte)
            });
        let mut candidate = INTERNAL_PORT_MIN + (hash as usize % range_size) as u16;
        while used.contains(&candidate) || RESERVED_RUNTIME_PORTS.contains(&candidate) {
            candidate = if candidate == INTERNAL_PORT_MAX {
                INTERNAL_PORT_MIN
            } else {
                candidate + 1
            };
        }
        used.insert(candidate);
        result.insert(project.service_id().to_owned(), candidate);
    }
    Ok(result)
}

/// 仅用于探测 `schema_version` 的最小结构。不带 `deny_unknown_fields`，故可从任意
/// 版本的 lock 内容里取出版本号，再据此分发到对应版本的反序列化/迁移路径。
#[derive(serde::Deserialize)]
struct VersionPeek {
    schema_version: u32,
}

/// 版本感知地加载 `release.lock.toml`。
///
/// 这是 release lock 的**唯一读取入口**（app-cli 运行时 + app_manager 注入运行时身份都用它）。
/// 按 `schema_version` 分发：
/// - 等于 [`SCHEMA_VERSION`]：反序列化成当前 [`ReleaseLock`] 并校验运行时不变量。
/// - 大于当前：[`LoadError::NewerThanKnown`]（正常被 `minimum_app_cli_version` 门禁提前拦截）。
/// - 已注册的历史版本：走迁移链上移到当前（Stage 1 落地时在此加分支）。
/// - 其它：[`LoadError::UnknownVersion`]。
///
/// 当前只有 v1。未来引入 v2 时（破坏性变更）：
/// 1. 把当前 `ReleaseLock` 形状冻结成 `release_lock::legacy::v1::LegacyV1Lock`（各 struct 照贴
///    `deny_unknown_fields`，保 fail-fast）；
/// 2. `ReleaseLock` 升为 v2 当前型，`SCHEMA_VERSION = 2`；
/// 3. 加 `migrate_v1_v2(LegacyV1Lock) -> Result<ReleaseLock, LoadError>`；
/// 4. 在下方 `match` 加 `1 => { let l1 = LegacyV1Lock::parse(content)?; migrate_v1_v2(l1) }`；
/// 5. golden 测试加 v1→v2 快照（`tests/fixtures/lock_v1.toml` 永久保留）。
///
/// 详见 crate 顶部"配置演进策略"。
pub fn load_release_lock(content: &str) -> Result<ReleaseLock, LoadError> {
    let peek: VersionPeek =
        toml::from_str(content).map_err(|error| LoadError::Parse(error.to_string()))?;
    match peek.schema_version {
        v if v == SCHEMA_VERSION => parse_current(content),
        v if v > SCHEMA_VERSION => Err(LoadError::NewerThanKnown {
            got: v,
            known: SCHEMA_VERSION,
        }),
        // 历史版本分支在此扩展（Stage 1）：v => parse LegacyV{v} → 迁移链 → 当前型。
        // C 类"不可推导"的迁移步骤返回 LoadError::RequiresRebuild，由平台侧重锁（Stage 2）。
        v => Err(LoadError::UnknownVersion(v)),
    }
}

/// 反序列化成当前版本 [`ReleaseLock`] 并校验运行时不变量。
///
/// 保留 `#[serde(deny_unknown_fields)]` 的 fail-fast：当前版本的 lock 出现未知字段即报错
/// （v1 当前型一旦见未知字段，必是真正的非法/外来内容）。
fn parse_current(content: &str) -> Result<ReleaseLock, LoadError> {
    let lock: ReleaseLock =
        toml::from_str(content).map_err(|error| LoadError::Parse(error.to_string()))?;
    if lock.services.is_empty() {
        return Err(LoadError::Invariant("release lock has no services".into()));
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{
        BuildSection, HealthSection, LogsSection, ProjectKind, ProjectManifest, ProjectMeta,
        ProjectType, RunSection,
    };

    use super::*;

    fn discovered(id: &str) -> DiscoveredProject {
        DiscoveredProject {
            dir: id.into(),
            manifest: ProjectManifest {
                schema_version: 1,
                project: ProjectMeta {
                    service_id: id.into(),
                    name: id.into(),
                    r#type: ProjectType::Go,
                    kind: ProjectKind::Web,
                    enabled: true,
                },
                build: BuildSection {
                    command: vec!["true".into()],
                    artifact: "artifact.zip".into(),
                },
                run: RunSection {
                    command: vec!["./server".into()],
                    migrate: Vec::new(),
                    depends_on: Vec::new(),
                    shutdown_timeout_seconds: 30,
                },
                health: HealthSection::default(),
                proxy: None,
                logs: LogsSection::default(),
                env: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn deterministic_ports_are_stable_across_input_order() {
        let a = discovered("backend-go");
        let b = discovered("frontend");
        let first = allocate_ports(&[a.clone(), b.clone()]).expect("ports");
        let second = allocate_ports(&[b, a]).expect("ports");
        assert_eq!(first, second);
    }

    #[test]
    fn allocated_ports_never_use_runtime_reserved_ports() {
        let projects: Vec<_> = (0..512)
            .map(|index| discovered(&format!("service-{index}")))
            .collect();
        let ports = allocate_ports(&projects).expect("ports");
        assert!(
            ports
                .values()
                .all(|port| !RESERVED_RUNTIME_PORTS.contains(port))
        );
    }
}
