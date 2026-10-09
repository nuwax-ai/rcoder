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
                }
                // 换代清理不发布 standby：入口保持当前配置（接管方的编排
                // 会发布新 active）；发布反而会与接管方竞态。
                .finish_verified_generation(root, &generation)
                .await?;
                runtime_supervisor::CleanupOutcome::Empty.record(root)?;
                Ok(runtime_supervisor::CleanupOutcome::Empty)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if Self::detect().await?.is_some() {
                    return Err(record_outcome(root, runtime_supervisor::CleanupOutcome::ObservationFailed {
                        reason: "supervisord is reachable but this generation has no engine ownership receipt".into(),
                    }));
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

    pub(crate) async fn shutdown_owner_entry(
        &self,
        runtime_root: &Path,
        scope: &runtime_supervisor::ResidentScope,
    ) -> Result<()> {
        scope
            .verify_owner_for_cleanup()
            .context("verify retained owner scope before resident shutdown")?;
        let configured = configured_entry_spec(&self.conf_path)?;
        let receipt = verify_resident_engine_receipt(
            runtime_root,
            scope.identity(),
            &crate::xmlrpc::default_socket_path(),
        )?;
        let Some(receipt) = receipt else {
            anyhow::ensure!(
                configured.is_none(),
                "configured resident has no owner domain receipt; shutdown is not authorized"
            );
            anyhow::ensure!(
                self.resident_entry_settled().await?.is_none()
                    && self.dynamic_groups().await?.is_empty(),
                "supervisord programs exist without owner domain provenance; shutdown is not authorized"
            );
            return Ok(());
        };
        if let Some(spec) = configured {
            anyhow::ensure!(
                entry_config_from_argv(&spec.argv)? == receipt.config,
                "configured resident differs from its owner domain receipt"
            );
        }
        if let Some(info) = self.resident_entry_settled().await?
            && info.checked_state(PINGAP_PROGRAM)? == SupervisorProcessState::Running
        {
            let (config, _) = self.live_entry_binding(&info).await?;
            anyhow::ensure!(
                config == receipt.config,
                "resident configuration belongs to a different owner runtime root"
            );
        }
        self.close_entry().await?;
        self.stop_business().await
    }

    /// The caller has already crossed a confirmed drain barrier, or has
    /// precisely shut down the resident entry during forced owner cleanup.
    pub(crate) async fn stop_business(&self) -> Result<()> {
        // Remove the restart source before stopping the live groups. Keeping the
        // old fragment after removeProcessGroup lets a supervisord restart or a
        // later reload resurrect the retired release. Do not touch fixed groups
        // and the resident fragment (app-pingap).
        let mut failures = Vec::new();
        if let Err(error) =
            write_conf(&self.conf_path, "# app-cli dynamic services stopped\n").await
        {
            failures.push(format!("withdraw dynamic configuration: {error:#}"));
        }
        if let Err(error) = mutation_result(self.client.reload_config().await) {
            failures.push(format!("reload dynamic configuration: {error:#}"));
        }
        // A broken configuration must not prevent stopping live groups. Keep
        // the withdrawal failure, but attempt the physical stop before returning
        // it; an external cleanup receipt still requires every part to succeed.
        let groups = self.dynamic_groups().await?;
        // Begin shutdown together; one slow service must not multiply the grace
        // period by the number of application modules.
        for (name, result) in futures::future::join_all(
            groups
                .iter()
                .map(|name| async move { (name, self.client.stop_remove_group(name).await) }),
        )
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
        let remaining = self.dynamic_groups().await?;
        if !remaining.is_empty() {
            bail!("dynamic groups remain after stop: {}", remaining.join(", "));
        }
        Ok(())
    }

    /// 当前动态业务组名集合（`app-svc-*`；P1 起 `app-pingap` 为常驻组，
    /// 不属业务停止/清理范围——入口生命周期独立于编排代次）。
    pub(super) async fn dynamic_groups(&self) -> Result<Vec<String>> {
        Ok(self
            .managed_groups()
            .await?
            .into_iter()
            .filter(|group| group != PINGAP_PROGRAM)
            .collect())
    }

    /// app-cli 托管的全部组（业务 `app-svc-*` + 常驻 `app-pingap`）——
    /// 编排就绪检查用（判定常驻组是否已被 supervisord 托管）。
    pub(super) async fn managed_groups(&self) -> Result<Vec<String>> {
        let infos = self.client.get_all_process_info().await?;
        let mut groups = std::collections::BTreeSet::new();
        for info in infos {
            if info.group.starts_with(SVC_PROGRAM_PREFIX) || info.group == PINGAP_PROGRAM {
                groups.insert(info.group);
            }
        }
        Ok(groups.into_iter().collect())
    }
}
