use super::*;

impl PreviewCoordinator {
    fn start_envelope(row: &PreviewInstanceRecord, message: &str) -> PreviewStartEnvelope {
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
        // 预读：幂等快路径 + Unknown 证据准备
        let mut recovery = None;
        if let Ok(Some(row)) = self.store.get(&key).await {
            match row.state {
                PreviewInstanceState::Ready if self.heartbeat_fresh(&row) => {
                    if !self.settle_if_host_absent(&row).await? {
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

        let mut candidate: Option<u16> = None;
        for _ in 0..START_PORT_ATTEMPTS {
            let input = AcceptStartInput {
                preview_key: key.clone(),
                project_id: identity.project_id.clone(),
                project_path: identity.resolved_path.clone(),
                host: self.host.clone(),
                operation_id: new_id(),
                instance_id: new_id(),
                requested_port: candidate,
                recover_unknown_evidence: recovery.clone(),
            };
            match self.store.accept_start(input).await {
                Ok(AcceptStartOutcome::Admitted(record)) => {
                    match self
                        .execute_start(&key, base_path.as_deref(), &record)
                        .await
                    {
                        Ok(envelope) => return Ok(envelope),
                        Err(StartExecError::PortInUse(port)) => {
                            note_store_result(
                                self.store
                                    .mark_failed(
                                        &key,
                                        &record.instance_id,
                                        record.revision,
                                        &format!("port {port} occupied locally, retrying"),
                                    )
                                    .await,
                            );
                            candidate = Some(Self::next_candidate(port));
                            continue;
                        }
                        Err(StartExecError::Fatal(error)) => {
                            note_store_result(
                                self.store
                                    .mark_failed(
                                        &key,
                                        &record.instance_id,
                                        record.revision,
                                        &error.to_string(),
                                    )
                                    .await,
                            );
                            return Err(error);
                        }
                    }
                }
                Ok(AcceptStartOutcome::ExistingReady(row)) => {
                    return Ok(Self::start_envelope(&row, "Development server started"));
                }
                Ok(AcceptStartOutcome::Blocked(row)) => {
                    return Err(conflict(format!(
                        "preview operation in progress: state={:?}",
                        row.state
                    )));
                }
                Err(error) => return Err(store_error(error)),
            }
        }
        Err(unavailable("preview start exhausted port retry budget"))
    }

    /// 受理后已知的换端口候选（避开冲突口与保留区；分配器已排除全局占用）。
    fn next_candidate(prev: u16) -> u16 {
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

    async fn execute_start(
        &self,
        key: &str,
        base_path: Option<&str>,
        record: &PreviewInstanceRecord,
    ) -> Result<PreviewStartEnvelope, StartExecError> {
        let Some(port) = record.port else {
            return Err(StartExecError::Fatal(invalid("admitted record lacks port")));
        };
        let ticket = ExecutorStartTicket {
            preview_key: key.to_string(),
            log_key: record.project_id.clone(),
            project_path: record.project_path.clone(),
            port,
            base_path: base_path.map(str::to_string),
            instance_id: record.instance_id.clone(),
        };
        let budget = Duration::from_secs(self.config.start_budget_secs);
        let started = tokio::time::timeout(budget, self.executor.start_local(&ticket)).await;
        let (pid, actual_port) = match started {
            Ok(Ok(pair)) => pair,
            Ok(Err(shared_types::PreviewExecutorError::PortInUse(detail))) => {
                return Err(StartExecError::PortInUse(port))
                    .inspect_err(|_| tracing::warn!(key, port, "preview port in use: {detail}"));
            }
            Ok(Err(error)) => {
                return Err(StartExecError::Fatal(unavailable(error.to_string())));
            }
            Err(_elapsed) => {
                // 预算超时：行保持 starting、执行器可能仍在安装——不自动重放；
                // 本机心跳扫描若发现已就绪将自愈 publish_running。
                return Err(StartExecError::Fatal(unavailable(
                    "preview start budget exceeded; instance left starting for heartbeat self-heal",
                )));
            }
        };
        let effective_base = base_path
            .map(normalize_base_path)
            .unwrap_or_else(|| default_base(actual_port));
        match self
            .store
            .publish_running(
                key,
                &record.operation_id,
                record.revision,
                pid,
                actual_port,
                Some(effective_base.as_str()),
            )
            .await
        {
            Ok(row) => {
                self.route_cache.invalidate(actual_port);
                Ok(Self::start_envelope(&row, "Development server started"))
            }
            Err(PreviewStoreError::Conflict(detail)) => {
                // 受理被并发接管：收掉我们刚起的进程（登记身份匹配才杀）。
                if let Err(kill_error) = self.executor.stop_local(key, &record.instance_id).await {
                    tracing::warn!(key, "post-supersede cleanup kill failed: {kill_error}");
                }
                Err(StartExecError::Fatal(conflict(format!(
                    "start admission superseded: {detail}"
                ))))
            }
            Err(error) => Err(StartExecError::Fatal(store_error(error))),
        }
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
                self.store
                    .mark_stopped(
                        &stopping.preview_key,
                        &stopping.operation_id,
                        stopping.revision,
                    )
                    .await
                    .map_err(store_error)?;
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
                .executor
                .stop_local(&stopping.preview_key, &stopping.instance_id)
                .await
                .map_err(|e| unavailable(e.to_string()));
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

/// 启动执行的内部错误分类（端口冲突可换端口重试）。
enum StartExecError {
    PortInUse(u16),
    Fatal(PreviewCoordinationError),
}
