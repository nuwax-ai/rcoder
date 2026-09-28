use super::*;

pub struct PreviewCoordinator {
    pub(crate) store: Arc<dyn shared_types::PreviewLifecycleStore>,
    pub(crate) executor: Arc<dyn shared_types::PreviewExecutor>,
    pub(super) evidence: Arc<dyn HostEvidence>,
    pub(super) dispatch: RemoteDispatch,
    /// 内部令牌（派发客户端与内部端点 guard 同源；显式持有避免 env 名漂移）。
    pub(crate) token: String,
    pub(super) host: PreviewHostIdentity,
    pub(super) config: CoordinatorConfig,
    pub(super) route_cache: RouteCache,
    pub(super) activity: ActivityAccumulator,
}

impl PreviewCoordinator {
    pub fn new(
        store: Arc<dyn shared_types::PreviewLifecycleStore>,
        executor: Arc<dyn shared_types::PreviewExecutor>,
        evidence: Arc<dyn HostEvidence>,
        internal_token: String,
        config: CoordinatorConfig,
    ) -> Self {
        let host = local_host_identity();
        let dispatch = RemoteDispatch::new(
            internal_token.clone(),
            config.peer_api_port,
            config.remote_dispatch_timeout_secs,
        );
        let route_cache = RouteCache::new(
            config.route_cache_positive_secs,
            config.route_cache_negative_secs,
        );
        Self {
            store,
            executor,
            evidence,
            dispatch,
            token: internal_token,
            host,
            config,
            route_cache,
            activity: ActivityAccumulator::default(),
        }
    }

    pub fn host(&self) -> &PreviewHostIdentity {
        &self.host
    }

    pub fn route_cache(&self) -> &RouteCache {
        &self.route_cache
    }

    pub fn activity(&self) -> &ActivityAccumulator {
        &self.activity
    }

    pub fn config(&self) -> &CoordinatorConfig {
        &self.config
    }

    /// 内部令牌（rcoder 装配 Pingora 转发槽回填用；与派发客户端/内部端点同源）。
    pub fn internal_token(&self) -> &str {
        &self.token
    }

    pub(super) fn heartbeat_fresh(&self, row: &PreviewInstanceRecord) -> bool {
        row.last_heartbeat_at.is_some_and(|at| {
            let age = chrono::Utc::now()
                .signed_duration_since(at)
                .to_std()
                .unwrap_or_default();
            age < Duration::from_secs(self.config.heartbeat_ttl_secs)
        })
    }

    /// Unknown 行恢复证据：宿主 Pod 不存在（K8s 查证/单机恒成立）。
    pub(super) async fn recovery_evidence(&self, row: &PreviewInstanceRecord) -> Option<String> {
        let pod_uid = pod_uid_of(&row.host_id);
        if self.host_pod_exists(pod_uid).await {
            tracing::warn!(
                preview_key = %row.preview_key,
                host_id = %row.host_id,
                "unknown instance host pod still exists; refuse takeover"
            );
            return None;
        }
        Some(format!(
            "host pod {pod_uid} no longer exists (verified at startup/recovery)"
        ))
    }

    pub(crate) async fn host_pod_exists(&self, pod_uid: &str) -> bool {
        self.evidence.host_pod_exists(pod_uid).await
    }

    /// With positive evidence that the recorded host no longer exists, fence
    /// the exact instance through Unknown and settle it as Stopped. Both writes
    /// remain generation-bound in the store, so a replacement instance wins a
    /// concurrent race instead of being overwritten by this recovery.
    pub(crate) async fn settle_orphaned_instance(
        &self,
        row: &PreviewInstanceRecord,
        evidence: &str,
    ) -> Result<PreviewInstanceRecord, PreviewCoordinationError> {
        if evidence.is_empty() {
            return Err(invalid("orphaned preview recovery evidence is empty"));
        }
        let unknown = if row.state == PreviewInstanceState::Unknown {
            row.clone()
        } else {
            self.store
                .mark_unknown(&row.preview_key, &row.instance_id, evidence)
                .await
                .map_err(store_error)?
        };
        let stopped = self
            .store
            .resolve_unknown_stopped(&unknown.preview_key, &unknown.instance_id, evidence)
            .await
            .map_err(store_error)?;
        if let Some(port) = stopped.port {
            self.route_cache.invalidate(port);
        }
        tracing::info!(
            preview_key = %stopped.preview_key,
            instance_id = %stopped.instance_id,
            host_id = %stopped.host_id,
            "orphaned preview instance reconciled as stopped"
        );
        Ok(stopped)
    }

    pub(super) async fn settle_if_host_absent(
        &self,
        row: &PreviewInstanceRecord,
    ) -> Result<bool, PreviewCoordinationError> {
        if row.host_id == self.host.host_id {
            return Ok(false);
        }
        let Some(evidence) = self.recovery_evidence(row).await else {
            return Ok(false);
        };
        self.settle_orphaned_instance(row, &evidence).await?;
        Ok(true)
    }
}
