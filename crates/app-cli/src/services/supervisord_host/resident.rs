//! Official Pingap replacement with exact supervisord owner provenance.
pub(super) use super::official_host::OfficialSupervisorHost;
use super::*;
use crate::proxy::{
    apply_status::ConfirmedPublication,
    publication::{self, ProcessHost, ProcessInstance},
};

impl SupervisordHost {
    pub(crate) async fn verify_entry_confirmation(
        &self,
        confirmation: &ConfirmedPublication,
    ) -> Result<()> {
        self.verify_entry_confirmation_until(
            confirmation,
            tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET,
        )
        .await
    }

    async fn verify_entry_confirmation_until(
        &self,
        confirmation: &ConfirmedPublication,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let info = self
            .client
            .process_info_until(PINGAP_PROGRAM, deadline)
            .await?;
        anyhow::ensure!(
            info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running,
            "proxy confirmation has no RUNNING supervisord instance"
        );
        anyhow::ensure!(
            info.running_identity()?.pid == i64::from(confirmation.process_id),
            "proxy confirmation belongs to another supervisord PID"
        );
        let (config, _) = self.live_entry_binding_until(&info, deadline).await?;
        let (outcome, held) = crate::proxy::compiler::confirmed_publication()
            .context("proxy has no local official confirmation")?;
        anyhow::ensure!(
            held == *confirmation && config == outcome.config_path,
            "proxy confirmation configuration or host instance changed"
        );
        let scope =
            supervisor::resident::owner_scope()?.context("proxy has no retained owner domain")?;
        scope.verify_owner_for_cleanup()?;
        let root = config
            .parent()
            .and_then(Path::parent)
            .context("proxy snapshot runtime root missing")?;
        let receipt = verify_resident_engine_receipt(
            root,
            scope.identity(),
            &crate::xmlrpc::default_socket_path(),
        )?
        .context("proxy owner receipt missing")?;
        anyhow::ensure!(
            receipt.config == config && receipt.process.as_ref() == Some(&info.running_identity()?),
            "proxy exact owner receipt changed"
        );
        Ok(())
    }

    /// Supervisord may restart only this immutable spec after a crash. Such a
    /// new PID is not Ready until the retained owner proves it again over HTTP.
    pub(crate) async fn reconcile_official_incarnation(
        &self,
        runtime_root: &Path,
        expected: &CompileOutcome,
        held: &ConfirmedPublication,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let epoch = supervisor::resident::control_epoch()?;
        let info = self.resident_entry_settled_until(deadline).await?;
        let Some(info) = info else {
            return self
                .replace_official(runtime_root, expected, deadline)
                .await
                .map(|_| ());
        };
        if info.checked_state(PINGAP_PROGRAM)? != SupervisorProcessState::Running {
            return self
                .replace_official(runtime_root, expected, deadline)
                .await
                .map(|_| ());
        }
        if info.running_identity()?.pid == i64::from(held.process_id) {
            // Equal PID alone cannot retain a prior incarnation after PID reuse.
            // Check start time, exact wrapper and owner receipt without mutation.
            match tokio::time::timeout_at(
                deadline,
                self.verify_entry_confirmation_until(held, deadline),
            )
            .await
            {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(_changed)) => {}
                Err(_) => anyhow::bail!("supervisord physical observation deadline exhausted"),
            }
        }
        let _guard = match crate::proxy::compiler::try_publication_guard() {
            Ok(guard) => guard,
            Err(_) => return Ok(()),
        };
        let (config, spec) = self.live_entry_binding_until(&info, deadline).await?;
        anyhow::ensure!(
            config == expected.config_path,
            "supervisord restarted a different proxy snapshot"
        );
        self.authorize_entry_domain_until(runtime_root, &config, &spec, &info, deadline)
            .await?;
        publication::validate_immutable_until(expected, deadline).await?;
        let running = info.running_identity()?;
        let instance = ProcessInstance {
            pid: u32::try_from(running.pid)?,
            id: uuid::Uuid::new_v4().to_string(),
        };
        let mut host = OfficialSupervisorHost {
            host: self,
            runtime_root,
            spec,
            current: Some((instance.clone(), running)),
            cancel: None,
            epoch,
        };
        crate::proxy::compiler::record_official_phase(
            expected,
            shared_types::ProxyPhase::Verifying,
            Some(&instance.id),
        )?;
        host.verify_exact(&instance, deadline).await?;
        let confirmation = host.confirm(expected, &instance, deadline).await?;
        host.verify_exact(&instance, deadline).await?;
        host.commit(expected, &confirmation, deadline).await
    }

    pub(crate) async fn drain_entry(&self, runtime_root: &Path) -> Result<()> {
        self.drain_entry_until(
            runtime_root,
            tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET,
        )
        .await
    }

    pub(crate) async fn drain_entry_until(
        &self,
        runtime_root: &Path,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.drain_entry_for_operation_until(runtime_root, deadline, None)
            .await
    }

    pub(crate) async fn drain_entry_for_operation_until(
        &self,
        runtime_root: &Path,
        deadline: tokio::time::Instant,
        operation_id: Option<String>,
    ) -> Result<()> {
        self.drain_entry_inner(runtime_root, deadline, operation_id)
            .await
    }

    pub(super) async fn drain_entry_inner(
        &self,
        runtime_root: &Path,
        deadline: tokio::time::Instant,
        operation_id: Option<String>,
    ) -> Result<()> {
        // Lifecycle control has already been admitted. Background reconfirmation
        // or readonly observation must settle rather than reject Stop as Busy.
        let _control = supervisor::resident::control_publication(deadline).await?;
        self.settle_entry_writes_until(deadline).await?;
        let Some(info) = self.resident_entry_settled_until(deadline).await? else {
            if configured_entry_spec(&self.conf_path)?.is_none() {
                return Ok(());
            }
            let spec = configured_entry_spec(&self.conf_path)?
                .context("configured proxy spec disappeared")?;
            return self
                .migrate_legacy_entry(
                    runtime_root,
                    &entry_config_from_argv(&spec.argv)?,
                    &spec,
                    deadline,
                    operation_id,
                )
                .await;
        };
        let (config, spec) =
            if info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running {
                let (config, spec) = self.live_entry_binding_until(&info, deadline).await?;
                self.authorize_entry_domain_until(runtime_root, &config, &spec, &info, deadline)
                    .await?;
                (config, spec)
            } else {
                let Some(spec) = configured_entry_spec(&self.conf_path)? else {
                    let scope = supervisor::resident::owner_scope()?
                        .context("withdrawn proxy has no retained owner capability")?;
                    return self
                        .reconcile_withdrawn_entry_until(runtime_root, &scope, deadline)
                        .await;
                };
                let config = entry_config_from_argv(&spec.argv)?;
                // Authority is checked before deriving or validating a candidate.
                // Missing binaries/configuration cannot mask a foreign receipt.
                self.authorize_inactive_entry_until(runtime_root, &config, deadline)
                    .await?;
                (config, spec)
            };
        let mut outcome = publication::standby_candidate(runtime_root, &config).await?;
        outcome.operation_id = operation_id;
        let bytes = publication::validate_immutable_until(&outcome, deadline).await?;
        let graph = pingap_config::PingapConfig::new(&bytes, true)?;
        crate::proxy::compiler::validate_candidate_until(
            Path::new(&spec.argv[0]),
            &graph,
            runtime_root,
            deadline,
        )
        .await?;
        self.replace_official_under_guard(runtime_root, &outcome, deadline, None)
            .await
            .map(|_| ())
    }

    /// The active file is only a compatibility topology view; its presence and
    /// historical Applied records never authorize a new process incarnation.
    pub(crate) async fn prepare_entry_baseline(&self, runtime_root: &Path) -> Result<()> {
        let Some(info) = self.resident_entry_settled().await? else {
            return Ok(());
        };
        if info.checked_state(PINGAP_PROGRAM)? != SupervisorProcessState::Running {
            return Ok(());
        }
        let (config, spec) = self.live_entry_binding(&info).await?;
        self.authorize_entry_domain(runtime_root, &config, &spec, &info)
            .await?;
        if config != crate::proxy::compiler::active_config_path(runtime_root) {
            crate::proxy::compiler::publish_to_path(
                &config,
                &crate::proxy::compiler::active_config_path(runtime_root),
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn migrate_legacy_entry(
        &self,
        runtime_root: &Path,
        old_config: &Path,
        old_spec: &ServiceSpecFile,
        deadline: tokio::time::Instant,
        operation_id: Option<String>,
    ) -> Result<()> {
        self.authorize_inactive_entry_until(runtime_root, old_config, deadline)
            .await?;
        admin_probe::register_verified_legacy_endpoint(old_spec)?;
        let mut outcome = publication::standby_candidate(runtime_root, old_config).await?;
        outcome.operation_id = operation_id;
        let graph = pingap_config::PingapConfig::new(
            &publication::validate_immutable_until(&outcome, deadline).await?,
            true,
        )?;
        crate::proxy::compiler::validate_candidate_until(
            Path::new(&old_spec.argv[0]),
            &graph,
            runtime_root,
            deadline,
        )
        .await?;
        self.replace_official_under_guard(runtime_root, &outcome, deadline, None)
            .await
            .map(|_| ())
    }

    pub(super) async fn authorize_inactive_entry_until(
        &self,
        runtime_root: &Path,
        config: &Path,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let scope = supervisor::resident::owner_scope()?
            .context("inactive proxy has no retained owner capability")?;
        self.authorize_inactive_entry_with_scope_until(runtime_root, config, &scope, deadline)
            .await
    }

    pub(super) async fn authorize_inactive_entry_with_scope_until(
        &self,
        runtime_root: &Path,
        config: &Path,
        scope: &runtime_supervisor::ResidentScope,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let result = async {
            self.settle_entry_writes_until(deadline).await?;
            scope.verify_owner_for_cleanup()?;
            validate_entry_configuration_domain(runtime_root, config)?;
            if let Some(info) = self.resident_entry_settled_until(deadline).await? {
                anyhow::ensure!(
                    matches!(
                        info.checked_state(PINGAP_PROGRAM)?,
                        SupervisorProcessState::Stopped
                            | SupervisorProcessState::Exited
                            | SupervisorProcessState::Fatal
                    ) && info.pid == Some(0),
                    "inactive proxy execution is not confirmed stopped"
                );
            }
            let spec = configured_entry_spec(&self.conf_path)?
                .context("inactive proxy launch configuration disappeared")?;
            anyhow::ensure!(
                entry_config_from_argv(&spec.argv)? == config,
                "inactive proxy actual configured path changed"
            );
            match verify_resident_engine_receipt(
                runtime_root,
                scope.identity(),
                &crate::xmlrpc::default_socket_path(),
            ) {
                Ok(Some(receipt)) if receipt.config == config => Ok(()),
                _ => {
                    self.authorize_current_entry_domain_until(&scope, deadline)
                        .await?;
                    self.preserve_resident_history_until(runtime_root, deadline)
                        .await
                }
            }
        }
        .await;
        result.map_err(|error| {
            anyhow::Error::new(EntryOwnershipUnconfirmed(format!(
                "inactive proxy current domain rejected: {error:#}"
            )))
            .context(supervisor::ShutdownUnconfirmed(
                "inactive proxy ownership is unconfirmed; business retained".into(),
            ))
        })
    }

    /// Only withdraw proxy restart sources. Business groups and their fragment
    /// remain intact during a configuration reload or failed proxy replacement.
    pub(super) async fn close_entry(&self) -> Result<()> {
        self.close_entry_until(tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET)
            .await
    }

    pub(super) async fn close_entry_until(&self, deadline: tokio::time::Instant) -> Result<()> {
        self.settle_entry_writes_until(deadline).await?;
        let existing = self.resident_entry_settled_until(deadline).await?;
        if let Some(info) = &existing {
            if info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running {
                self.live_entry_binding_until(info, deadline).await?;
            } else {
                let Some(spec) = configured_entry_spec(&self.conf_path)? else {
                    let scope = supervisor::resident::owner_scope()?
                        .context("withdrawn proxy has no retained owner capability")?;
                    let runtime_root = supervisor::resident::owner_runtime_root()?
                        .context("withdrawn proxy has no retained runtime root")?;
                    return self
                        .reconcile_withdrawn_entry_until(&runtime_root, &scope, deadline)
                        .await;
                };
                admin_probe::register_verified_legacy_endpoint(&spec)?;
            }
        }
        let content =
            match tokio::time::timeout_at(deadline, tokio::fs::read_to_string(&self.conf_path))
                .await
                .context("supervisor fragment read deadline exceeded")?
            {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => {
                    return Err(error).context("read business fragment before proxy withdrawal");
                }
            };
        crate::xmlrpc::require_deadline_budget("proxy config withdrawal", deadline)?;
        write_conf_until(
            &self.conf_path,
            &without_resident_program(&content),
            deadline,
        )
        .await?;
        write_conf_until(
            &self.resident_conf_path()?,
            "# official proxy stopped\n",
            deadline,
        )
        .await?;
        mutation_result(self.client.reload_config_until(deadline).await)?;
        if existing.is_some() {
            mutation_result(
                self.client
                    .stop_remove_group_until(PINGAP_PROGRAM, deadline)
                    .await,
            )?;
        }
        anyhow::ensure!(
            self.resident_entry_settled_until(deadline).await?.is_none(),
            "proxy group remains after exact shutdown"
        );
        crate::xmlrpc::require_deadline_budget("proxy cleanup confirmation", deadline)?;
        crate::proxy::compiler::clear_current_confirmation()?;
        Ok(())
    }

    pub(crate) async fn replace_official(
        &self,
        runtime_root: &Path,
        outcome: &CompileOutcome,
        deadline: tokio::time::Instant,
    ) -> Result<ConfirmedPublication> {
        self.replace_official_with_cancel(runtime_root, outcome, deadline, None)
            .await
    }

    pub(crate) async fn replace_official_with_cancel(
        &self,
        runtime_root: &Path,
        outcome: &CompileOutcome,
        deadline: tokio::time::Instant,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<ConfirmedPublication> {
        let _guard = crate::proxy::compiler::try_publication_guard()?;
        self.replace_official_under_guard(runtime_root, outcome, deadline, cancel)
            .await
    }

    async fn replace_official_under_guard(
        &self,
        runtime_root: &Path,
        outcome: &CompileOutcome,
        deadline: tokio::time::Instant,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<ConfirmedPublication> {
        let epoch = supervisor::resident::control_epoch()?;
        let candidate = tokio::time::timeout_at(
            deadline,
            publication::freeze_candidate(runtime_root, outcome),
        )
        .await
        .context("proxy snapshot preparation deadline exceeded")??;
        let configured = configured_entry_spec(&self.conf_path)?;
        let was_configured = configured.is_some();
        let original_spec = match configured {
            Some(spec) => spec,
            None => {
                anyhow::ensure!(
                    self.resident_entry_settled_until(deadline).await?.is_none(),
                    "proxy process exists without an owned launch specification"
                );
                let scope = supervisor::resident::owner_scope()?
                    .context("proxy bootstrap has no owner capability")?;
                scope.record_running_until(deadline).await?;
                let spec = ServiceSpecFile::load(crate::svc_spec::RESIDENT_SPEC_ID, "pingap")?;
                anyhow::ensure!(
                    entry_config_from_argv(&spec.argv)?.starts_with(runtime_root),
                    "proxy bootstrap specification belongs to another runtime root"
                );
                spec
            }
        };
        match self.resident_entry_settled_until(deadline).await? {
            Some(info)
                if info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running =>
            {
                let (config, spec) = self.live_entry_binding_until(&info, deadline).await?;
                self.authorize_entry_domain_until(runtime_root, &config, &spec, &info, deadline)
                    .await?;
            }
            _ if was_configured => {
                self.authorize_inactive_entry_until(
                    runtime_root,
                    &entry_config_from_argv(&original_spec.argv)?,
                    deadline,
                )
                .await?
            }
            _ => {}
        }
        let previous = crate::proxy::compiler::confirmed_publication()
            .filter(|(outcome, _)| outcome.config_path.starts_with(runtime_root))
            .map(|(outcome, _)| outcome);
        let mut host = OfficialSupervisorHost {
            host: self,
            runtime_root,
            spec: original_spec,
            current: None,
            cancel,
            epoch,
        };
        publication::restart_verified(&mut host, &candidate, previous.as_ref(), deadline).await
    }

    /// Bind the observed PID/start pair to its exact wrapper spec and -c path.
    /// Credentials alone cannot authorize adopting a namesake business file.
    pub(super) async fn live_entry_binding(
        &self,
        info: &SupervisordProcessInfo,
    ) -> Result<(PathBuf, ServiceSpecFile)> {
        self.live_entry_binding_until(
            info,
            tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET,
        )
        .await
    }

    pub(super) async fn live_entry_binding_until(
        &self,
        info: &SupervisordProcessInfo,
        deadline: tokio::time::Instant,
    ) -> Result<(PathBuf, ServiceSpecFile)> {
        crate::xmlrpc::require_deadline_budget("resident command identity", deadline)?;
        let instance = info.running_identity()?;
        #[cfg(test)]
        if let Some(observe) = &self.fixture_argv {
            let argv = observe(instance.pid)?;
            let config = entry_config_from_argv(&argv)?;
            let spec = matching_entry_spec(&self.conf_path, &argv, &config)?
                .context("controlled process argv has no exact wrapper binding")?;
            anyhow::ensure!(
                self.client
                    .running_process_until(PINGAP_PROGRAM, deadline)
                    .await?
                    == instance,
                "controlled resident identity changed during argv observation"
            );
            admin_probe::register_verified_legacy_endpoint(&spec)?;
            return Ok((config, spec));
        }
        #[cfg(target_os = "linux")]
        {
            let bytes = tokio::time::timeout_at(
                deadline,
                tokio::fs::read(format!("/proc/{}/cmdline", instance.pid)),
            )
            .await
            .context("resident command observation deadline exceeded")?
            .context("read resident command identity")?;
            let argv: Vec<String> = bytes
                .split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8(arg.to_vec()).context("decode resident argv"))
                .collect::<Result<_>>()?;
            let config = entry_config_from_argv(&argv)?;
            let spec = matching_entry_spec(&self.conf_path, &argv, &config)?
                .context("resident command has no exact owned service spec binding")?;
            anyhow::ensure!(
                self.client
                    .running_process_until(PINGAP_PROGRAM, deadline)
                    .await?
                    == instance,
                "resident program changed while binding its command"
            );
            admin_probe::register_verified_legacy_endpoint(&spec)?;
            return Ok((config, spec));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = instance;
            anyhow::bail!("supervisord resident command identity requires Linux /proc evidence");
        }
    }

    pub(super) async fn authorize_entry_domain(
        &self,
        runtime_root: &Path,
        config: &Path,
        spec: &ServiceSpecFile,
        info: &SupervisordProcessInfo,
    ) -> Result<()> {
        self.authorize_entry_domain_until(
            runtime_root,
            config,
            spec,
            info,
            tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET,
        )
        .await
    }

    pub(super) async fn authorize_entry_domain_until(
        &self,
        runtime_root: &Path,
        config: &Path,
        spec: &ServiceSpecFile,
        info: &SupervisordProcessInfo,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let scope = supervisor::resident::owner_scope()?
            .context("supervisord adoption requires a retained owner capability")?;
        scope.record_running_until(deadline).await?;
        let identity = scope.identity();
        let root =
            std::fs::canonicalize(runtime_root).context("resolve resident runtime domain")?;
        anyhow::ensure!(
            config.is_absolute()
                && config.starts_with(&root)
                && !config
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir)),
            "resident configuration belongs to another runtime root"
        );
        match std::fs::canonicalize(config) {
            Ok(resolved) => anyhow::ensure!(
                resolved.starts_with(&root),
                "resident configuration resolves outside the owner range"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("resolve resident configuration"),
        }
        self.settle_entry_writes_until(deadline).await?;
        if let Ok(Some(receipt)) = verify_resident_engine_receipt(
            runtime_root,
            identity,
            &crate::xmlrpc::default_socket_path(),
        ) {
            if receipt.config == config {
                return Ok(());
            }
        }
        self.authorize_live_supervisor_owner_until(&scope, deadline)
            .await?;
        let (observed_config, observed_spec) =
            self.live_entry_binding_until(info, deadline).await?;
        anyhow::ensure!(
            observed_config == config
                && observed_spec.argv == spec.argv
                && observed_spec.release_id == spec.release_id,
            "live proxy actual command or launch specification changed"
        );
        let resource: PathBuf = serde_json::from_value(
            identity
                .binding
                .get("resource")
                .cloned()
                .context("owner workspace binding missing")?,
        )?;
        let resource = std::fs::canonicalize(resource)?;
        let mut workspace_proved = spec.release_id == crate::svc_spec::RESIDENT_SPEC_ID
            && supervisor::resident::owner_runtime_root()?
                .is_some_and(|held| std::fs::canonicalize(held).ok().as_ref() == Some(&root));
        let directory = crate::svc_spec::spec_root().join(&spec.release_id);
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|name| name == "pingap.toml") {
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "toml") {
                continue;
            }
            let service = path
                .file_stem()
                .and_then(|name| name.to_str())
                .context("invalid legacy service specification name")?;
            let business = ServiceSpecFile::load(&spec.release_id, service)?;
            let cwd = std::fs::canonicalize(&business.cwd)
                .context("resolve legacy business workspace")?;
            anyhow::ensure!(
                cwd.starts_with(&resource),
                "legacy business belongs to a different owner workspace"
            );
            workspace_proved = true;
        }
        if !workspace_proved {
            for workspace in [&resource, &resource.join(".run")] {
                if let Ok(release) = crate::manifest::read_release_lock(workspace)
                    && release.release_id == spec.release_id
                {
                    workspace_proved = true;
                }
            }
        }
        anyhow::ensure!(
            workspace_proved,
            "legacy proxy has no application workspace provenance"
        );
        #[cfg(target_os = "linux")]
        anyhow::ensure!(
            std::fs::read_link(format!("/proc/{}/ns/pid", info.running_identity()?.pid))?
                == std::fs::read_link("/proc/self/ns/pid")?,
            "legacy proxy belongs to another process namespace"
        );
        self.preserve_resident_history_until(runtime_root, deadline)
            .await?;
        anyhow::ensure!(
            self.client
                .running_process_until(PINGAP_PROGRAM, deadline)
                .await?
                == info.running_identity()?,
            "live proxy incarnation changed before controlled retirement"
        );
        // Do not issue a new live claim or serving confirmation from old history.
        // The original drain/replace caller must close this exact instance first.
        Ok(())
    }

    pub(super) async fn record_entry_domain_until(
        &self,
        runtime_root: &Path,
        config: &Path,
        process: Option<&crate::xmlrpc::RunningProcess>,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let scope = supervisor::resident::owner_scope()?
            .context("resident engine requires an owner scope")?;
        scope.record_running_until(deadline).await?;
        self.settle_entry_writes_until(deadline).await?;
        validate_entry_configuration_domain(runtime_root, config)?;
        if process.is_none() {
            if let Some(info) = self.resident_entry_settled_until(deadline).await? {
                anyhow::ensure!(
                    matches!(
                        info.checked_state(PINGAP_PROGRAM)?,
                        SupervisorProcessState::Stopped
                            | SupervisorProcessState::Exited
                            | SupervisorProcessState::Fatal
                    ) && info.pid == Some(0),
                    "proxy bootstrap cannot rewrite a live or unconfirmed execution claim"
                );
            }
        }
        if let Err(history) = verify_resident_engine_receipt(
            runtime_root,
            scope.identity(),
            &crate::xmlrpc::default_socket_path(),
        ) {
            let existing = self.resident_entry_settled_until(deadline).await?;
            let inactive = match existing.as_ref() {
                None => true,
                Some(info) => {
                    matches!(
                        info.checked_state(PINGAP_PROGRAM)?,
                        SupervisorProcessState::Stopped
                            | SupervisorProcessState::Exited
                            | SupervisorProcessState::Fatal
                    ) && info.pid == Some(0)
                }
            };
            anyhow::ensure!(
                process.is_none() && inactive,
                "resident ownership cannot replace an executing or unconfirmed proxy claim: {history:#}"
            );
            // A valid same-application historical domain remains sufficient for
            // the existing bootstrap path. Otherwise use current live authority,
            // not a historical PID or a parseable old metadata prerequisite.
            if verify_withdrawn_entry_receipt(
                runtime_root,
                scope.identity(),
                &crate::xmlrpc::default_socket_path(),
            )
            .is_err()
            {
                self.authorize_current_entry_domain_until(&scope, deadline).await
                    .with_context(|| format!("current resident bootstrap domain is unconfirmed; historical receipt: {history:#}"))?;
            }
            self.preserve_resident_history_until(runtime_root, deadline)
                .await?;
        }
        let receipt = ResidentEngineReceipt {
            version: 1,
            identity: scope.identity().clone(),
            runtime_root: std::fs::canonicalize(runtime_root)?,
            socket: crate::xmlrpc::default_socket_path(),
            config: config.to_path_buf(),
            process: process.cloned(),
        };
        let bytes = serde_json::to_vec(&receipt)?;
        let claims = runtime_root.join("resident-supervisord-claims");
        tokio::time::timeout_at(deadline, tokio::fs::create_dir_all(&claims))
            .await
            .context("resident receipt directory deadline exceeded")??;
        let evidence = claims.join(format!("{}.json", uuid::Uuid::new_v4()));
        tracing::debug!(
            target_path = %evidence.display(),
            persistence_stage = "resident_evidence_write",
            remaining_budget_ms = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_millis() as u64,
            "resident engine persistence admitted"
        );
        tokio::time::timeout_at(
            deadline,
            crate::proxy::compiler::replace_file_bytes(&evidence, bytes.clone()),
        )
        .await
        .map_err(|_| {
            supervisor::ShutdownUnconfirmed(
                "resident evidence write remains unsettled at deadline".into(),
            )
        })??;
        let receipt_path = runtime_root.join(RESIDENT_ENGINE_RECEIPT);
        tracing::debug!(
            target_path = %receipt_path.display(),
            persistence_stage = "resident_receipt_write",
            remaining_budget_ms = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_millis() as u64,
            "resident engine persistence admitted"
        );
        tokio::time::timeout_at(
            deadline,
            crate::proxy::compiler::replace_file_bytes(&receipt_path, bytes),
        )
        .await
        .map_err(|_| {
            supervisor::ShutdownUnconfirmed(
                "resident receipt write remains unsettled at deadline".into(),
            )
        })?
    }

    /// 常驻入口是否**实际运行**（C2：组已配置 ≠ 在运行——autostart=false
    /// 下 supervisord 重启/STOPPED/FATAL 的组仍在进程表但 state 非 RUNNING，
    /// 不得据此跳过 startProcess）。证据 = 该组的 statename 字段。
    pub(super) async fn resident_entry_running(&self) -> Result<bool> {
        Ok(self
            .resident_entry_settled()
            .await?
            .is_some_and(|info| info.statename == Some(SupervisorProcessState::Running)))
    }

    pub(super) async fn resident_entry_settled(&self) -> Result<Option<SupervisordProcessInfo>> {
        self.resident_entry_settled_with_budget(admin_probe::CONFIRM_BUDGET)
            .await
    }

    pub(super) async fn resident_entry_settled_with_budget(
        &self,
        budget: std::time::Duration,
    ) -> Result<Option<SupervisordProcessInfo>> {
        self.resident_entry_settled_until(tokio::time::Instant::now() + budget)
            .await
    }

    pub(super) async fn resident_entry_settled_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Option<SupervisordProcessInfo>> {
        tokio::time::timeout_at(deadline, async {
            loop {
                let mut entries = self
                    .client
                    .get_all_process_info_until(deadline)
                    .await?
                    .into_iter()
                    .filter(|info| info.group == PINGAP_PROGRAM);
                let Some(info) = entries.next() else {
                    return Ok(None);
                };
                anyhow::ensure!(
                    entries.next().is_none(),
                    "multiple resident program identities observed"
                );
                let state = info.checked_state(PINGAP_PROGRAM)?;
                if !state.is_transitional() {
                    return Ok(Some(info));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .context("resident process did not settle before observation deadline")?
    }
}

fn validate_entry_configuration_domain(runtime_root: &Path, config: &Path) -> Result<()> {
    let root = std::fs::canonicalize(runtime_root)?;
    anyhow::ensure!(
        config.is_absolute()
            && config.starts_with(&root)
            && !config
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "proxy actual configuration belongs to another runtime root"
    );
    match std::fs::canonicalize(config) {
        Ok(actual) => anyhow::ensure!(
            actual.starts_with(root),
            "proxy actual configuration resolves outside the owner range"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("resolve actual proxy configuration"),
    }
    Ok(())
}
