//! Resident entry observation, ownership, migration and publication barriers.
use super::*;

impl SupervisordHost {
    pub(crate) async fn verify_entry_confirmation(
        &self,
        confirmation: &crate::proxy::apply_status::ConfirmedPublication,
    ) -> Result<()> {
        let info = self.client.process_info(PINGAP_PROGRAM).await?;
        anyhow::ensure!(
            info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running,
            "proxy application result has no RUNNING supervisord instance"
        );
        anyhow::ensure!(
            info.running_identity()?.pid == i64::from(confirmation.process_id),
            "proxy application result belongs to another supervisord PID"
        );
        let (config, _) = self.live_entry_binding(&info).await?;
        anyhow::ensure!(
            config
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "active"),
            "proxy confirmation belongs to a legacy configuration path"
        );
        let scope = supervisor::resident::owner_scope()?
            .context("proxy confirmation has no retained owner domain")?;
        scope.verify_owner_for_cleanup()?;
        let root = config
            .parent()
            .and_then(Path::parent)
            .context("proxy confirmation has no runtime root")?;
        let receipt = verify_resident_engine_receipt(
            root,
            scope.identity(),
            &crate::xmlrpc::default_socket_path(),
        )?
        .context("proxy confirmation has no resident owner receipt")?;
        anyhow::ensure!(
            receipt.config == config,
            "proxy confirmation configuration differs from owner receipt"
        );
        Ok(())
    }

    /// User controls call this before surrendering business Child ownership.
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
        tokio::time::timeout_at(deadline, self.drain_entry_inner(runtime_root, deadline)).await
            .map_err(|_| supervisor::ShutdownUnconfirmed("entry drain deadline exhausted; business retained and publication result unknown".into()))?
    }

    pub(super) async fn drain_entry_inner(
        &self,
        runtime_root: &Path,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let Some(info) = self.resident_entry_settled().await? else {
            if let Some(spec) = configured_entry_spec(&self.conf_path)? {
                return self
                    .migrate_legacy_entry(runtime_root, &entry_config_from_argv(&spec.argv)?, &spec)
                    .await;
            }
            anyhow::ensure!(
                self.dynamic_groups().await?.is_empty(),
                "business groups exist without an owned resident entry configuration"
            );
            return Ok(());
        };
        if info.checked_state(PINGAP_PROGRAM)? != SupervisorProcessState::Running {
            let spec = configured_entry_spec(&self.conf_path)?
                .context("stopped resident has no owned wrapper specification")?;
            return self
                .migrate_legacy_entry(runtime_root, &entry_config_from_argv(&spec.argv)?, &spec)
                .await;
        }
        let (config, spec) = self.live_entry_binding(&info).await?;
        self.authorize_entry_domain(runtime_root, &config, &spec, &info)
            .await
            .map_err(|error| {
                anyhow::Error::new(EntryOwnershipUnconfirmed(format!(
                    "resident owner domain is unconfirmed: {error:#}"
                )))
                .context(supervisor::ShutdownUnconfirmed(
                    "resident ownership is unconfirmed; business retained".into(),
                ))
            })?;
        if config != crate::proxy::compiler::active_config_path(runtime_root) {
            return self
                .migrate_legacy_entry(runtime_root, &config, &spec)
                .await;
        }
        let endpoint = admin_probe::ensure_admin_endpoint()?;
        let observation = admin_probe::fetch_apply_status(endpoint).await?;
        anyhow::ensure!(
            i64::from(observation.process_id) == info.running_identity()?.pid,
            "proxy admin does not belong to the observed supervisord PID"
        );
        crate::proxy::compiler::publish_standby_confirmed_to_path_until(
            runtime_root,
            &crate::proxy::compiler::active_config_path(runtime_root),
            endpoint,
            deadline,
        )
        .await?;
        if self.client.running_process(PINGAP_PROGRAM).await? != info.running_identity()? {
            return Err(supervisor::ShutdownUnconfirmed(
                "resident instance changed after drain; business retained".into(),
            )
            .into());
        }
        Ok(())
    }

    /// Snapshot the exact old -c topology before candidate compatibility checks.
    /// Copying it to the new path does not claim that the old process watches it.
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
        let active = crate::proxy::compiler::active_config_path(runtime_root);
        if config != active {
            crate::proxy::compiler::publish_to_path(&config, &active).await?;
        }
        Ok(())
    }

    pub(super) async fn migrate_legacy_entry(
        &self,
        runtime_root: &Path,
        old_config: &Path,
        old_spec: &ServiceSpecFile,
    ) -> Result<()> {
        let _publication_guard = crate::proxy::compiler::publication_guard().await;
        // A configured but inactive program supplies no PID provenance. Its
        // immutable owner claim must authorize reuse before copying active or
        // withdrawing any supervisor fragment.
        let ownership = (|| -> Result<()> {
            let scope = supervisor::resident::owner_scope()?
                .context("inactive resident has no retained owner capability")?;
            scope.record_running()?;
            let receipt = verify_resident_engine_receipt(
                runtime_root,
                scope.identity(),
                &crate::xmlrpc::default_socket_path(),
            )?
            .context(
                "inactive resident has no owner-domain receipt; explicit bootstrap required",
            )?;
            anyhow::ensure!(
                receipt.config == old_config
                    && std::fs::canonicalize(old_config)?
                        .starts_with(std::fs::canonicalize(runtime_root)?),
                "inactive resident configuration differs from its owner-domain claim"
            );
            Ok(())
        })();
        ownership.map_err(|error| {
            anyhow::Error::new(EntryOwnershipUnconfirmed(format!(
                "inactive resident ownership rejected: {error:#}"
            )))
            .context(supervisor::ShutdownUnconfirmed(
                "inactive resident ownership is unconfirmed; business retained".into(),
            ))
        })?;
        let endpoint = admin_probe::register_verified_legacy_endpoint(old_spec)?;
        let active = crate::proxy::compiler::active_config_path(runtime_root);
        crate::proxy::compiler::publish_to_path(old_config, &active).await?;
        let publication = uuid::Uuid::new_v4().simple().to_string();
        // Build the complete new standby before touching the old entry. An old
        // pin need not implement Applied; the new instance must prove it does.
        let standby = crate::proxy::compiler::publish_standby(runtime_root, &publication).await?;
        self.close_entry().await?;
        let migration = async {
            self.record_entry_domain(runtime_root, &active, None)
                .await?;
            let mut spec = old_spec.clone();
            spec.release_id = crate::svc_spec::RESIDENT_SPEC_ID.into();
            spec.argv[2] = active.to_string_lossy().into_owned();
            spec.write()?;
            let content = match tokio::fs::read_to_string(&self.conf_path).await {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => {
                    return Err(error).context("read dynamic fragment for resident migration");
                }
            };
            write_conf(&self.conf_path, &without_resident_program(&content)).await?;
            let log_root = runtime_root
                .parent()
                .context("resident runtime has no log root")?;
            tokio::fs::create_dir_all(log_root.join("services")).await?;
            let resident_conf = self
                .conf_path
                .parent()
                .context("supervisor fragment has no parent")?
                .join(RESIDENT_CONF_FILE);
            write_conf(&resident_conf, &render_resident_conf(log_root)).await?;
            mutation_result(self.client.reload_config().await)?;
            mutation_result(self.client.add_process_group(PINGAP_PROGRAM).await)?;
            mutation_result(self.client.start_process_wait(PINGAP_PROGRAM).await)?;
            let instance = self.client.running_process(PINGAP_PROGRAM).await?;
            let confirmed =
                admin_probe::wait_for_publication(endpoint, &standby, admin_probe::CONFIRM_BUDGET)
                    .await?;
            anyhow::ensure!(
                i64::from(confirmed.process_id) == instance.pid,
                "new resident Applied receipt belongs to another PID"
            );
            self.record_entry_domain(runtime_root, &active, Some(&instance))
                .await?;
            anyhow::ensure!(
                self.client.running_process(PINGAP_PROGRAM).await? == instance,
                "new resident restarted before migration confirmation"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        migration.map_err(|error| supervisor::ShutdownUnconfirmed(format!(
            "legacy entry was closed; new resident standby is unconfirmed; business execution preserved: {error:#}"
        )).into())
    }

    pub(super) async fn close_entry(&self) -> Result<()> {
        let Some(info) = self.resident_entry_settled().await? else {
            return Ok(());
        };
        if info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running {
            self.live_entry_binding(&info).await?;
        } else {
            let spec = configured_entry_spec(&self.conf_path)?
                .context("resident group has no owned configuration")?;
            admin_probe::register_verified_legacy_endpoint(&spec)?;
        }
        // Remove both restart sources before touching the authenticated group.
        write_conf(
            &self.conf_path,
            "# app-cli business and legacy proxy stopped\n",
        )
        .await?;
        let resident_conf = self
            .conf_path
            .parent()
            .context("dynamic configuration has no parent")?
            .join(RESIDENT_CONF_FILE);
        write_conf(&resident_conf, "# app-cli resident stopped\n").await?;
        mutation_result(self.client.reload_config().await)?;
        mutation_result(self.client.stop_remove_group(PINGAP_PROGRAM).await)?;
        anyhow::ensure!(
            self.resident_entry_settled().await?.is_none(),
            "resident group still registered after shutdown"
        );
        Ok(())
    }

    /// Bind the observed PID/start pair to its exact wrapper spec and -c path.
    /// Credentials alone cannot authorize adopting a namesake business file.
    pub(super) async fn live_entry_binding(
        &self,
        info: &SupervisordProcessInfo,
    ) -> Result<(PathBuf, ServiceSpecFile)> {
        let instance = info.running_identity()?;
        #[cfg(target_os = "linux")]
        {
            let bytes = tokio::fs::read(format!("/proc/{}/cmdline", instance.pid))
                .await
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
                self.client.running_process(PINGAP_PROGRAM).await? == instance,
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
        let scope = supervisor::resident::owner_scope()?
            .context("supervisord adoption requires a retained owner capability")?;
        scope.record_running()?;
        let identity = scope.identity();
        let root =
            std::fs::canonicalize(runtime_root).context("resolve resident runtime domain")?;
        anyhow::ensure!(
            std::fs::canonicalize(config)?.starts_with(&root),
            "resident configuration belongs to another runtime root"
        );
        if let Some(receipt) = verify_resident_engine_receipt(
            runtime_root,
            identity,
            &crate::xmlrpc::default_socket_path(),
        )? {
            anyhow::ensure!(
                receipt.config == config,
                "resident process configuration differs from its owner-domain claim"
            );
            return Ok(());
        }
        // Legacy upgrade is bounded to the current authenticated platform
        // application and its exact source/artifact workspace. Native callers
        // with no prior domain proof must explicitly rebuild the owner.
        anyhow::ensure!(
            identity.physical_domain.is_some() && identity.process_epoch.is_some(),
            "legacy supervisord entry has no physical-domain and incarnation proof"
        );
        anyhow::ensure!(
            std::env::var("PROJECT_ID").ok().as_deref() == Some(identity.application_id.as_str()),
            "legacy proxy application differs from the platform owner context"
        );
        let resource: PathBuf = serde_json::from_value(
            identity
                .binding
                .get("resource")
                .cloned()
                .context("owner workspace binding missing")?,
        )?;
        let resource = std::fs::canonicalize(resource)?;
        let mut workspace_proved = false;
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
        if let Some(session) = runtime_supervisor::current_scope() {
            let path = session.work_root.join(ENGINE_RECEIPT);
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let receipt: EngineReceipt = serde_json::from_slice(&bytes)?;
                    let generation: serde_json::Value = serde_json::from_slice(&std::fs::read(
                        session.work_root.join("generation.json"),
                    )?)?;
                    anyhow::ensure!(
                        generation["id"] == receipt.generation
                            && generation["supervisor"] == receipt.supervisor_id
                            && receipt.socket == crate::xmlrpc::default_socket_path()
                            && generation.get("physical_domain")
                                == identity.physical_domain.as_ref()
                            && generation
                                .get("process_epoch")
                                .and_then(serde_json::Value::as_str)
                                == identity.process_epoch.as_deref(),
                        "legacy engine receipt differs from the owner physical domain or incarnation"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("read legacy engine provenance"),
            }
        }
        #[cfg(target_os = "linux")]
        anyhow::ensure!(
            std::fs::read_link(format!("/proc/{}/ns/pid", info.running_identity()?.pid))?
                == std::fs::read_link("/proc/self/ns/pid")?,
            "legacy proxy belongs to another process namespace"
        );
        self.record_entry_domain(runtime_root, config, Some(&info.running_identity()?))
            .await
    }

    pub(super) async fn record_entry_domain(
        &self,
        runtime_root: &Path,
        config: &Path,
        process: Option<&crate::xmlrpc::RunningProcess>,
    ) -> Result<()> {
        let scope = supervisor::resident::owner_scope()?
            .context("resident engine requires an owner scope")?;
        scope.record_running()?;
        if let Err(error) = verify_resident_engine_receipt(
            runtime_root,
            scope.identity(),
            &crate::xmlrpc::default_socket_path(),
        ) {
            let previous = read_resident_engine_receipt(runtime_root)?
                .context("resident ownership receipt disappeared")?;
            let existing = self.resident_entry_settled().await?;
            let absent = existing
                .as_ref()
                .is_none_or(|info| info.statename != Some(SupervisorProcessState::Running));
            anyhow::ensure!(
                process.is_none()
                    && absent
                    && previous.identity.application_id == scope.identity().application_id
                    && previous.identity.binding == scope.identity().binding
                    && previous.runtime_root == std::fs::canonicalize(runtime_root)?,
                "resident ownership cannot authorize reuse or bootstrap: {error:#}"
            );
            // No local entry is reused. The immutable old claim remains history;
            // the current typed owner publishes a new-domain bootstrap claim.
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
        tokio::fs::create_dir_all(&claims).await?;
        let evidence = claims.join(format!("{}.json", uuid::Uuid::new_v4()));
        crate::proxy::compiler::replace_file_bytes(&evidence, bytes.clone()).await?;
        crate::proxy::compiler::replace_file_bytes(
            &runtime_root.join(RESIDENT_ENGINE_RECEIPT),
            bytes,
        )
        .await
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
        let deadline = tokio::time::Instant::now() + budget;
        tokio::time::timeout_at(deadline, async {
            loop {
                let mut entries = self
                    .client
                    .get_all_process_info()
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
