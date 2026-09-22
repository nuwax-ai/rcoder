//! Docker builder restart templates.
//!
//! Docker builder containers are created with auto-remove, so a compute stop
//! deletes the physical container. A Restart therefore captures an inspect
//! checkpoint before the stop and recreates a STOPPED replacement afterwards:
//! the platform's checkpoint CAS must succeed before the replacement starts,
//! mirroring the K8s zero-replica restore contract.
//!
//! The archive replays the captured runtime configuration (image, env, binds,
//! network). Builder container env never carries credentials — they travel on
//! the runtime-operation channel and are excluded from disk persistence — and
//! the archive file is written with owner-only permissions. Identity labels
//! are re-derived from the operation context so the replacement binds to the
//! current lifecycle instead of replaying possibly stale labels.
use super::docker_runtime::DockerRuntime;
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use sha2::{Digest as _, Sha256};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderRestartTemplate,
    ServiceType, UserAppExecutionContext,
};
use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    path::PathBuf,
};

/// Marks a container created by restart restore. A later restore retry with
/// the same archive accepts exactly this replacement instead of conflicting.
const RESTORE: &str = "rcoder.io/restart-archive-uid";
const ARCHIVE_LIMIT: u64 = 96 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    source: BuilderControlTarget,
    image: String,
    /// HostConfig.Binds entries in their original `source:target[:options]`
    /// form; replayed verbatim so mount options survive the round trip.
    binds: Vec<String>,
    network_mode: String,
    network_name: Option<String>,
    env: Vec<String>,
    cmd: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
    working_dir: Option<String>,
    hostname: Option<String>,
    domainname: Option<String>,
    security_opt: Option<Vec<String>>,
    memory: Option<i64>,
    nano_cpus: Option<i64>,
    cpu_shares: Option<i64>,
    cpuset_cpus: Option<String>,
}

fn fail(message: impl std::fmt::Display) -> Error {
    Error::DockerError(format!("Docker builder restart archive: {message}"))
}

fn conflict(message: impl Into<String>) -> Error {
    Error::Conflict(message.into())
}

fn sha256_hex(payload: &[u8]) -> String {
    Sha256::digest(payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn archive_path(context: &UserAppExecutionContext, name: &str) -> Result<PathBuf> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::ConfigurationError)?;
    Ok(
        PathBuf::from(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
            .join(".app-operation-receipts")
            .join(&context.app_id)
            .join(format!("{name}.json")),
    )
}

/// Resolved bind mounts as comparable witnesses. `name` is the container
/// mount target, `uid` the host source path; both must survive unchanged.
pub(super) fn bind_witness(
    inspect: &bollard::models::ContainerInspectResponse,
) -> Result<Vec<AppResourceIdentity>> {
    let mut binds: Vec<AppResourceIdentity> = inspect
        .mounts
        .iter()
        .flatten()
        .filter(|mount| mount.typ.as_deref() == Some("bind"))
        .filter_map(|mount| {
            let source = mount.source.as_deref()?;
            let destination = mount.destination.as_deref()?;
            if source.is_empty() || destination.is_empty() {
                return None;
            }
            Some(AppResourceIdentity {
                kind: AppResourceKind::HostPath,
                name: destination.to_string(),
                uid: source.to_string(),
                resource_version: None,
            })
        })
        .collect();
    binds.sort_by(|a, b| (&a.name, &a.uid).cmp(&(&b.name, &b.uid)));
    if binds.is_empty() {
        return Err(conflict(
            "Builder has no bind mounts; its workspace would not survive a restart",
        ));
    }
    Ok(binds)
}

fn config_of(
    inspect: &bollard::models::ContainerInspectResponse,
) -> Result<&bollard::models::ContainerConfig> {
    inspect
        .config
        .as_ref()
        .ok_or_else(|| conflict("Builder configuration is missing from inspect"))
}

async fn persist_exclusive(path: PathBuf, payload: Vec<u8>) -> Result<()> {
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("Archive parent missing"))?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                // Owner-only: the payload is builder configuration, not a
                // credential, but it still describes the deployment shape.
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&payload)?;
            file.sync_all()?;
            // Never overwrite evidence belonging to another capture; an
            // identical payload is the accepted idempotent case.
            match std::fs::hard_link(&temporary, &path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if std::fs::read(&path)? != payload {
                        return Err(std::io::Error::other(
                            "Docker restart archive identity changed",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = std::fs::remove_file(&temporary)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "Could not remove restart archive temporary file");
        }
        result
    })
    .await
    .map_err(|error| fail(format!("Archive writer worker: {error}")))?
    .map_err(|error| fail(format!("Persist restart archive: {error}")))
}

async fn read_archive(path: PathBuf) -> Result<Option<Vec<u8>>> {
    tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(fail(format!("Inspect restart archive: {error}"))),
        };
        if !metadata.is_file() || metadata.len() > ARCHIVE_LIMIT {
            return Err(conflict("Invalid Docker restart archive file"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .map_err(|error| fail(format!("Open restart archive: {error}")))?;
        let mut bytes = Vec::new();
        file.take(ARCHIVE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| fail(format!("Read restart archive: {error}")))?;
        if bytes.len() as u64 > ARCHIVE_LIMIT {
            return Err(conflict("Docker restart archive exceeds limit"));
        }
        Ok(Some(bytes))
    })
    .await
    .map_err(|error| fail(format!("Archive reader worker: {error}")))?
}

/// Bind witnesses record the daemon-reported HOST path, but this process may
/// itself run in a container where that path is mounted elsewhere. Rewrite
/// through the stable `userapp-workspace` anchor onto this process's own
/// workspace root; fall back to the raw path (host-side rcoder).
fn bind_source_exists(host_path: &str) -> Result<bool> {
    let raw = std::path::Path::new(host_path);
    if raw.is_dir() {
        return Ok(true);
    }
    let components: Vec<std::ffi::OsString> = raw
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect();
    let Some(anchor) = components
        .iter()
        .position(|component| component == "userapp-workspace")
    else {
        return Ok(false);
    };
    let mut visible = PathBuf::from(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT);
    for component in &components[anchor + 1..] {
        visible.push(component);
    }
    Ok(visible.is_dir())
}

fn inspect_absent(
    result: std::result::Result<bollard::models::ContainerInspectResponse, bollard::errors::Error>,
    what: &str,
) -> Result<Option<bollard::models::ContainerInspectResponse>> {
    match result {
        Ok(inspect) => {
            // Auto-remove deletion is asynchronous: a container answering
            // inspect while removing/dead can never be started again and its
            // UID can never come back — it is already absent for every
            // restart/restore decision.
            let removing = inspect.state.as_ref().is_some_and(|state| {
                state.dead == Some(true)
                    || state.status.as_ref().is_some_and(|status| {
                        matches!(
                            status,
                            bollard::models::ContainerStateStatusEnum::REMOVING
                                | bollard::models::ContainerStateStatusEnum::DEAD
                        )
                    })
            });
            if removing {
                Ok(None)
            } else {
                Ok(Some(inspect))
            }
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(None),
        Err(error) => Err(fail(format!("{what}: {error}"))),
    }
}

impl DockerRuntime {
    pub(super) async fn archive_builder_template(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<BuilderRestartTemplate>> {
        target.validate().map_err(Error::ConfigurationError)?;
        let Some(resource) = &target.workload else {
            return Err(conflict("Original builder container is missing"));
        };
        if resource.kind != AppResourceKind::Container {
            return Err(conflict(
                "Only container builders archive restart templates",
            ));
        }
        let client = self.inner.get_docker_client();
        let inspect = inspect_absent(
            client.inspect_container(&resource.uid, None).await,
            "Inspect builder before archive",
        )?
        .ok_or_else(|| conflict("Original builder container is missing"))?;
        if super::docker_builder_control::control_identity_with_binding(
            &inspect,
            &resource.name,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )? != *resource
        {
            return Err(conflict("Builder changed before template capture"));
        }
        let config = config_of(&inspect)?;
        let host = inspect.host_config.as_ref();
        // Bind specifications live either in the legacy `Binds` strings or as
        // structured `Mounts` entries (the ContainerConfigBuilder path). Replay
        // needs one canonical form; both describe the same host bind sources.
        let mut binds = host.and_then(|h| h.binds.clone()).unwrap_or_default();
        if binds.is_empty() {
            binds = host
                .and_then(|h| h.mounts.as_ref())
                .into_iter()
                .flatten()
                .filter(|mount| mount.typ == Some(bollard::models::MountType::BIND))
                .map(|mount| {
                    let source = mount.source.clone().unwrap_or_default();
                    let target = mount.target.clone().unwrap_or_default();
                    match mount.read_only {
                        Some(true) => format!("{source}:{target}:ro"),
                        _ => format!("{source}:{target}"),
                    }
                })
                .collect();
        }
        if binds.is_empty() {
            // Last resort: the daemon-resolved mount table. It carries no
            // option flags, but the resolved host sources are authoritative.
            binds = inspect
                .mounts
                .iter()
                .flatten()
                .filter(|mount| mount.typ.as_deref() == Some("bind"))
                .filter_map(|mount| {
                    let source = mount.source.as_deref()?;
                    let target = mount.destination.as_deref()?;
                    if source.is_empty() || target.is_empty() {
                        return None;
                    }
                    Some(format!("{source}:{target}"))
                })
                .collect();
        }
        let archive = Archive {
            source: target.clone(),
            image: config
                .image
                .clone()
                .ok_or_else(|| conflict("Builder image is missing from inspect"))?,
            binds,
            network_mode: host
                .and_then(|h| h.network_mode.clone())
                .unwrap_or_default(),
            network_name: inspect
                .network_settings
                .as_ref()
                .and_then(|settings| settings.networks.as_ref())
                .and_then(|networks| networks.keys().next().cloned()),
            env: config.env.clone().unwrap_or_default(),
            cmd: config.cmd.clone(),
            entrypoint: config.entrypoint.clone(),
            working_dir: config.working_dir.clone(),
            hostname: config.hostname.clone(),
            domainname: config.domainname.clone(),
            security_opt: host.and_then(|h| h.security_opt.clone()),
            memory: host.and_then(|h| h.memory),
            nano_cpus: host.and_then(|h| h.nano_cpus),
            cpu_shares: host.and_then(|h| h.cpu_shares),
            cpuset_cpus: host.and_then(|h| h.cpuset_cpus.clone()),
        };
        let volumes = bind_witness(&inspect)?;
        if archive.binds.is_empty() {
            return Err(conflict(
                "Builder inspect carries no bind specification for replay",
            ));
        }
        // Drift check: the captured identity must still hold after the reads.
        let after = self
            .capture_builder_compute_with_binding(
                &target.context,
                target.resource_binding.as_ref(),
                false,
            )
            .await?;
        if after != *target {
            return Err(conflict("Builder changed during template capture"));
        }
        let payload = serde_json::to_vec(&archive)
            .map_err(|error| fail(format!("Encode archive: {error}")))?;
        if payload.len() as u64 > ARCHIVE_LIMIT {
            return Err(Error::ConfigurationError(
                "Restart template exceeds archive limit".into(),
            ));
        }
        let digest = sha256_hex(&payload);
        let name = format!("builder-restart-{}.{}", &digest[..32], &digest[32..]);
        let path = archive_path(&target.context, &name)?;
        persist_exclusive(path, payload).await?;
        Ok(Some(BuilderRestartTemplate {
            source: target.clone(),
            archive: AppResourceIdentity {
                kind: AppResourceKind::File,
                name,
                uid: digest,
                resource_version: None,
            },
            volumes,
        }))
    }

    pub(super) async fn restore_builder_template(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<BuilderControlTarget> {
        template
            .source
            .validate()
            .map_err(Error::ConfigurationError)?;
        if template.archive.kind != AppResourceKind::File
            || template.archive.name.is_empty()
            || template.archive.uid.is_empty()
        {
            return Err(conflict("Invalid restart archive identity"));
        }
        let path = archive_path(&template.source.context, &template.archive.name)?;
        let payload = read_archive(path)
            .await?
            .ok_or_else(|| conflict("Restart archive payload is missing"))?;
        if sha256_hex(&payload) != template.archive.uid {
            return Err(conflict("Restart archive content changed"));
        }
        let archive: Archive = serde_json::from_slice(&payload)
            .map_err(|error| fail(format!("Decode restart archive: {error}")))?;
        if archive.source != template.source || archive.image.is_empty() {
            return Err(conflict("Restart archive does not match checkpoint"));
        }
        let original = template
            .source
            .workload
            .as_ref()
            .filter(|resource| resource.kind == AppResourceKind::Container)
            .ok_or_else(|| conflict("Restart source container is missing"))?;
        let client = self.inner.get_docker_client();
        // The old physical instance must already be gone; its durable stopped
        // boundary (or absence checkpoint) proves the exit, never this probe.
        if inspect_absent(
            client.inspect_container(&original.uid, None).await,
            "Confirm original builder exit",
        )?
        .is_some()
        {
            return Err(conflict("Original builder still exists"));
        }
        // Idempotent re-entry: an interrupted restore may have created the
        // replacement already. Accept only this archive's marker, never a
        // running occupant or a different container under the same name.
        let mut resumed = false;
        match inspect_absent(
            client.inspect_container(&original.name, None).await,
            "Inspect restart target",
        )? {
            None => {}
            Some(existing) => {
                let labels = existing
                    .config
                    .as_ref()
                    .and_then(|config| config.labels.clone())
                    .unwrap_or_default();
                if labels.get(RESTORE) != Some(&template.archive.uid)
                    || existing.state.as_ref().and_then(|state| state.running) == Some(true)
                {
                    return Err(conflict("Another builder occupies restart target"));
                }
                resumed = true;
            }
        }
        if !resumed {
            for volume in &template.volumes {
                if !bind_source_exists(&volume.uid)? {
                    return Err(conflict(format!(
                        "Restart bind source is missing: {}",
                        volume.uid
                    )));
                }
            }
            self.inner
                .ensure_image_exists(&archive.image)
                .await
                .map_err(|error| {
                    Error::DockerError(format!("Prepare restart image {}: {error}", archive.image))
                })?;
            let body = restore_body(template, &archive)?;
            let options = bollard::query_parameters::CreateContainerOptions {
                name: Some(original.name.clone()),
                platform: self.inner.config.default_platform.clone(),
            };
            // A container mid-removal still owns its name: creation 409s with
            // "name already in use". The daemon finishes auto-remove quickly;
            // wait it out bounded instead of failing the whole restart.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let created = loop {
                match client
                    .create_container(Some(options.clone()), body.clone())
                    .await
                {
                    Ok(created) => break created,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 409,
                        message,
                    }) if message.contains("already in use")
                        && std::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    }
                    Err(error) => return Err(fail(format!("Create replacement builder: {error}"))),
                }
            };
            if created.id.is_empty() {
                return Err(fail("Replacement builder create returned no physical ID"));
            }
        }
        let restored = self
            .capture_builder_compute_with_binding(&template.source.context, None, false)
            .await?;
        let resource = restored
            .workload
            .as_ref()
            .filter(|resource| resource.kind == AppResourceKind::Container)
            .ok_or_else(|| conflict("Restart did not produce a replacement builder"))?;
        if resource.uid == original.uid {
            return Err(conflict("Restart did not produce a replacement builder"));
        }
        let inspect = client
            .inspect_container(&resource.uid, None)
            .await
            .map_err(|error| fail(format!("Verify replacement builder: {error}")))?;
        if bind_witness(&inspect)? != template.volumes {
            return Err(conflict("Restart workspace binds changed"));
        }
        // deploy-host：replacement 容器是新物理实例，刷新寻址登记（与
        // compute_mode 钩子可能双触发——整条目幂等替换，无害）
        #[cfg(feature = "deploy-host")]
        if shared_types::is_deploy_host()
            && let Err(error) = crate::deploy_host_ports::register_reach_from_inspect(
                &resource.name,
                None,
                &inspect,
            )
        {
            tracing::warn!(
                "[deploy-host] builder restart reach refresh {} failed: {error}",
                resource.name
            );
        }
        Ok(restored)
    }

    pub(super) async fn remove_builder_restart_archive(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<()> {
        if template.archive.kind != AppResourceKind::File || template.archive.uid.is_empty() {
            return Err(conflict("Invalid restart archive identity"));
        }
        let path = archive_path(&template.source.context, &template.archive.name)?;
        match read_archive(path.clone()).await? {
            None => Ok(()),
            Some(payload) => {
                if sha256_hex(&payload) != template.archive.uid {
                    return Err(conflict("Restart archive content changed"));
                }
                tokio::task::spawn_blocking(move || std::fs::remove_file(&path))
                    .await
                    .map_err(|error| fail(format!("Archive cleanup worker: {error}")))?
                    .map_err(|error| fail(format!("Remove restart archive: {error}")))
            }
        }
    }

    pub(super) async fn capture_builder_volume_witness(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Vec<AppResourceIdentity>> {
        target.validate().map_err(Error::ConfigurationError)?;
        let Some(resource) = &target.workload else {
            return Err(conflict("Captured builder is absent"));
        };
        if resource.kind != AppResourceKind::Container {
            return Err(conflict("Only container builders expose bind witnesses"));
        }
        let inspect = self
            .inner
            .get_docker_client()
            .inspect_container(&resource.uid, None)
            .await
            .map_err(|error| fail(format!("Inspect builder volumes: {error}")))?;
        bind_witness(&inspect)
    }

    /// HostPath witnesses verify by the presence of the source directory;
    /// other kinds never belonged to a Docker capture and are rejected.
    pub(super) fn verify_recovered_host_volumes(volumes: &[AppResourceIdentity]) -> Result<()> {
        for volume in volumes {
            match volume.kind {
                AppResourceKind::HostPath => {
                    if !std::path::Path::new(&volume.uid).is_dir() {
                        return Err(conflict(format!(
                            "Recovered bind source is missing: {}",
                            volume.uid
                        )));
                    }
                }
                _ => {
                    return Err(conflict(
                        "Recovered volume kind is not a Docker bind witness",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Rebuild the create body from the archive. Identity labels are re-derived
/// from the operation context (never replayed), the restore marker makes a
/// retry idempotent, and the captured image/config replay verbatim.
fn restore_body(
    template: &BuilderRestartTemplate,
    archive: &Archive,
) -> Result<bollard::models::ContainerCreateBody> {
    let context = &template.source.context;
    let name = template
        .source
        .workload
        .as_ref()
        .map(|resource| resource.name.clone())
        .ok_or_else(|| conflict("Restart source container is missing"))?;
    let mut labels: HashMap<String, String> = context.resource_metadata().into_iter().collect();
    labels.insert(
        "service-type".into(),
        ServiceType::UserappBuilder
            .container_family_key()
            .to_string(),
    );
    labels.insert("identifier".into(), context.app_id.clone());
    labels.insert(RESTORE.into(), template.archive.uid.clone());
    let (networking_config, network_mode) = if archive.network_mode == "host" {
        (None, "host".to_string())
    } else {
        let endpoints = HashMap::from([(
            archive
                .network_name
                .clone()
                .unwrap_or_else(|| "bridge".to_string()),
            bollard::models::EndpointSettings {
                aliases: Some(vec![name.clone()]),
                ..Default::default()
            },
        )]);
        (
            Some(bollard::models::NetworkingConfig {
                endpoints_config: Some(endpoints),
            }),
            archive
                .network_name
                .clone()
                .unwrap_or_else(|| "bridge".into()),
        )
    };
    let host_config = bollard::models::HostConfig {
        binds: Some(archive.binds.clone()),
        network_mode: Some(network_mode),
        // Preserve the original lifecycle semantics: the replacement is also
        // removed by the daemon when it next stops.
        auto_remove: Some(true),
        security_opt: archive.security_opt.clone(),
        memory: archive.memory,
        nano_cpus: archive.nano_cpus,
        cpu_shares: archive.cpu_shares,
        cpuset_cpus: archive.cpuset_cpus.clone(),
        ..Default::default()
    };
    Ok(bollard::models::ContainerCreateBody {
        image: Some(archive.image.clone()),
        env: Some(archive.env.clone()),
        cmd: archive.cmd.clone(),
        entrypoint: archive.entrypoint.clone(),
        working_dir: archive.working_dir.clone(),
        labels: Some(labels),
        host_config: Some(host_config),
        networking_config,
        tty: Some(true),
        open_stdin: Some(true),
        hostname: archive.hostname.clone(),
        domainname: archive.domainname.clone(),
        ..Default::default()
    })
}
