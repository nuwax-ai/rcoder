//! Ordinary Agent operations share one physical-workload admission domain.
//! The caller keeps the guard through runtime acknowledgement and registration.

use super::AppState;
use rcoder_storage::{
    AdmissionVerifyError, WorkloadAdmissionGuard, WorkloadAdmissionMode, WorkloadKey,
    WorkloadRegistrationFrame,
};
use shared_types::{ServiceType, persistence::PersistenceWriteOutcome};

/// 准入目标核验失败：runtime 名字解析失败或 admission 未覆盖目标。
#[derive(Debug, thiserror::Error)]
pub enum AgentAdmissionTargetError {
    #[error(transparent)]
    Runtime(#[from] container_runtime_api::ContainerRuntimeError),
    #[error("Workload admission does not cover the captured Agent target")]
    Mismatch,
}

/// `refresh_agent_workload_fence` 的失败来源全分类。签名即契约：新增来源会
/// 迫使 HTTP 映射（http-server workload_reuse）补齐，不再静默落入通用后端错误。
#[derive(Debug, thiserror::Error)]
pub enum AgentWorkloadFenceError {
    #[error(transparent)]
    Target(#[from] AgentAdmissionTargetError),
    #[error("Workload admission could not be confirmed before the runtime fence refresh")]
    AdmissionBeforeRuntime {
        #[source]
        source: AdmissionVerifyError,
    },
    #[error("Workload admission could not be confirmed after the runtime fence refresh")]
    AdmissionAfterRuntime {
        #[source]
        source: AdmissionVerifyError,
    },
    #[error(transparent)]
    Runtime(#[from] container_runtime_api::ContainerRuntimeError),
}

/// `stop_ordinary_agent_workload` 的阶段全分类。物理退出→条件退休的既有顺序
/// 不变；未确认的物理结果绝不授权退休登记或替换创建。
#[derive(Debug, thiserror::Error)]
pub enum AgentWorkloadStopError {
    #[error(transparent)]
    Target(#[from] AgentAdmissionTargetError),
    #[error("Stopping Agent compute requires exclusive workload admission")]
    NotExclusive,
    #[error("Workload admission could not be confirmed before capturing the workload")]
    AdmissionBeforeCapture {
        #[source]
        source: AdmissionVerifyError,
    },
    /// 读取待停止快照（read-only 观测）失败。
    #[error(transparent)]
    Capture(#[from] container_runtime_api::ContainerRuntimeError),
    #[error("Agent registration read failed")]
    RegistrationRead(#[source] anyhow::Error),
    /// runtime 停止失败：计算退出未确认，登记原样保留。
    #[error("Agent compute exit remains unconfirmed; registration retained")]
    StopUnconfirmed(#[source] container_runtime_api::ContainerRuntimeError),
    #[error("Agent compute exit confirmed but registration retirement failed")]
    RetirementFailed {
        #[source]
        source: anyhow::Error,
        pending: Option<Vec<container_runtime_api::AgentServiceCleanupFailure>>,
    },
    #[error("Agent registration changed after compute exit; current registration was retained")]
    RegistrationSuperseded {
        pending: Option<Vec<container_runtime_api::AgentServiceCleanupFailure>>,
    },
    /// 计算已退出，但退休登记的 SQL 结果未知：重试原操作，不得重放 prompt。
    #[error(
        "Agent registration retirement remains unconfirmed; retry the original operation: {reason}"
    )]
    RetirementDeferred {
        reason: String,
        pending: Option<Vec<container_runtime_api::AgentServiceCleanupFailure>>,
    },
    #[error(
        "Agent compute stopped and its captured registration retired; Service cleanup remains pending and will be retried: {pending:?}"
    )]
    ServicesPending {
        pending: Vec<container_runtime_api::AgentServiceCleanupFailure>,
    },
}

impl AppState {
    fn verify_agent_admission_target(
        &self,
        guard: &WorkloadAdmissionGuard,
        identifier: &str,
        service_type: &ServiceType,
    ) -> Result<(), AgentAdmissionTargetError> {
        let expected = self
            .runtime()
            .agent_workload_name(identifier, service_type)?;
        if guard.key().namespace() == self.runtime().agent_workload_namespace()
            && guard.key().logical_name() == expected
            && guard.key().family().same_container_family(*service_type)
        {
            Ok(())
        } else {
            Err(AgentAdmissionTargetError::Mismatch)
        }
    }

    /// Explicit Stop/Restart and registered idle cleanup use the same ordering.
    /// An uncertain physical result never grants permission to retire metadata
    /// or to create a replacement under the old logical name.
    pub async fn stop_ordinary_agent_workload(
        &self,
        guard: &WorkloadAdmissionGuard,
        identifier: &str,
        service_type: &ServiceType,
    ) -> Result<bool, AgentWorkloadStopError> {
        self.verify_agent_admission_target(guard, identifier, service_type)?;
        let snapshot = self
            .runtime()
            .capture_agent_stop(identifier, service_type)
            .await?;
        self.stop_captured_ordinary_agent_workload(guard, &snapshot)
            .await
    }

    /// Idle cleanup carries the physical snapshot it actually observed idle;
    /// do not recapture a same-name successor after an awaited status request.
    pub async fn stop_captured_ordinary_agent_workload(
        &self,
        guard: &WorkloadAdmissionGuard,
        snapshot: &container_runtime_api::AgentStopSnapshot,
    ) -> Result<bool, AgentWorkloadStopError> {
        self.verify_agent_admission_target(guard, &snapshot.identifier, &snapshot.service_type)?;
        if guard.mode() != WorkloadAdmissionMode::Exclusive {
            return Err(AgentWorkloadStopError::NotExclusive);
        }
        guard
            .verify_current_typed()
            .await
            .map_err(|source| AgentWorkloadStopError::AdmissionBeforeCapture { source })?;
        let frame = self
            .projects
            .capture_workload_registration(guard, guard.key().logical_name())
            .await
            .map_err(AgentWorkloadStopError::RegistrationRead)?;
        // Explicit stop addresses freshly verified owned runtime resources.
        // Historical registration UIDs are CAS input, never a startup gate.
        let was_existing = snapshot.controller.is_some()
            || snapshot.container.is_some()
            || !snapshot.pods.is_empty()
            || !snapshot.services.is_empty()
            || frame.physical_uid().is_some();
        let pending_services = match self.runtime().stop_captured_agent(snapshot).await {
            Ok(container_runtime_api::AgentStopOutcome::Complete) => None,
            Ok(container_runtime_api::AgentStopOutcome::ComputeGoneServicesPending { pending }) => {
                Some(pending)
            }
            Err(error) => return Err(AgentWorkloadStopError::StopUnconfirmed(error)),
        };
        // Both typed success outcomes prove compute is gone. Retire its exact
        // registration even when Services remain, otherwise inventory would
        // keep seeing canonical references and could never retry those routes.
        // The partial result is still returned as an error after safe cleanup,
        // so this Restart never proceeds to a second compute creation.
        let retirement = self
            .retire_stopped_agent_registration(guard, &frame)
            .await
            .map_err(|source| AgentWorkloadStopError::RetirementFailed {
                source,
                pending: pending_services.clone(),
            })?;
        match retirement {
            PersistenceWriteOutcome::Committed => {}
            PersistenceWriteOutcome::Superseded => {
                return Err(AgentWorkloadStopError::RegistrationSuperseded {
                    pending: pending_services,
                });
            }
            PersistenceWriteOutcome::Deferred { reason } => {
                return Err(AgentWorkloadStopError::RetirementDeferred {
                    reason,
                    pending: pending_services,
                });
            }
        }
        if let Some(address) = snapshot.endpoint.as_deref() {
            // Admission still excludes a successor using the stable endpoint.
            // SSE cleanup above is scoped to the pre-stop canonical sessions.
            self.grpc_pool.remove(address).await;
        }
        if let Some(pending) = pending_services {
            return Err(AgentWorkloadStopError::ServicesPending { pending });
        }
        Ok(was_existing)
    }

    pub async fn acquire_agent_workload(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        mode: WorkloadAdmissionMode,
    ) -> anyhow::Result<WorkloadAdmissionGuard> {
        let name = self
            .runtime()
            .agent_workload_name(identifier, service_type)?;
        let key = WorkloadKey::new(
            &self.runtime().agent_workload_namespace(),
            *service_type,
            &name,
        )?;
        // Same wait budget as Computer creation admission; the caller acquires
        // once and reuses this guard through all internal stages.
        self.projects
            .acquire_workload_admission(
                key,
                mode,
                tokio::time::Instant::now()
                    + crate::service::computer_container_manager::WORKLOAD_ADMISSION_WAIT,
            )
            .await
    }

    /// Refresh the runtime resource-version fence even when Ensure reuses an
    /// already-running instance. No registration is deleted by this operation.
    pub async fn refresh_agent_workload_fence(
        &self,
        guard: &WorkloadAdmissionGuard,
        identifier: &str,
        service_type: &ServiceType,
    ) -> Result<(), AgentWorkloadFenceError> {
        self.verify_agent_admission_target(guard, identifier, service_type)?;
        guard
            .verify_current_typed()
            .await
            .map_err(|source| AgentWorkloadFenceError::AdmissionBeforeRuntime { source })?;
        let snapshot = self
            .runtime()
            .capture_agent_stop(identifier, service_type)
            .await?;
        self.runtime()
            .refresh_agent_retirement_fence(&snapshot)
            .await?;
        guard
            .verify_current_typed()
            .await
            .map_err(|source| AgentWorkloadFenceError::AdmissionAfterRuntime { source })
    }

    /// Retire only the canonical frame captured before the acknowledged stop.
    /// A competing registration or uncertain SQL result keeps all local routes
    /// and claims intact. Never enumerate current project sessions after CAS.
    pub async fn retire_stopped_agent_registration(
        &self,
        guard: &WorkloadAdmissionGuard,
        frame: &WorkloadRegistrationFrame,
    ) -> anyhow::Result<PersistenceWriteOutcome> {
        let old_routes: Vec<_> = frame
            .project_ids()
            .into_iter()
            .filter_map(|id| {
                let project = self.get_project(id)?;
                let container = project.container_info()?;
                if container.container_name != frame.container_name()
                    || frame
                        .physical_uid()
                        .is_some_and(|uid| uid != container.container_id)
                {
                    return None;
                }
                Some((
                    id.to_owned(),
                    project.user_id().map(str::to_owned),
                    project.service_type(),
                    container,
                ))
            })
            .collect();
        let old_streams: Vec<_> = frame
            .session_ids()
            .into_iter()
            .map(|sid| self.session_stream_registry.capture_session_streams(sid))
            .collect();
        let claims: Vec<_> = frame
            .session_ids()
            .into_iter()
            .filter_map(|sid| {
                self.session_stream_registry
                    .capture_first_client_claim(sid)
                    .map(|claim| (sid.to_owned(), claim))
            })
            .collect();
        guard.verify_current().await?;
        let outcome = self
            .projects
            .retire_workload_registration(guard, frame)
            .await?;
        if outcome == PersistenceWriteOutcome::Committed {
            for streams in old_streams {
                self.session_stream_registry
                    .shutdown_captured_streams(streams);
            }
            for (sid, claim) in claims {
                self.session_stream_registry
                    .release_first_client_claim_if_current(&sid, &claim);
            }
            for (project_id, user_id, service_type, container) in old_routes {
                // Use the runtime scope pinned by admission. A deployment can
                // override the configured namespace; retiring that workload
                // must not invalidate a same-name route in another namespace.
                let expected = shared_types::build_backend_addr(
                    &container.container_name,
                    &container.container_ip,
                    guard.key().namespace(),
                    &self.cluster_domain,
                );
                if let Some(proxy) = self.pingora_service.as_ref() {
                    match service_type {
                        Some(
                            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject,
                        ) => {
                            if let Some(user) = user_id.as_deref() {
                                proxy.remove_vnc_backend_if_matches(user, &expected);
                            }
                        }
                        Some(ServiceType::WebAgentRunner) => {
                            proxy.remove_project_backend_if_matches(&project_id, &expected);
                        }
                        Some(ServiceType::Userapp | ServiceType::UserappBuilder) | None => {}
                    }
                }
                if let Ok(address) = shared_types::build_grpc_addr(
                    &container.container_name,
                    &container.container_ip,
                    guard.key().namespace(),
                    &self.cluster_domain,
                ) {
                    self.grpc_pool.remove(&address).await;
                }
            }
        }
        Ok(outcome)
    }
}
