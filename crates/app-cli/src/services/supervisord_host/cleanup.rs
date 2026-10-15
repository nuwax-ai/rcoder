//! Generation and owner cleanup keep their distinct resident lifetimes.
use super::*;

impl SupervisordHost {
    /// Called only after the guardian authenticated the draining generation.
    /// Returns the structured physical-cleanup outcome (recovery v2 §7.1) and
    /// persists it beside the receipts; business results are never inferred.
    pub(crate) async fn cleanup_generation(
        root: &Path,
    ) -> Result<runtime_supervisor::CleanupOutcome> {
        match std::fs::read(root.join(ENGINE_RECEIPT)) {
            Ok(bytes) => {
                let receipt: EngineReceipt = match serde_json::from_slice(&bytes) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        return Err(record_outcome(
                            root,
                            runtime_supervisor::CleanupOutcome::ObservationFailed {
                                reason: format!("decode engine receipt: {error}"),
                            },
                        ));
                    }
                };
                let generation: serde_json::Value =
                    match serde_json::from_slice(&std::fs::read(root.join("generation.json"))?) {
                        Ok(value) => value,
                        Err(error) => {
                            return Err(record_outcome(
                                root,
                                runtime_supervisor::CleanupOutcome::ObservationFailed {
                                    reason: format!("decode generation receipt: {error}"),
                                },
                            ));
                        }
                    };
                if !(generation["id"] == receipt.generation
                    && generation["supervisor"] == receipt.supervisor_id)
                {
                    // 记录属于另一管理域（前容器/换代）——保留为历史，不在
                    // 本域执行引擎清理（plan §7.4：不考古旧本地进程）。
                    return Err(record_outcome(
                        root,
                        runtime_supervisor::CleanupOutcome::ForeignIdentity {
                            detail: format!(
                                "supervisord cleanup generation identity differs: engine {}",
                                receipt.generation
                            ),
                        },
                    ));
                }
                let client = SupervisorClient::new(receipt.socket);
                if let Err(error) = client.ping().await {
                    return Err(record_outcome(
                        root,
                        runtime_supervisor::CleanupOutcome::ObservationFailed {
                            reason: format!(
                                "recorded supervisord engine is unavailable: {error:#}"
                            ),
                        },
                    ));
                }
                Self {
                    client,
                    conf_path: CONF_PATH.into(),
                    #[cfg(test)]
                    fixture_argv: None,
                }
                // 换代清理不发布 standby：入口保持当前配置（接管方的编排
                // 会发布新 active）；发布反而会与接管方竞态。
                .finish_verified_generation(root, &generation)
                .await?;
                runtime_supervisor::CleanupOutcome::Empty.record(root)?;
                Ok(runtime_supervisor::CleanupOutcome::Empty)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A management-only generation may never have acquired a
                // business engine. Reachability alone is not leftover work;
                // detect() also registers a receipt and cannot prove emptiness.
                let deadline = tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET;
                if let Err(error) = observe_unregistered_engine_empty_until(
                    &crate::xmlrpc::default_socket_path(),
                    &cleanup_observer_conf_path(),
                    deadline,
                )
                .await
                {
                    return Err(record_outcome(
                        root,
                        runtime_supervisor::CleanupOutcome::ObservationFailed {
                            reason: format!("observe generation without engine receipt: {error:#}"),
                        },
                    ));
                }
                runtime_supervisor::CleanupOutcome::Empty.record(root)?;
                Ok(runtime_supervisor::CleanupOutcome::Empty)
            }
            Err(error) => Err(record_outcome(
                root,
                runtime_supervisor::CleanupOutcome::ObservationFailed {
                    reason: format!("read supervisord engine receipt: {error}"),
                },
            )),
        }
    }

    /// 停掉全部**业务**动态组（`app-svc-*`；热部署切换 / 容器停服级联）。
    ///
    /// P1 常驻语义（V2-03/V2-07）：`app-pingap` 不在停止集——入口常驻。
    /// `standby_root = Some(runtime_root)` 时先发布 standby 到 active 并
    /// 经 admin hash 确认热载生效，**然后**才停止业务（摘流顺序；发布或
    /// 确认失败 = 停机失败返回 Err，旧服务保持可证实地运行）。`None` 用于
    /// 拆除/换代清理路径（入口保持当前配置，由接管方发布新 active）。
    pub(crate) async fn stop_all(&self, standby_root: Option<&Path>) -> Result<()> {
        if let Some(runtime_root) = standby_root {
            self.drain_entry(runtime_root).await?;
        } else {
            self.close_entry().await?;
        }
        self.stop_business().await
    }

    pub(crate) async fn finish_business_session(&self, runtime_root: Option<&Path>) -> Result<()> {
        let root = match runtime_root {
            Some(root) => Some(root.to_path_buf()),
            None => match self.resident_entry_settled().await? {
                Some(info)
                    if info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running =>
                {
                    let (config, _) = self.live_entry_binding(&info).await?;
                    Some(
                        config
                            .parent()
                            .and_then(Path::parent)
                            .context("resident configuration has no runtime root")?
                            .to_path_buf(),
                    )
                }
                Some(_) => configured_entry_spec(&self.conf_path)?
                    .map(|spec| entry_config_from_argv(&spec.argv))
                    .transpose()?
                    .map(|config| {
                        config
                            .parent()
                            .and_then(Path::parent)
                            .map(Path::to_path_buf)
                            .context("resident configuration has no runtime root")
                    })
                    .transpose()?,
                None => None,
            },
        };
        if let Some(root) = root
            && let Err(drain) = self.drain_entry(&root).await
        {
            if drain.downcast_ref::<EntryOwnershipUnconfirmed>().is_some() {
                return Err(drain);
            }
            if let Err(close) = self.close_entry().await {
                let business = self.stop_business().await;
                return Err(supervisor::ShutdownUnconfirmed(format!(
                    "entry drain: {drain:#}; entry close: {close:#}; business cleanup: {business:?}"
                ))
                .into());
            }
            warn!(
                "business session entry could not drain; exact entry shutdown confirmed: {drain:#}"
            );
        }
        self.stop_business().await
    }

    pub(super) async fn finish_verified_generation(
        &self,
        generation_root: &Path,
        generation: &serde_json::Value,
    ) -> Result<()> {
        let Some(info) = self.resident_entry_settled().await? else {
            return self.stop_business().await;
        };
        if info.checked_state(PINGAP_PROGRAM)? != SupervisorProcessState::Running {
            return self.stop_business().await;
        }
        let (config, _) = self.live_entry_binding(&info).await?;
        let runtime_root = config
            .parent()
            .and_then(Path::parent)
            .context("resident configuration has no runtime root")?;
        let domain = runtime_supervisor::domain::PhysicalDomain::from_env()?
            .map(serde_json::to_value)
            .transpose()?;
        anyhow::ensure!(
            generation.get("physical_domain") == domain.as_ref(),
            "cleanup generation belongs to another physical domain"
        );
        let recorded: Option<ResidentEngineReceipt> = read_resident_engine_receipt(runtime_root)?;
        if let Some(receipt) = &recorded {
            let owner_root = generation_root
                .parent()
                .and_then(Path::parent)
                .context("business generation owner missing")?;
            let snapshot = runtime_supervisor::last_snapshot(owner_root)?;
            anyhow::ensure!(
                receipt.identity.binding == serde_json::to_value(&snapshot.binding)?
                    && receipt.identity.physical_domain == domain
                    && generation.get("physical_domain")
                        == receipt.identity.physical_domain.as_ref()
                    && generation
                        .get("process_epoch")
                        .and_then(serde_json::Value::as_str)
                        == receipt.identity.process_epoch.as_deref(),
                "resident and cleanup generation belong to different application workspace, domain or incarnation"
            );
            if let Ok(application) = std::env::var("PROJECT_ID") {
                anyhow::ensure!(
                    application == receipt.identity.application_id,
                    "resident belongs to a different platform application"
                );
            }
        }
        let drain = match recorded {
            Some(_) => crate::proxy::compiler::publish_standby_confirmed_to_path(
                runtime_root,
                &config,
                admin_probe::ensure_admin_endpoint()?,
            )
            .await
            .map(|_| ()),
            None => {
                anyhow::ensure!(
                    domain.is_some() && std::env::var("PROJECT_ID").is_ok(),
                    "legacy entry has no authenticated platform application domain"
                );
                Err(anyhow::anyhow!(
                    "legacy entry has no owner resident-domain receipt; close through its verified generation"
                ))
            }
        };
        if let Err(error) = drain {
            self.close_entry().await.map_err(|close| {
                supervisor::ShutdownUnconfirmed(format!(
                    "generation entry drain: {error:#}; exact entry shutdown unknown: {close:#}"
                ))
            })?;
        }
        self.stop_business().await
    }

    #[cfg(test)]
    pub(crate) async fn shutdown_owner_entry(
        &self,
        runtime_root: &Path,
        scope: &runtime_supervisor::ResidentScope,
    ) -> Result<()> {
        self.shutdown_owner_entry_until(
            runtime_root,
            scope,
            tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        )
        .await
    }

    pub(crate) async fn shutdown_owner_entry_until(
        &self,
        runtime_root: &Path,
        scope: &runtime_supervisor::ResidentScope,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        scope
            .verify_owner_for_cleanup()
            .context("verify retained owner scope before resident shutdown")?;
        crate::xmlrpc::require_deadline_budget("owner shutdown authorization", deadline)?;
        self.settle_entry_writes_until(deadline).await?;
        let configured = configured_entry_spec(&self.conf_path)?;
        if configured.is_none() {
            match self.resident_entry_settled_until(deadline).await? {
                Some(info)
                    if info.checked_state(PINGAP_PROGRAM)? != SupervisorProcessState::Running =>
                {
                    self.reconcile_withdrawn_entry_until(runtime_root, scope, deadline)
                        .await?;
                    return self.stop_business_until(deadline).await;
                }
                None if self.dynamic_groups_until(deadline).await?.is_empty() => {
                    // No local execution or restart source remains. Historical
                    // receipt epochs, missing files or corruption grant no new
                    // authority and must not prevent confirming this empty owner.
                    crate::xmlrpc::require_deadline_budget("empty owner confirmation", deadline)?;
                    return crate::proxy::compiler::clear_current_confirmation();
                }
                _ => {}
            }
        }
        if let Some(spec) = configured.as_ref() {
            let config = entry_config_from_argv(&spec.argv)?;
            match self.resident_entry_settled_until(deadline).await? {
                Some(info)
                    if info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running =>
                {
                    let (live, actual_spec) =
                        self.live_entry_binding_until(&info, deadline).await?;
                    anyhow::ensure!(
                        live == config,
                        "shutdown actual proxy configuration changed"
                    );
                    self.authorize_entry_domain_until(
                        runtime_root,
                        &live,
                        &actual_spec,
                        &info,
                        deadline,
                    )
                    .await?;
                }
                _ => {
                    self.authorize_inactive_entry_with_scope_until(
                        runtime_root,
                        &config,
                        scope,
                        deadline,
                    )
                    .await?
                }
            }
        } else {
            anyhow::ensure!(
                self.resident_entry_settled_until(deadline).await?.is_none(),
                "unconfigured live proxy remains unconfirmed before owner shutdown"
            );
            self.authorize_remaining_business_until(scope, deadline)
                .await?;
        }
        self.close_entry_until(deadline).await?;
        self.stop_business_until(deadline).await
    }

    /// The caller has already crossed a confirmed drain barrier, or has
    /// precisely shut down the resident entry during forced owner cleanup.
    pub(crate) async fn stop_business(&self) -> Result<()> {
        self.stop_business_until(tokio::time::Instant::now() + std::time::Duration::from_secs(60))
            .await
    }

    pub(crate) async fn stop_business_until(&self, deadline: tokio::time::Instant) -> Result<()> {
        // Remove the restart source before stopping the live groups. Keeping the
        // old fragment after removeProcessGroup lets a supervisord restart or a
        // later reload resurrect the retired release. Do not touch fixed groups
        // and the resident fragment (app-pingap).
        let mut failures = Vec::new();
        if let Err(error) = write_conf_until(
            &self.conf_path,
            "# app-cli dynamic services stopped\n",
            deadline,
        )
        .await
        {
            failures.push(format!("withdraw dynamic configuration: {error:#}"));
        }
        if let Err(error) = mutation_result(self.client.reload_config_until(deadline).await) {
            failures.push(format!("reload dynamic configuration: {error:#}"));
        }
        // A broken configuration must not prevent stopping live groups. Keep
        // the withdrawal failure, but attempt the physical stop before returning
        // it; an external cleanup receipt still requires every part to succeed.
        let groups = self.dynamic_groups_until(deadline).await?;
        // Begin shutdown together; one slow service must not multiply the grace
        // period by the number of application modules.
        for (name, result) in futures::future::join_all(groups.iter().map(|name| async move {
            (
                name,
                self.client.stop_remove_group_until(name, deadline).await,
            )
        }))
        .await
        {
            if let Err(error) = result {
                failures.push(format!("{name}: {error:#}"));
            }
        }
        if !failures.is_empty() {
            bail!(
                "dynamic groups did not stop or configuration withdrawal failed: {}",
                failures.join("; ")
            );
        }
        let remaining = self.dynamic_groups_until(deadline).await?;
        if !remaining.is_empty() {
            bail!("dynamic groups remain after stop: {}", remaining.join(", "));
        }
        Ok(())
    }

    /// 当前动态业务组名集合（`app-svc-*`；P1 起 `app-pingap` 为常驻组，
    /// 不属业务停止/清理范围——入口生命周期独立于编排代次）。
    pub(super) async fn dynamic_groups(&self) -> Result<Vec<String>> {
        self.dynamic_groups_until(tokio::time::Instant::now() + std::time::Duration::from_secs(60))
            .await
    }

    pub(super) async fn dynamic_groups_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<String>> {
        Ok(self
            .managed_groups_until(deadline)
            .await?
            .into_iter()
            .filter(|group| group != PINGAP_PROGRAM)
            .collect())
    }

    /// app-cli 托管的全部组（业务 `app-svc-*` + 常驻 `app-pingap`）——
    /// 编排就绪检查用（判定常驻组是否已被 supervisord 托管）。
    pub(super) async fn managed_groups_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<String>> {
        let infos = self.client.get_all_process_info_until(deadline).await?;
        let mut groups = std::collections::BTreeSet::new();
        for info in infos {
            if info.group.starts_with(SVC_PROGRAM_PREFIX) || info.group == PINGAP_PROGRAM {
                groups.insert(info.group);
            }
        }
        Ok(groups.into_iter().collect())
    }
}

fn cleanup_observer_conf_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("APP_CLI_TEST_CLEANUP_CONF") {
        return path.into();
    }
    CONF_PATH.into()
}

/// The callback already owns the draining generation guard and has confirmed
/// its commands/guardians quiescent. This observation grants no engine ownership
/// and performs no stop, reload, configuration write or receipt registration.
pub(super) async fn observe_unregistered_engine_empty_until(
    socket: &Path,
    conf_path: &Path,
    deadline: tokio::time::Instant,
) -> Result<()> {
    crate::proxy::compiler::wait_for_file_replacements_until(deadline).await?;
    let client = SupervisorClient::new(socket);
    client.wait_mutations_until(deadline).await?;
    let before = empty_dynamic_fragments(conf_path)?;
    crate::xmlrpc::require_deadline_budget("unregistered engine observation", deadline)?;
    let original_socket = observed_supervisor_socket(socket)?;
    if original_socket.is_some() {
        client
            .ping_until(deadline)
            .await
            .context("unregistered supervisord RPC unavailable")?;
        ensure_no_managed_programs(&client, deadline).await?;
    }
    crate::proxy::compiler::wait_for_file_replacements_until(deadline).await?;
    client.wait_mutations_until(deadline).await?;
    anyhow::ensure!(
        empty_dynamic_fragments(conf_path)? == before,
        "unregistered supervisor configuration changed during cleanup observation"
    );
    anyhow::ensure!(
        observed_supervisor_socket(socket)? == original_socket,
        "supervisord socket identity changed during empty generation observation"
    );
    if original_socket.is_some() {
        ensure_no_managed_programs(&client, deadline).await?;
    }
    anyhow::ensure!(
        observed_supervisor_socket(socket)? == original_socket,
        "supervisord socket identity changed before cleanup confirmation"
    );
    crate::xmlrpc::require_deadline_budget("unregistered engine cleanup confirmation", deadline)
}

#[derive(Debug, PartialEq, Eq)]
struct SupervisorSocketIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn observed_supervisor_socket(socket: &Path) -> Result<Option<SupervisorSocketIdentity>> {
    let metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("observe unregistered supervisor socket"),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        anyhow::ensure!(
            metadata.file_type().is_socket(),
            "unregistered supervisor endpoint is not a Unix socket"
        );
        Ok(Some(SupervisorSocketIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        anyhow::bail!("supervisord Unix socket observation is unsupported on this platform")
    }
}

fn empty_dynamic_fragments(conf_path: &Path) -> Result<Vec<Option<Vec<u8>>>> {
    let resident = conf_path
        .parent()
        .context("supervisor fragment has no parent")?
        .join(RESIDENT_CONF_FILE);
    [conf_path, &resident]
        .into_iter()
        .map(|path| {
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("read unregistered supervisor fragment {}", path.display()));
                }
            };
            let content = std::str::from_utf8(&bytes)
                .with_context(|| format!("decode unregistered supervisor fragment {}", path.display()))?;
            if let Some((index, _)) = content.lines().enumerate().find(|(_, line)| {
                let line = line.trim();
                !line.is_empty() && !line.starts_with('#') && !line.starts_with(';')
            }) {
                anyhow::bail!(
                    "supervisor fragment {} contains executable or unclassified configuration without engine ownership (line {})",
                    path.display(),
                    index + 1
                );
            }
            Ok(Some(bytes))
        })
        .collect()
}

async fn ensure_no_managed_programs(
    client: &SupervisorClient,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let managed = |name: &str| name == PINGAP_PROGRAM || name.starts_with(SVC_PROGRAM_PREFIX);
    for info in client
        .get_all_process_info_until(deadline)
        .await
        .context("observe unregistered supervisord programs")?
    {
        anyhow::ensure!(
            !info.group.is_empty(),
            "supervisord returned a process without group identity"
        );
        if managed(&info.group) || info.name.as_deref().is_some_and(managed) {
            anyhow::bail!(
                "supervisord managed program {} exists without this generation's engine ownership receipt",
                info.name.as_deref().unwrap_or(&info.group)
            );
        }
    }
    Ok(())
}

impl SupervisordHost {
    async fn authorize_remaining_business_until(
        &self,
        scope: &runtime_supervisor::ResidentScope,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.authorize_current_entry_domain_until(scope, deadline)
            .await?;
        let workspace: PathBuf = serde_json::from_value(
            scope
                .identity()
                .binding
                .get("resource")
                .cloned()
                .context("current business owner workspace missing")?,
        )?;
        let workspace = std::fs::canonicalize(workspace)?;
        let owner_root = scope
            .root()
            .parent()
            .and_then(Path::parent)
            .context("current business owner root missing")?;
        let private = runtime_state_layout::ManagedWorkspace::from_env(&workspace, owner_root)
            .ok()
            .flatten()
            .is_some_and(|managed| managed.application_id == scope.identity().application_id);
        let conf =
            match tokio::time::timeout_at(deadline, tokio::fs::read_to_string(&self.conf_path))
                .await
                .context("current business configuration observation deadline exceeded")?
            {
                Ok(conf) => conf,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error).context("observe current business configuration"),
            };
        for info in self.client.get_all_process_info_until(deadline).await? {
            if !info.group.starts_with(SVC_PROGRAM_PREFIX) {
                continue;
            }
            let state = info.checked_state(&info.group)?;
            if matches!(
                state,
                SupervisorProcessState::Stopped
                    | SupervisorProcessState::Exited
                    | SupervisorProcessState::Fatal
            ) {
                anyhow::ensure!(
                    info.pid == Some(0),
                    "terminal business PID remains unconfirmed"
                );
            }
            if private {
                continue;
            }
            let configured = match current_business_workspace(&conf, &info.group) {
                Ok(configured) => configured,
                Err(_) => {
                    tracing::warn!(group = %info.group,
                        "current launch proof unavailable; observing the actual business workspace");
                    None
                }
            };
            if let Some(cwd) = configured.as_ref() {
                anyhow::ensure!(
                    std::fs::canonicalize(cwd)?.starts_with(&workspace),
                    "current configured business belongs to another workspace"
                );
            }
            #[cfg(target_os = "linux")]
            if state == SupervisorProcessState::Running {
                let running = info.running_identity()?;
                let cwd = std::fs::canonicalize(format!("/proc/{}/cwd", running.pid))
                    .context("observe actual shared-engine business workspace")?;
                anyhow::ensure!(
                    cwd.starts_with(&workspace),
                    "actual running business belongs to another workspace"
                );
                anyhow::ensure!(
                    self.client
                        .running_process_until(&info.group, deadline)
                        .await?
                        == running,
                    "shared-engine business incarnation changed during workspace observation"
                );
                continue;
            }
            anyhow::ensure!(
                configured.is_some(),
                "shared-engine business has no current workspace ownership evidence"
            );
        }
        scope.verify_owner_for_cleanup()?;
        crate::xmlrpc::require_deadline_budget("current business shutdown authorization", deadline)
    }
}

fn current_business_workspace(conf: &str, group: &str) -> Result<Option<PathBuf>> {
    let mut current = false;
    let service = group
        .strip_prefix(SVC_PROGRAM_PREFIX)
        .context("business service identity missing")?;
    for line in conf.lines().map(str::trim) {
        if line.starts_with('[') {
            current = line == format!("[program:{group}]");
        }
        if current && let Some(command) = line.strip_prefix("command=") {
            let fields: Vec<_> = command.split_whitespace().collect();
            anyhow::ensure!(
                fields.len() == 4 && fields[1] == "run-service" && fields[3] == service,
                "current business wrapper identity is unconfirmed"
            );
            let mut parts = Path::new(fields[2]).components();
            anyhow::ensure!(
                matches!(parts.next(), Some(std::path::Component::Normal(_)))
                    && parts.next().is_none(),
                "current business release identity is unconfirmed"
            );
            return Ok(Some(PathBuf::from(
                ServiceSpecFile::load(fields[2], service)?.cwd,
            )));
        }
    }
    Ok(None)
}
