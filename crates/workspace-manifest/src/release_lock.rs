use std::collections::{BTreeMap, BTreeSet};

use crate::{
    DiscoveredProject, INTERNAL_PORT_MAX, INTERNAL_PORT_MIN, LockedPingap, LockedService,
    ManifestError, ReleaseLock, SCHEMA_VERSION, WorkspaceManifest, validate_topology,
    validate_workspace,
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
