use super::*;

impl PreviewCoordinator {
    pub(super) fn start_envelope(
        row: &PreviewInstanceRecord,
        message: &str,
    ) -> PreviewStartEnvelope {
        PreviewStartEnvelope {
            success: true,
            message: message.to_string(),
            project_id: row.project_id.clone(),
            pid: row.pid.unwrap_or_default(),
            port: row.port.unwrap_or_default(),
        }
    }

    /// 受理 + 执行启动（端口有界重试）。
    pub(super) async fn admit_and_start(
        &self,
        identity: &PreviewProjectIdentity,
        base_path: Option<String>,
    ) -> Result<PreviewStartEnvelope, PreviewCoordinationError> {
        let key = compute_key(identity);
        let base_path = base_path.as_deref().map(normalize_base_path);
        // 预读：幂等快路径 + Unknown 证据准备
        let mut recovery = None;
        if let Ok(Some(row)) = self.store.get(&key).await {
            match row.state {
                PreviewInstanceState::Ready if self.heartbeat_fresh(&row) => {
                    if !self.settle_if_host_absent(&row).await? {
                        Self::check_requested_base(&row, base_path.as_deref())?;
                        return Ok(Self::start_envelope(&row, "Development server started"));
                    }
                }
                PreviewInstanceState::Ready
                | PreviewInstanceState::Starting
                | PreviewInstanceState::Stopping
                    if self.settle_if_host_absent(&row).await? => {}
                PreviewInstanceState::Unknown => match self.recovery_evidence(&row).await {
                    Some(evidence) => {
                        recovery = Some(shared_types::PreviewRecoveryEvidence {
                            instance_id: row.instance_id.clone(),
                            revision: row.revision,
                            detail: evidence,
                        })
                    }
                    None => {
                        return Err(conflict(
                            "preview instance state unknown and host pod still exists; takeover refused",
                        ));
                    }
                },
                _ => {}
            }
        }

        let input = AcceptStartInput {
            preview_key: key,
            project_id: identity.project_id.clone(),
            project_path: identity.resolved_path.clone(),
            host: self.host.clone(),
            operation_id: new_id(),
            instance_id: new_id(),
            requested_port: None,
            base_path,
            recover_unknown_evidence: recovery,
        };
        let owner = self.start_owned(input)?;
        let observed = tokio::time::timeout(
            Duration::from_secs(self.config.start_budget_secs), owner.wait_result(),
        ).await.map_err(|_| unavailable(
            "preview start budget exceeded; original admission/start owner continues without replay",
        ))??;
        match observed {
            startup::StartupResult::Ready(envelope) => Ok(envelope),
            startup::StartupResult::Error(error) => Err(startup::copy_error(&error)),
        }
    }

    pub(super) fn check_requested_base(
        row: &PreviewInstanceRecord,
        requested: Option<&str>,
    ) -> Result<(), PreviewCoordinationError> {
        if let Some(requested) = requested {
            let selected = row.base_path.clone().or_else(|| row.port.map(default_base));
            if selected.as_deref() != Some(requested) {
                return Err(conflict(
                    "preview instance already uses a different base path",
                ));
            }
        }
        Ok(())
    }

    /// 受理后已知的换端口候选（避开冲突口与保留区；分配器已排除全局占用）。
    pub(super) fn next_candidate(prev: u16) -> u16 {
        let mut next = if prev >= PREVIEW_PORT_MAX {
            PREVIEW_PORT_MIN
        } else {
            prev + 1
        };
        if (PREVIEW_PORT_RESERVED_MIN..=PREVIEW_PORT_RESERVED_MAX).contains(&next) {
            next = PREVIEW_PORT_RESERVED_MAX + 1;
        }
        next
    }

    /// 单实例协调停止（受理→派发→终态）。
    pub(super) async fn coordinated_stop(
        &self,
        row: &PreviewInstanceRecord,
    ) -> Result<(), PreviewCoordinationError> {
        if self.settle_if_host_absent(row).await? {
            return Ok(());
        }
        // Resume an already accepted stop with its durable operation identity.
        // Re-dispatch is safe: executor stop is instance-bound and idempotent.
        let current = self
            .store
            .get(&row.preview_key)
            .await
            .map_err(store_error)?
            .ok_or_else(|| invalid("preview stop target disappeared"))?;
        if current.instance_id != row.instance_id {
            return Err(conflict(
                "stop target superseded by a newer preview instance",
            ));
        }
        let stopping = if current.state == PreviewInstanceState::Stopping {
            current
        } else {
            self.store
                .accept_stop(&current.preview_key, &new_id())
                .await
                .map_err(store_error)?
        };
        if !stopping.state.is_active() {
            return Ok(()); // 幂等：并发停止已完成或实例已终结
        }
        let outcome = match self.dispatch_stop(&stopping, &stopping.operation_id).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let detail = format!("stop dispatch outcome unknown: {error}");
                if let Err(store_failure) = self
                    .store
                    .mark_unknown(&stopping.preview_key, &stopping.instance_id, &detail)
                    .await
                {
                    return Err(unavailable(format!(
                        "{error}; failed to persist uncertain stop outcome: {store_failure}"
                    )));
                }
                if let Some(port) = stopping.port {
                    self.route_cache.invalidate(port);
                }
                return Err(error);
            }
        };
        match outcome {
            ExecutorStopOutcome::Stopped | ExecutorStopOutcome::NotRegistered => {
                match self
                    .store
                    .mark_stopped(
                        &stopping.preview_key,
                        &stopping.operation_id,
                        stopping.revision,
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(PreviewStoreError::Conflict(detail)) => {
                        let current = self
                            .store
                            .get(&stopping.preview_key)
                            .await
                            .map_err(store_error)?;
                        if !current.is_some_and(|row| {
                            row.instance_id == stopping.instance_id
                                && row.operation_id == stopping.operation_id
                                && row.revision == stopping.revision
                                && row.state == PreviewInstanceState::Stopped
                        }) {
                            return Err(conflict(detail));
                        }
                    }
                    Err(
                        error @ (PreviewStoreError::Unavailable(_) | PreviewStoreError::Invalid(_)),
                    ) => return Err(store_error(error)),
                }
                if let Some(port) = stopping.port {
                    self.route_cache.invalidate(port);
                }
                Ok(())
            }
            // 迟到旧操作打到新登记：不杀新实例；行由新实例持有，本操作作废。
            ExecutorStopOutcome::IdentityMismatch => {
                let detail = "stop target superseded by a newer instance on the host";
                self.store
                    .mark_unknown(&stopping.preview_key, &stopping.instance_id, detail)
                    .await
                    .map_err(store_error)?;
                if let Some(port) = stopping.port {
                    self.route_cache.invalidate(port);
                }
                Err(conflict(detail))
            }
        }
    }

    /// 停止派发：本机执行器 or 宿主 Pod 内部端点。
    async fn dispatch_stop(
        &self,
        stopping: &PreviewInstanceRecord,
        operation_id: &str,
    ) -> Result<ExecutorStopOutcome, PreviewCoordinationError> {
        if stopping.host_id == self.host.host_id {
            return self
                .stop_local_startup(&stopping.preview_key, &stopping.instance_id)
                .await;
        }
        let Some(pod_ip) = stopping.pod_ip.clone() else {
            return Err(unavailable(format!(
                "stop target host {} lacks pod ip",
                stopping.host_id
            )));
        };
        self.dispatch
            .remote_stop(
                &pod_ip,
                &stopping.preview_key,
                &stopping.instance_id,
                operation_id,
                stopping.revision,
            )
            .await
            .map_err(unavailable)
    }

    /// verify 派发：本机 or 远端（不可达=Unavailable）。
    pub(super) async fn verify_via_host(
        &self,
        row: &PreviewInstanceRecord,
    ) -> Result<ExecutorVerifyReport, PreviewCoordinationError> {
        if row.host_id == self.host.host_id {
            return self
                .executor
                .verify_local(&row.preview_key, &row.instance_id)
                .await
                .map_err(|e| unavailable(e.to_string()));
        }
        let Some(pod_ip) = row.pod_ip.clone() else {
            return Err(unavailable(format!(
                "verify target host {} lacks pod ip",
                row.host_id
            )));
        };
        self.dispatch
            .remote_verify(
                &pod_ip,
                &row.preview_key,
                &row.instance_id,
                self.config.remote_verify_timeout_secs,
            )
            .await
            .map_err(unavailable)
    }

    pub(super) fn degraded(
        project_id: &str,
        reason: &str,
        message: &str,
    ) -> PreviewKeepAliveEnvelope {
        PreviewKeepAliveEnvelope {
            success: false,
            message: message.to_string(),
            project_id: project_id.to_string(),
            pid: None,
            port: None,
            action: None,
            reason: Some(reason.to_string()),
        }
    }

    pub(super) fn rebuilt_envelope(env: PreviewStartEnvelope) -> PreviewKeepAliveEnvelope {
        PreviewKeepAliveEnvelope {
            success: true,
            message: "Development server started".to_string(),
            project_id: env.project_id,
            pid: Some(env.pid),
            port: Some(env.port),
            action: Some("start".to_string()),
            reason: None,
        }
    }

    /// 按端口定位活跃实例（先缓存后权威库）。
    pub(super) async fn locate_by_port(
        &self,
        port: u16,
    ) -> Result<Option<PreviewInstanceRecord>, PreviewCoordinationError> {
        self.store
            .find_active_by_port(port)
            .await
            .map_err(store_error)
    }
}
