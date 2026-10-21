//! Deadline-bound official proxy writes retain the exact owner and incarnation.
use super::*;
use crate::proxy::{
    apply_status::ConfirmedPublication,
    publication::{ProcessHost, ProcessInstance},
};

pub(super) struct OfficialSupervisorHost<'a> {
    pub(super) host: &'a SupervisordHost,
    pub(super) runtime_root: &'a Path,
    pub(super) spec: ServiceSpecFile,
    pub(super) current: Option<(ProcessInstance, crate::xmlrpc::RunningProcess)>,
    pub(super) cancel: Option<&'a tokio_util::sync::CancellationToken>,
    pub(super) epoch: u64,
}

impl OfficialSupervisorHost<'_> {
    fn admitted(&self, deadline: tokio::time::Instant) -> Result<()> {
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline
                && !self.cancel.is_some_and(|token| token.is_cancelled())
                && !supervisor::resident::owner_is_closing()?,
            "proxy operation expired or was cancelled"
        );
        anyhow::ensure!(
            supervisor::resident::control_epoch()? == self.epoch,
            "proxy operation revoked by lifecycle control"
        );
        let scope = supervisor::resident::owner_scope()?
            .context("proxy operation has no owner capability")?;
        scope.verify_owner_for_cleanup()?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProcessHost for OfficialSupervisorHost<'_> {
    async fn stop_exact(&mut self, deadline: tokio::time::Instant) -> Result<()> {
        // XML-RPC mutations settle their actual outcome. Do not cancel an
        // in-flight write and pretend the group was never started or stopped.
        self.host.close_entry_until(deadline).await?;
        self.current = None;
        let scope =
            supervisor::resident::owner_scope()?.context("proxy stop has no owner capability")?;
        if !supervisor::resident::owner_is_closing()? {
            scope.record_running_until(deadline).await?;
        }
        Ok(())
    }
    async fn start_exact(
        &mut self,
        candidate: &CompileOutcome,
        deadline: tokio::time::Instant,
    ) -> Result<ProcessInstance> {
        self.admitted(deadline)?;
        let scope =
            supervisor::resident::owner_scope()?.context("proxy start has no owner capability")?;
        scope.record_running_until(deadline).await?;
        let mut spec = self.spec.clone();
        spec.release_id = crate::svc_spec::RESIDENT_SPEC_ID.into();
        spec.argv.truncate(3);
        spec.argv[2] = candidate.config_path.to_string_lossy().into_owned();
        let credentials = [
            "PINGAP_ADMIN_ADDR",
            "PINGAP_ADMIN_USER",
            "PINGAP_ADMIN_PASSWORD",
        ]
        .into_iter()
        .filter_map(|key| {
            spec.env
                .get(key)
                .cloned()
                .map(|value| (key.to_string(), value))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
        spec.env
            .extend(crate::proxy::official_env::cleared_environment());
        spec.env.extend(credentials);
        self.host
            .record_entry_domain_until(self.runtime_root, &candidate.config_path, None, deadline)
            .await?;
        relative_bootstrap::write_proxy_spec_until(&spec, deadline).await?;
        let log_root = self
            .runtime_root
            .parent()
            .context("proxy runtime log root missing")?;
        tokio::fs::create_dir_all(log_root.join("services")).await?;
        write_conf_until(
            &self.host.resident_conf_path()?,
            &render_resident_conf(log_root),
            deadline,
        )
        .await?;
        self.admitted(deadline)?;
        mutation_result(self.host.client.reload_config_until(deadline).await)?;
        self.admitted(deadline)?;
        scope.record_running_until(deadline).await?;
        mutation_result(
            self.host
                .client
                .add_process_group_until(PINGAP_PROGRAM, deadline)
                .await,
        )?;
        self.admitted(deadline)?;
        scope.record_running_until(deadline).await?;
        mutation_result(
            self.host
                .client
                .start_process_wait_until(PINGAP_PROGRAM, deadline)
                .await,
        )?;
        self.admitted(deadline)?;
        let running = self
            .host
            .client
            .running_process_until(PINGAP_PROGRAM, deadline)
            .await?;
        self.admitted(deadline)?;
        let instance = ProcessInstance {
            pid: u32::try_from(running.pid).context("invalid official proxy PID")?,
            id: uuid::Uuid::new_v4().to_string(),
        };
        self.current = Some((instance.clone(), running));
        self.spec = spec;
        Ok(instance)
    }
    async fn verify_exact(
        &mut self,
        expected: &ProcessInstance,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.admitted(deadline)?;
        let (held, running) = self
            .current
            .as_ref()
            .context("proxy host has no retained instance")?;
        anyhow::ensure!(
            held == expected
                && self
                    .host
                    .client
                    .running_process_until(PINGAP_PROGRAM, deadline)
                    .await?
                    == *running,
            "official proxy supervisord process identity changed"
        );
        let info = self
            .host
            .client
            .process_info_until(PINGAP_PROGRAM, deadline)
            .await?;
        let (config, _) = self.host.live_entry_binding_until(&info, deadline).await?;
        anyhow::ensure!(
            config == entry_config_from_argv(&self.spec.argv)?,
            "official proxy launch snapshot changed"
        );
        Ok(())
    }
    async fn confirm(
        &mut self,
        candidate: &CompileOutcome,
        instance: &ProcessInstance,
        deadline: tokio::time::Instant,
    ) -> Result<ConfirmedPublication> {
        self.admitted(deadline)?;
        let endpoint = admin_probe::ensure_admin_endpoint()?;
        let cancelled = async {
            match self.cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            result=admin_probe::wait_for_official_publication(endpoint,candidate,instance.pid,&instance.id,
                deadline.saturating_duration_since(tokio::time::Instant::now()))=>result,
            ()=cancelled=>Err(anyhow::anyhow!("proxy confirmation cancelled by its operation")),
            ()=supervisor::resident::control_revoked(self.epoch)=>Err(anyhow::anyhow!("proxy confirmation revoked by lifecycle control")),
        }
    }
    async fn commit(
        &mut self,
        candidate: &CompileOutcome,
        receipt: &ConfirmedPublication,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.admitted(deadline)?;
        let (_, running) = self
            .current
            .as_ref()
            .context("proxy commit has no retained instance")?;
        self.host
            .record_entry_domain_until(
                self.runtime_root,
                &candidate.config_path,
                Some(running),
                deadline,
            )
            .await?;
        self.admitted(deadline)?;
        crate::proxy::compiler::publish_active_until(
            self.runtime_root,
            &candidate.config_path,
            deadline,
        )
        .await?;
        self.admitted(deadline)?;
        let file = crate::proxy::compiler::prepare_official_confirmation(
            self.runtime_root,
            candidate,
            receipt,
        )?;
        supervisor::resident::commit_current_epoch(deadline, self.epoch, || {
            file.publish()?;
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "supervisord proxy confirmation commit deadline exhausted"
            );
            crate::proxy::compiler::record_confirmed_publication(candidate, receipt)
        })
    }
    fn mark_unknown(&self, message: String) -> anyhow::Error {
        if let Ok(Some(scope)) = supervisor::resident::owner_scope()
            && let Err(error) = scope.record_unknown(&message)
        {
            return supervisor::ShutdownUnconfirmed(format!(
                "{message}; persist proxy Unknown failed: {error:#}"
            ))
            .into();
        }
        supervisor::ShutdownUnconfirmed(message).into()
    }
}
