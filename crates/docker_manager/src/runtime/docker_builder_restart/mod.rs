//! Docker builder restart templates.
//!
//! Docker builder containers are created with auto-remove, so a compute stop
//! deletes the physical container. A Restart therefore captures an inspect
//! checkpoint before the stop and recreates a STOPPED replacement afterwards:
//! the platform's checkpoint CAS must succeed before the replacement starts,
//! mirroring the K8s zero-replica restore contract.
//!
//! The archive replays the captured runtime configuration (image, env, binds,
//! network), including deployed FILE_SERVER_PROXY_TOKEN/APP_CLI_DEPLOY_TOKEN
//! credentials when present in the retained environment. The private payload
//! has owner-only 0600 permissions and never enters public checkpoints or logs;
//! checkpoints carry only its reference. PG operation inputs still use their
//! existing private operation receipts. Identity labels
//! are re-derived from the operation context so the replacement binds to the
//! current lifecycle instead of replaying possibly stale labels.
//!
//! The log mount is the one bind that is not replayed verbatim: the replacement
//! gets a new log directory instead, see [`log_directory`].
use super::docker_runtime::DockerRuntime;
use crate::log_directory::{DirectoryBeingReclaimed, PendingBinds};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use log_directory::{FreshLogDirectories, plan};
use log_label::RESTART_LOG_DIR;

/// 这次恢复给出的替代容器：按名称找到或刚创建的那一个，以及它应当有的挂载
#[derive(Debug, PartialEq)]
struct Replacement {
    /// 替代容器的 Docker ID。最终核验必须是同一个容器
    id: String,
    expected_volumes: Vec<AppResourceIdentity>,
}

/// 替代容器没建成。两种情况的善后不一样
enum CreateFailure {
    /// 请求没有发出去，或者 daemon 明确拒绝：替代容器不存在
    NotCreated(Error),
    /// 请求发出去了，结果不明：替代容器可能已经存在
    Unknown(Error),
}
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
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

async fn archive_path(context: &UserAppExecutionContext, name: &str) -> Result<PathBuf> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::ConfigurationError)?;
    Ok(super::docker_compute_receipt::receipts_base()
        .await?
        .join(".app-operation-receipts")
        .join(&context.app_id)
        .join(format!("{name}.json")))
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
        self.capture_builder_archive(target, false).await
    }

    pub(super) async fn recapture_stopped_builder_template(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<BuilderRestartTemplate> {
        self.capture_builder_archive(target, true)
            .await?
            .ok_or_else(|| conflict("Stopped builder recapture returned no template"))
    }

    pub(super) async fn inspect_private_builder_archive(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<Option<bool>> {
        let name = std::path::Path::new(&template.archive.name);
        let mut components = name.components();
        let safe_name = matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none();
        if template.source.validate().is_err()
            || template.archive.kind != AppResourceKind::File
            || template.archive.uid.is_empty()
            || !safe_name
            || !template
                .source
                .workload
                .as_ref()
                .is_some_and(|resource| resource.kind == AppResourceKind::Container)
            || template.volumes.is_empty()
        {
            return Ok(Some(false));
        }
        let path = archive_path(&template.source.context, &template.archive.name).await?;
        let payload = match read_archive(path).await {
            Ok(Some(payload)) => payload,
            Ok(None) | Err(Error::Conflict(_)) => return Ok(Some(false)),
            Err(error) => return Err(error),
        };
        if sha256_hex(&payload) != template.archive.uid {
            return Ok(Some(false));
        }
        let archive: Archive = match serde_json::from_slice(&payload) {
            Ok(archive) => archive,
            Err(_) => return Ok(Some(false)),
        };
        Ok(Some(
            archive.source == template.source
                && !archive.image.is_empty()
                && !archive.binds.is_empty(),
        ))
    }

    async fn capture_builder_archive(
        &self,
        target: &BuilderControlTarget,
        recovery: bool,
    ) -> Result<Option<BuilderRestartTemplate>> {
        target.validate().map_err(Error::ConfigurationError)?;
        if recovery {
            self.confirm_stopped_archive_recapture(target).await?;
        }
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
        let mut after = self
            .capture_builder_compute_with_binding(
                &target.context,
                target.resource_binding.as_ref(),
                false,
            )
            .await?;
        after.restart_image = target.restart_image.clone();
        after.restart_runtime_workspace = target.restart_runtime_workspace.clone();
        after.builder_startup_mode = target.builder_startup_mode;
        if after != *target {
            return Err(conflict("Builder changed during template capture"));
        }
        if recovery {
            if self.capture_builder_volume_witness(target).await? != volumes {
                return Err(conflict("Stopped builder volumes changed during recapture"));
            }
            self.confirm_stopped_archive_recapture(target).await?;
        }
        let payload = serde_json::to_vec(&archive)
            .map_err(|error| fail(format!("Encode archive: {error}")))?;
        if payload.len() as u64 > ARCHIVE_LIMIT {
            return Err(Error::ConfigurationError(
                "Restart template exceeds archive limit".into(),
            ));
        }
        let digest = sha256_hex(&payload);
        let name = if recovery {
            // Preserve the poisoned historical path. A fresh basename also
            // prevents another damaged recovery file becoming a new permanent
            // collision; the payload UID stays its actual content digest.
            format!(
                "builder-restart-recovered-{digest}.{}",
                uuid::Uuid::new_v4()
            )
        } else {
            format!("builder-restart-{}.{}", &digest[..32], &digest[32..])
        };
        let path = archive_path(&target.context, &name).await?;
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

    async fn confirm_stopped_archive_recapture(&self, target: &BuilderControlTarget) -> Result<()> {
        if !super::docker_compute_receipt::matches_stop(target).await?
            || !self.observe_retained_builder_stopped(target).await?
        {
            return Err(conflict(
                "Original operation stop and retained builder exit are not confirmed for recapture",
            ));
        }
        Ok(())
    }

    /// 读出这次恢复的归档并核验：引用完整，内容和模板记的摘要一致
    async fn load_archive(&self, template: &BuilderRestartTemplate) -> Result<Archive> {
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
        let path = archive_path(&template.source.context, &template.archive.name).await?;
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
        Ok(archive)
    }

    pub(super) async fn restore_builder_template(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<BuilderControlTarget> {
        let archive = self.load_archive(template).await?;
        let original = template
            .source
            .workload
            .as_ref()
            .filter(|resource| resource.kind == AppResourceKind::Container)
            .ok_or_else(|| conflict("Restart source container is missing"))?;
        let replacement = self
            .replace_builder_container(template, &archive, original)
            .await?;
        let client = self.inner.get_docker_client();
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
        // 核验的必须就是上面认下的那一个替代容器：同名容器在这之间被换掉的话，它记的日志
        // 目录 label 就不作数了
        if resource.uid != replacement.id {
            return Err(conflict(
                "Restart target was replaced before it could be verified",
            ));
        }
        let inspect = client
            .inspect_container(&resource.uid, None)
            .await
            .map_err(|error| fail(format!("Verify replacement builder: {error}")))?;
        if bind_witness(&inspect)? != replacement.expected_volumes {
            return Err(conflict("Restart workspace binds changed"));
        }
        // Replacement is deliberately stopped here. Its Direct IP and
        // published ports are not available until start_builder_compute.
        Ok(restored)
    }

    /// 用归档重放出替代 builder：先定下要换哪些日志目录，必要时移除仍被保留的旧容器，
    /// 核对其它 bind 源，再创建。
    ///
    /// 落在可回收日志根下面的 bind 不照原样重放，换成同级的新目录：旧目录可能已经过期，
    /// 日志清理器正在回收它。新目录登记为正在准备挂载，清理器碰不到它，这一段不需要和
    /// 清理器互斥。
    ///
    /// 判不出在不在日志根里的 bind 照原样重放，源登记到替代容器建出来：清理器对它同样判不
    /// 出来，登记着就不会删。日志根定位不完整、取不到路径空间都不让恢复失败，只告警。
    async fn replace_builder_container(
        &self,
        template: &BuilderRestartTemplate,
        archive: &Archive,
        original: &AppResourceIdentity,
    ) -> Result<Replacement> {
        let client = self.inner.get_docker_client();
        let mut replay = archive.clone();
        // 先把能提前定的都定下来，再去动旧容器：日志域在哪、哪些 bind 要换源。这一步失败时
        // 原容器还在，恢复可以原地重试
        let admission = self.log_domain().await;
        admission.warn_if_limited("Restart builder");
        let binds = plan(template, archive, &admission).await?;
        // 原样重放的 bind（落在日志根里的持久数据、判不出来的源）从现在起登记到替代容器建
        // 出来。要在动旧容器之前：旧容器一删，这些目录就不在任何容器的挂载里了，到建替代
        // 容器之间可能还要拉镜像。下面任何一步失败，守卫 drop，登记注销
        let mut replayed = PendingBinds::default();
        for source in &binds.replayed {
            replayed
                .claim(source)
                .await
                .map_err(DirectoryBeingReclaimed::into_container_creation_error)?;
        }
        // Old versions may retain stopped builders. Replace that exact instance
        // only after its stop receipt and mount witnesses are confirmed.
        if let Some(existing) = inspect_absent(
            client.inspect_container(&original.uid, None).await,
            "Confirm original builder exit",
        )? {
            if existing.state.as_ref().and_then(|state| state.running) != Some(false)
                || !super::docker_compute_receipt::matches_stop(&template.source).await?
                || bind_witness(&existing)? != template.volumes
            {
                return Err(conflict(
                    "Original builder stop or volume identity is not confirmed",
                ));
            }
            let image = archive
                .source
                .restart_image
                .as_deref()
                .unwrap_or(&archive.image);
            self.inner.ensure_image_exists(image).await.map_err(fail)?;
            client
                .remove_container(
                    &original.uid,
                    Some(bollard::query_parameters::RemoveContainerOptions {
                        force: false,
                        v: false,
                        ..Default::default()
                    }),
                )
                .await
                .map_err(fail)?;
            if inspect_absent(
                client.inspect_container(&original.uid, None).await,
                "Confirm builder removal",
            )?
            .is_some()
            {
                return Err(conflict("Original builder removal is not confirmed"));
            }
        }
        #[cfg(feature = "deploy-host")]
        if shared_types::is_deploy_host() {
            shared_types::published::unregister_if_physical(&original.name, &original.uid);
            shared_types::published::unregister_if_physical(
                &template.source.context.app_id,
                &original.uid,
            );
        }
        // Idempotent re-entry: an interrupted restore may have created the
        // replacement already. Accept only this archive's marker, never a
        // running occupant or a different container under the same name.
        if let Some(existing) = inspect_absent(
            client.inspect_container(&original.name, None).await,
            "Inspect restart target",
        )? {
            let resumed = self
                .resumed_replacement(template, archive, existing)
                .await?;
            let expected_volumes = resumed.expected_volumes(template);
            return Ok(Replacement {
                id: resumed.id,
                expected_volumes,
            });
        }
        // 先定下要换哪些日志目录，再核对其它 bind 源：核对不过就不建
        for volume in &template.volumes {
            // 日志目录换成新建的，旧的可能早已被清理器回收，不要求它还在
            if binds.plans.iter().any(|plan| plan.volume() == volume) {
                continue;
            }
            if !bind_source_exists(&volume.uid)? {
                return Err(conflict(format!(
                    "Restart bind source is missing: {}",
                    volume.uid
                )));
            }
        }
        let image = archive
            .source
            .restart_image
            .as_deref()
            .unwrap_or(&archive.image);
        self.inner
            .ensure_image_exists(image)
            .await
            .map_err(|error| {
                Error::DockerError(format!("Prepare restart image {image}: {error}"))
            })?;
        let mut fresh = FreshLogDirectories::create(
            admission.space(),
            &binds,
            replayed,
            &self.inner.config.log_reclaim_root,
        )
        .await?;
        fresh.apply(&mut replay, &binds.plans);
        let log_label = fresh.has_sources().then(|| fresh.label());
        // 创建请求马上发出。之后拿不到确切结果——包括这个 future 等应答时被取消——登记都会
        // 转去"结果不明"保留着：替代容器可能已经建出来、正绑着这些目录
        fresh.request_sent();
        match self
            .create_replacement(template, &replay, log_label.as_deref(), original)
            .await
        {
            Ok(id) => {
                fresh.created();
                Ok(Replacement {
                    id,
                    expected_volumes: fresh.expected_volumes(&template.volumes),
                })
            }
            // 确认没建成：登记注销，刚建的日志目录没人用，删掉
            Err(CreateFailure::NotCreated(error)) => {
                fresh.confirmed_absent().await;
                Err(error)
            }
            // 结果不明：目录和登记都原样留着。下一次恢复按名称找到替代容器、按它的 label
            // 核对；确实没建成的，登记过期之后由清理器回收
            Err(CreateFailure::Unknown(error)) => {
                fresh.outcome_unknown();
                Err(error)
            }
        }
    }

    /// Create the stopped replacement and return its physical ID.
    ///
    /// 只有请求发出去之前失败、或者 daemon 以 4xx 明确拒绝，才确认替代容器不存在。其余
    /// （连接断了、应答丢了、5xx、应答里没有 ID）结果不明。
    async fn create_replacement(
        &self,
        template: &BuilderRestartTemplate,
        replay: &Archive,
        log_label: Option<&str>,
        original: &AppResourceIdentity,
    ) -> std::result::Result<String, CreateFailure> {
        let client = self.inner.get_docker_client();
        let mut body =
            restore_body(template, replay, log_label).map_err(CreateFailure::NotCreated)?;
        crate::native_domain::stamp(client, &mut body)
            .await
            .map_err(|e| {
                CreateFailure::NotCreated(fail(format!("bind replacement execution domain: {e:#}")))
            })?;
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
                Ok(created) => break Ok(created),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 409,
                    message,
                }) if message.contains("already in use")
                    && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                Err(error) => break Err(error),
            }
        };
        let created = match created {
            Ok(created) => created,
            Err(error) => {
                return Err(match observed_rejection(&error) {
                    // daemon 明确拒绝了这次请求：替代容器不存在。保留结构化的拒绝，上层据此
                    // 按"确实被拒绝"归类，而不是按结果不明
                    Some(rejection) => CreateFailure::NotCreated(Error::RequestRejected(rejection)),
                    // 5xx、408、499、连接断了、应答丢了：请求可能已经生效，结果不明
                    None => {
                        CreateFailure::Unknown(fail(format!("Create replacement builder: {error}")))
                    }
                });
            }
        };
        if created.id.is_empty() {
            // 请求发出去并得到了成功应答，却拿不到 ID：容器可能已经建出来了
            return Err(CreateFailure::Unknown(fail(
                "Replacement builder create returned no physical ID",
            )));
        }
        Ok(created.id)
    }

    pub(super) async fn remove_builder_restart_archive(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<()> {
        if template.archive.kind != AppResourceKind::File || template.archive.uid.is_empty() {
            return Err(conflict("Invalid restart archive identity"));
        }
        let path = archive_path(&template.source.context, &template.archive.name).await?;
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

/// daemon 对这次请求给出的明确拒绝。只有 4xx 算（408、499 除外）：5xx 和超时类的应答
/// 说明请求可能已经生效。口径同 `RuntimeRequestRejection::from_status`，也就是
/// `container_runtime_api::runtime_mutation_outcome_unknown` 的反面
fn observed_rejection(
    error: &bollard::errors::Error,
) -> Option<shared_types::RuntimeRequestRejection> {
    match error {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        } => shared_types::RuntimeRequestRejection::from_status(*status_code, message.clone()),
        _ => None,
    }
}

/// Rebuild the create body from the archive. Identity labels are re-derived
/// from the operation context (never replayed), the restore marker makes a
/// retry idempotent, and the captured image/config replay verbatim — except the
/// log mounts, which follow `log_label` when there are any.
fn restore_body(
    template: &BuilderRestartTemplate,
    archive: &Archive,
    log_label: Option<&str>,
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
    if let Some(log_label) = log_label {
        labels.insert(RESTART_LOG_DIR.into(), log_label.to_owned());
    }
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
    // This is the same service-type port policy used by initial builder
    // creation. Do not serialize host ports into Archive: Docker allocates new
    // ports for every physical replacement.
    #[cfg(feature = "deploy-host")]
    let port_bindings = if shared_types::is_deploy_host()
        && !crate::deploy_host_ports::suppress_port_publishing()
    {
        Some(
            crate::deploy_host_ports::published_ports_for(&ServiceType::UserappBuilder)
                .into_iter()
                .map(|port| {
                    (
                        format!("{port}/tcp"),
                        Some(vec![bollard::models::PortBinding {
                            host_ip: Some("0.0.0.0".into()),
                            host_port: None,
                        }]),
                    )
                })
                .collect(),
        )
    } else {
        None
    };
    let host_config = bollard::models::HostConfig {
        binds: Some(archive.binds.clone()),
        network_mode: Some(network_mode),
        #[cfg(feature = "deploy-host")]
        port_bindings,
        // Stop releases the compute container; workspace data remains on its mounts.
        auto_remove: Some(true),
        security_opt: archive.security_opt.clone(),
        memory: archive.memory,
        nano_cpus: archive.nano_cpus,
        cpu_shares: archive.cpu_shares,
        cpuset_cpus: archive.cpuset_cpus.clone(),
        ..Default::default()
    };
    let mut env = archive.env.clone();
    if archive.source.builder_startup_mode == shared_types::BuilderStartupMode::ManagementOnly {
        env.retain(|entry| {
            !entry.starts_with(&format!("{}=", shared_types::APP_CLI_CONTROL_ONLY_ENV))
                && !entry.starts_with(&format!(
                    "{}=",
                    shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV
                ))
        });
        env.push(format!("{}=true", shared_types::APP_CLI_CONTROL_ONLY_ENV));
        env.push(format!(
            "{}={}",
            shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV,
            archive.source.context.operation_id
        ));
    }
    Ok(bollard::models::ContainerCreateBody {
        image: Some(
            archive
                .source
                .restart_image
                .clone()
                .unwrap_or_else(|| archive.image.clone()),
        ),
        env: Some(env),
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

mod log_directory;
mod log_label;

#[cfg(test)]
mod tests;
