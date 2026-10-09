//! Userapp 生命周期 + 观测操作（从 service.rs 拆出，extension-impl）。
//!
//! start/stop/restart/recycle + stats/events 观测委托（转调 ContainerRuntime）。

use tracing::{info, instrument};

use container_runtime_api::DeploymentStatus;

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    /// 启动应用（scale replicas = 1）
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        let _identity = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
            })?;
        self.start_app_controlled(
            app_id,
            shared_types::UserAppControlRequest {
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
    }

    pub async fn start_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<AppRuntimeInfo> {
        self.activate_existing_runtime(app_id, request, false).await
    }

    pub async fn restart_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<AppRuntimeInfo> {
        crate::service::restart_wait::with_context(
            self.activate_existing_runtime(app_id, request, true),
        )
        .await
    }

    async fn activate_existing_runtime(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
        restart: bool,
    ) -> AppResult<AppRuntimeInfo> {
        let kind = if restart {
            shared_types::UserAppOperationKind::Restart
        } else {
            shared_types::UserAppOperationKind::Start
        };
        let wait_deadline = if restart {
            Some(self.restart_admission_deadline()?)
        } else {
            None
        };
        validate_app_id(app_id)?;
        self.discover_missing_identity(app_id).await?;
        crate::service::restart_wait::prepare(
            self.verify_recovered_storage(app_id, shared_types::UserAppOperationScope::Prod),
        )
        .await?;
        crate::service::restart_wait::prepare(
            self.metadata
                .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref()),
        )
        .await?;
        let captured_lifecycle = crate::service::restart_wait::prepare(async {
            Ok(self
                .metadata
                .store
                .get_application(app_id)
                .await?
                .map(|app| app.lifecycle_id))
        })
        .await?;
        use sha2::Digest as _;
        let fingerprint = hex::encode(sha2::Sha256::digest(
            shared_types::encode_userapp_intent(&request).map_err(|error| {
                AppOperationError::Backend(format!("Encode runtime activation intent: {error}"))
            })?,
        ));
        if restart
            && let Some(replayed) = crate::service::restart_wait::prepare(self.replay_control(
                app_id,
                &request,
                kind,
                &fingerprint,
            ))
            .await?
        {
            return self
                .get_app(app_id)
                .await
                .map_err(|source| AppOperationError::Operation {
                    operation_id: replayed.operation_id,
                    source: Box::new(source),
                });
        }
        loop {
            // Check priority intent before waiting on the local process guard. A
            // completed Stop permits an explicit start, an active Stop never queues it.
            if !restart {
                self.metadata
                    .store
                    .check_compute_access(app_id, shared_types::UserAppOperationScope::Prod, true)
                    .await?;
            }
            // Restart shares one bounded pre-admission deadline across intent
            // and physical ownership; Start keeps its existing queue semantics.
            let guard = if let Some(deadline) = wait_deadline {
                self.acquire_restart_admission_guard(
                    app_id,
                    captured_lifecycle.as_deref(),
                    deadline,
                )
                .await?
            } else {
                self.acquire_process_release_lock(app_id).await?
            };
            let result = async {
                crate::service::restart_wait::prepare(
                    self.metadata.validate_request_lifecycle(
                        app_id,
                        captured_lifecycle
                            .as_deref()
                            .or(request.lifecycle_id.as_deref()),
                    ),
                )
                .await?;
                use sha2::Digest as _;
                let fingerprint = hex::encode(sha2::Sha256::digest(
                    shared_types::encode_userapp_intent(&request).map_err(|error| {
                        AppOperationError::Backend(format!(
                            "Encode runtime activation intent: {error}"
                        ))
                    })?,
                ));
                if let Some(replayed) = crate::service::restart_wait::prepare(self.replay_control(
                    app_id,
                    &request,
                    kind,
                    &fingerprint,
                ))
                .await?
                {
                    return self.get_app(app_id).await.map_err(|source| {
                        AppOperationError::Operation {
                            operation_id: replayed.operation_id,
                            source: Box::new(source),
                        }
                    });
                }
                let previous =
                    crate::service::restart_wait::prepare(self.fetch_runtime_status_or_err(app_id))
                        .await?;
                let mut operation = crate::service::OwnedOperation::admit(
                    self.metadata.store.clone(),
                    shared_types::UserAppAdmission {
                        runtime_policy_on_success: None,
                        command: Some(if restart {
                            shared_types::UserAppControlCommand::Restart
                        } else {
                            shared_types::UserAppControlCommand::Start { traffic: false }
                        }),
                        app_id: app_id.into(),
                        lifecycle_id: request.lifecycle_id.clone(),
                        request_id: request.request_id.clone(),
                        operation_id: uuid::Uuid::new_v4().to_string(),
                        kind,
                        request_fingerprint: fingerprint,
                        metadata: None,
                    },
                )
                .await?;
                let mutation = async {
                    if restart {
                        self.metadata
                            .store
                            .check_compute_access(
                                app_id,
                                shared_types::UserAppOperationScope::Prod,
                                true,
                            )
                            .await?;
                    }
                    operation.bind_lease(&guard).await?;
                    let context = operation.execution_context();
                    let target = self
                        .capture_bound_app_target(&context, previous.resource_version.as_deref())
                        .await?;
                    operation
                        .checkpoint(
                            if restart {
                                "restarting_runtime"
                            } else {
                                "starting_runtime"
                            },
                            serde_json::json!({"target":target}),
                        )
                        .await?;
                    operation.authorize_mutation().await?;
                    guard.mark_mutating()?;
                    let restart_image = crate::runtime::params::platform_restart_image(
                        &std::env::var("RCODER_RUNTIME_IMAGE_DIGEST").ok(),
                    );
                    let result = if restart {
                        self.runtime
                            .restart_app_target(&target, restart_image.as_deref())
                            .await
                    } else {
                        self.runtime
                            .start_app_target_with_image(&target, restart_image.as_deref())
                            .await
                    };
                    result.map_err(|error| {
                        map_runtime_mutation_error(
                            "container_start",
                            "Activate captured application",
                            error,
                        )
                    })
                }
                .await;
                let accepted_operation_id = operation.execution_context().operation_id;
                match mutation {
                    Ok(()) => {
                        self.refresh_pingora_after_restart(app_id).await;
                        operation.confirm_effects().await?;
                        operation.succeed().await?;
                        guard.mark_completed();
                    }
                    Err(error) => {
                        let error = operation.correlate_error(error);
                        if guard.has_unfinished_mutation() {
                            operation.fail(&error).await?;
                        } else {
                            operation.reject_without_mutation(&error).await?;
                        }
                        return Err(error);
                    }
                }
                self.activity.mark_running(app_id);
                self.get_app(app_id)
                    .await
                    .map_err(|source| AppOperationError::Operation {
                        operation_id: accepted_operation_id,
                        source: Box::new(source),
                    })
            }
            .await;
            if result.is_ok() || !guard.has_unfinished_mutation() {
                if let Some(deadline) = wait_deadline
                    && result
                        .as_ref()
                        .is_err_and(|error| error.operation_id().is_none())
                {
                    guard.finish_unadmitted(deadline).await?;
                } else {
                    guard.finish().await?;
                }
            }
            if let (Some(deadline), Err(error)) = (wait_deadline, &result)
                && error.code() == shared_types::ERR_OPERATION_IN_PROGRESS
                && error.operation_id().is_none()
            {
                self.wait_restart_blocker(
                    app_id,
                    result.err().ok_or_else(|| {
                        AppOperationError::Backend("Restart admission rejection disappeared".into())
                    })?,
                    deadline,
                )
                .await?;
                continue;
            }
            return result;
        }
    }

    /// 停止应用（scale replicas = 0）。拍板 2026-09-23：手动 stop 与闲置回收
    /// 统一——停止后流量即唤醒；参数仅区分外部快失败与回收器排队两种受理模式。
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.scale_to_zero(app_id, false).await
    }

    /// 闲置回收使用：排队受理（等锁）的 scale0。
    #[instrument(skip(self))]
    pub async fn recycle_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.scale_to_zero(app_id, true).await
    }

    async fn scale_to_zero(
        &self,
        app_id: &str,
        wake_on_traffic: bool,
    ) -> AppResult<AppRuntimeInfo> {
        let identity = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
            })?;
        self.scale_to_zero_controlled(
            app_id,
            shared_types::UserAppControlRequest {
                // Only the internal recycler supplies the authoritative current token.
                lifecycle_id: wake_on_traffic.then_some(identity.lifecycle_id),
                request_id: None,
            },
            wake_on_traffic,
        )
        .await
    }

    pub async fn stop_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<AppRuntimeInfo> {
        self.scale_to_zero_controlled(app_id, request, false).await
    }

    async fn scale_to_zero_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
        wake_on_traffic: bool,
    ) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        // 外部 stop 快失败：锁被进行中操作（start 等就绪可达数分钟）持有时立即
        // Conflict 让调用方稍后重试，不排队占用调用方连接；内部回收器
        // （wake_on_traffic=true）保持排队——周期扫描的清理动作，等一下无妨。
        let operation = if wake_on_traffic {
            self.acquire_process_release_lock(app_id).await?
        } else {
            self.try_acquire_process_release_lock(app_id).await?
        };
        let result = async {
            self.metadata
                .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
                .await?;
            use sha2::Digest as _;
            let fingerprint = hex::encode(sha2::Sha256::digest(
                shared_types::encode_userapp_intent(
                    &serde_json::json!({"request":request,"wake_on_traffic":wake_on_traffic}),
                )
                .map_err(|error| {
                    AppOperationError::Backend(format!("Encode stop intent: {error}"))
                })?,
            ));
            if let Some(replayed) = self
                .replay_control(
                    app_id,
                    &request,
                    shared_types::UserAppOperationKind::Stop,
                    &fingerprint,
                )
                .await?
            {
                return self
                    .get_app(app_id)
                    .await
                    .map_err(|source| AppOperationError::Operation {
                        operation_id: replayed.operation_id,
                        source: Box::new(source),
                    });
            }
            let previous = self.fetch_runtime_status_or_err(app_id).await?;
            let mut durable = crate::service::OwnedOperation::admit(
                self.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    runtime_policy_on_success: None,
                    command: Some(shared_types::UserAppControlCommand::Stop { wake_on_traffic }),
                    app_id: app_id.into(),
                    lifecycle_id: request.lifecycle_id.clone(),
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    request_id: request.request_id.clone(),
                    request_fingerprint: fingerprint,
                    kind: shared_types::UserAppOperationKind::Stop,
                    metadata: None,
                },
            )
            .await?;
            let mutation = async {
                durable.bind_lease(&operation).await?;
                let context = durable.execution_context();
                let target = self
                    .capture_bound_app_target(&context, previous.resource_version.as_deref())
                    .await?;
                durable
                    .checkpoint(
                        "stopping_runtime",
                        serde_json::json!({"target":target,"wake_on_traffic":wake_on_traffic}),
                    )
                    .await?;
                durable.authorize_mutation().await?;
                self.apply_scale_zero(&target, wake_on_traffic, &previous, &operation)
                    .await
            }
            .await;
            let accepted_operation_id = durable.execution_context().operation_id;
            match mutation {
                Ok(()) => {
                    durable.confirm_effects().await?;
                    durable.succeed().await?;
                    operation.mark_completed();
                }
                Err(error) => {
                    let error = durable.correlate_error(error);
                    if operation.has_unfinished_mutation() {
                        durable.fail(&error).await?;
                    } else {
                        durable.reject_without_mutation(&error).await?;
                    }
                    return Err(error);
                }
            }
            self.get_app(app_id)
                .await
                .map_err(|source| AppOperationError::Operation {
                    operation_id: accepted_operation_id,
                    source: Box::new(source),
                })
        }
        .await;
        if result.is_ok() || !operation.has_unfinished_mutation() {
            operation.finish().await?;
        }
        result
    }

    pub(crate) async fn apply_scale_zero(
        &self,
        target: &shared_types::UserAppMutationTarget,
        wake_on_traffic: bool,
        previous: &DeploymentStatus,
        operation: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        let app_id = &target.context.app_id;
        operation.mark_mutating()?;
        // 拍板 2026-09-23：手动 stop 与闲置回收统一——scale0 后一律可被
        // 流量唤醒；wake_on_traffic 参数只决定锁模式与注解/命令记录值。
        self.activity.mark_stopped(app_id);
        if let Err(error) = self.runtime.stop_app_target(target, wake_on_traffic).await {
            // This target operation makes exactly one remote stop/scale request.
            // Only a definitive rejection proves that this stop had no effects.
            // Timeout/disconnection labels and server errors retain uncertainty
            // even when a runtime presents them in a rejection envelope.
            if matches!(
                error,
                container_runtime_api::ContainerRuntimeError::RequestRejected(_)
            ) && !container_runtime_api::runtime_mutation_outcome_unknown(&error)
            {
                operation.mark_rejected_before_mutation();
                self.restore_activity_state(app_id, previous);
            }
            // Do not issue a compensating name-based patch after an uncertain
            // response or version conflict. It could modify a replacement.
            // Recovery resolves an uncertain stop outcome from durable state.
            return Err(map_runtime_mutation_error(
                "container_stop",
                "Stop captured application",
                error,
            ));
        }
        info!(app_id, operation_id = %target.context.operation_id, "Application stopped using captured resource identity");
        Ok(())
    }

    pub(crate) fn restore_activity_state(&self, app_id: &str, previous: &DeploymentStatus) {
        if previous.replicas > 0 {
            self.activity.mark_running(app_id);
        } else {
            self.activity.mark_stopped(app_id);
        }
    }

    /// 重启应用（rollout restart）
    #[instrument(skip(self))]
    pub async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        let _identity = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
            })?;
        self.restart_app_controlled(
            app_id,
            shared_types::UserAppControlRequest {
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
    }
}

/// 校验 recycle-policy 请求至少带一个字段(纯函数,便于单测)。
pub(super) fn validate_recycle_policy_fields(
    recycle_enabled: Option<bool>,
    idle_timeout_seconds: Option<u64>,
    wake_on_traffic: Option<bool>,
) -> AppResult<()> {
    if recycle_enabled.is_none() && idle_timeout_seconds.is_none() && wake_on_traffic.is_none() {
        return Err(AppOperationError::Validation(
            "recycle-policy requires at least one of recycle_enabled / idle_timeout_seconds / wake_on_traffic"
                .to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycle_policy_requires_at_least_one_field() {
        // 三字段皆 None → Fail Fast
        assert!(validate_recycle_policy_fields(None, None, None).is_err());
        // 任一 Some → Ok
        assert!(validate_recycle_policy_fields(Some(true), None, None).is_ok());
        assert!(validate_recycle_policy_fields(Some(false), None, None).is_ok());
        assert!(validate_recycle_policy_fields(None, Some(60), None).is_ok());
        assert!(validate_recycle_policy_fields(None, None, Some(false)).is_ok());
        assert!(validate_recycle_policy_fields(Some(true), Some(60), Some(true)).is_ok());
    }
}
