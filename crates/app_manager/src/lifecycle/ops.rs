//! Userapp 生命周期 + 观测操作（从 service.rs 拆出，extension-impl）。
//!
//! start/stop/restart/recycle + stats/events 观测委托（转调 ContainerRuntime）。

use tracing::{info, instrument, warn};

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
        self.activate_existing_runtime(app_id, request, true).await
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
        validate_app_id(app_id)?;
        self.discover_missing_identity(app_id).await?;
        self.verify_recovered_storage(app_id, shared_types::UserAppOperationScope::Prod)
            .await?;
        self.metadata
            .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
            .await?;
        // Check priority intent before waiting on the local process guard. A
        // completed Stop permits an explicit start, an active Stop never queues it.
        self.metadata
            .store
            .check_compute_access(app_id, shared_types::UserAppOperationScope::Prod, true)
            .await?;
        // restart 与带 url 的 deploy_controlled 一致：锁被占立即 Conflict；
        // start 保持排队——本身是等待型操作（等就绪数分钟），调用方宽超时
        // 预算覆盖排队。
        let guard = if restart {
            self.try_acquire_process_release_lock(app_id).await?
        } else {
            self.acquire_process_release_lock(app_id).await?
        };
        let result = async {
            self.metadata
                .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
                .await?;
            use sha2::Digest as _;
            let fingerprint = hex::encode(sha2::Sha256::digest(
                shared_types::encode_userapp_intent(&request).map_err(|error| {
                    AppOperationError::Backend(format!("Encode runtime activation intent: {error}"))
                })?,
            ));
            if self
                .replay_control(app_id, &request, kind, &fingerprint)
                .await?
                .is_some()
            {
                return self.get_app(app_id).await;
            }
            let previous = self.fetch_runtime_status_or_err(app_id).await?;
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
                result.map_err(|error| map_runtime_error("Activate captured application", error))
            }
            .await;
            match mutation {
                Ok(()) => {
                    self.refresh_pingora_after_restart(app_id).await;
                    operation.confirm_effects().await?;
                    operation.succeed().await?;
                    guard.mark_completed();
                }
                Err(error) => {
                    if guard.has_unfinished_mutation() {
                        operation.fail(&error).await?;
                    } else {
                        operation.reject_without_mutation(&error).await?;
                    }
                    return Err(error);
                }
            }
            self.activity.mark_running(app_id);
            self.get_app(app_id).await
        }
        .await;
        if result.is_ok() || !guard.has_unfinished_mutation() {
            guard.finish().await?;
        }
        result
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
            if self
                .replay_control(
                    app_id,
                    &request,
                    shared_types::UserAppOperationKind::Stop,
                    &fingerprint,
                )
                .await?
                .is_some()
            {
                return self.get_app(app_id).await;
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
            match mutation {
                Ok(()) => {
                    durable.confirm_effects().await?;
                    durable.succeed().await?;
                    operation.mark_completed();
                }
                Err(error) => {
                    if operation.has_unfinished_mutation() {
                        durable.fail(&error).await?;
                    } else {
                        durable.reject_without_mutation(&error).await?;
                    }
                    return Err(error);
                }
            }
            self.get_app(app_id).await
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
            // A structured rejection therefore proves this stop had no effects.
            if matches!(
                error,
                container_runtime_api::ContainerRuntimeError::RequestRejected(_)
            ) {
                operation.mark_rejected_before_mutation();
                self.restore_activity_state(app_id, previous);
            }
            // Do not issue a compensating name-based patch after an uncertain
            // response or version conflict. It could modify a replacement.
            // Recovery resolves an uncertain stop outcome from durable state.
            return Err(map_runtime_error("Stop captured application", error));
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

    /// 获取资源使用情况（app_stage 分派：prod=运行容器 label 查询；dev=开发容器
    /// 双键 selector——instance+service-type，K8s 专属）。
    ///
    /// CPU/内存用量 + 限额来自运行时（K8s = metrics.k8s.io PodMetrics + pod limits；Docker 默认 0），
    /// 百分比 = usage/limit×100（limit=0 → 0）。restart_count 来自 Deployment 状态
    /// （dev 形态为 STS 容器，restart 计数无对应视图 → 取 dev_container_alive 探活结果粗略映射 0/自身不计）。
    /// network（rx/tx）metrics.k8s.io 不提供，留 0。运行时用量查询失败降级为 0（不 500）。
    #[instrument(skip(self))]
    pub async fn get_app_stats(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<ResourceStats> {
        use shared_types::UserappStage;
        validate_app_id(app_id)?;
        if app_stage == UserappStage::Dev {
            return self.get_dev_stats(app_id).await;
        }
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        let restart_count = status.restart_count;
        let usage = match self.runtime.get_app_resource_usage(app_id).await {
            Ok(u) => u,
            Err(e) => {
                warn!(
                    "[APP] get_app_resource_usage failed app_id={app_id}: {e} (stats fallback to zero)"
                );
                Default::default()
            }
        };
        Ok(Self::resource_stats_from(usage, restart_count))
    }

    /// 开发容器资源统计：`get_app_resource_usage_for(UserappBuilder)` 双键定位。
    /// 用量降级语义与 prod 相同；dev builder 常驻自愈，restart 视图不存在 → 0。
    async fn get_dev_stats(&self, app_id: &str) -> AppResult<ResourceStats> {
        let usage = match self
            .runtime
            .get_app_resource_usage_for(app_id, &shared_types::ServiceType::UserappBuilder)
            .await
        {
            Ok(u) => u,
            Err(e) => {
                warn!(
                    "[APP] dev resource usage failed app_id={app_id}: {e} (stats fallback to zero)"
                );
                Default::default()
            }
        };
        Ok(Self::resource_stats_from(usage, 0))
    }

    fn resource_stats_from(
        usage: container_runtime_api::ResourceUsage,
        restart_count: u32,
    ) -> ResourceStats {
        let cpu_percent = if usage.cpu_limit_cores > 0.0 {
            (usage.cpu_usage_cores / usage.cpu_limit_cores * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        let mem_percent = if usage.mem_limit_bytes > 0 {
            usage.mem_usage_bytes as f64 / usage.mem_limit_bytes as f64 * 100.0
        } else {
            0.0
        };
        ResourceStats {
            restart_count,
            cpu: CpuStats {
                usage_cores: usage.cpu_usage_cores,
                limit_cores: usage.cpu_limit_cores,
                usage_percent: cpu_percent,
            },
            memory: MemoryStats {
                usage_bytes: usage.mem_usage_bytes,
                limit_bytes: usage.mem_limit_bytes,
                usage_percent: mem_percent,
            },
            network: NetworkStats::default(),
        }
    }

    /// 获取应用健康状态（app_stage 分派）：
    /// - prod：实时集群查询派生（`AppRuntimeInfo.health`）
    /// - dev：探活开发容器内 file-server `/health`（经 `UserappDevLocator`
    ///   幂等 ensure+探活自愈定位）；2xx→Running / 其余→Unhealthy
    #[instrument(skip(self))]
    pub async fn get_app_health(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<HealthInfo> {
        validate_app_id(app_id)?;
        if app_stage == shared_types::UserappStage::Prod {
            let runtime = self.get_app(app_id).await?;
            return Ok(runtime.health);
        }
        // health 不在接口面收 user_id（⚪/dev🟢 不补参）——传 None 走 metadata
        // owner 链（ensure 侧取值链自降级，无需此处预查）
        let base = self.app_files_base(app_stage, app_id).await?;
        let ok = reqwest::Client::new()
            .get(format!("{base}/health"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        Ok(HealthInfo {
            status: if ok { "Running" } else { "Unhealthy" }.to_string(),
            instance: None,
            probes: None,
        })
    }

    /// 日志转发基址。dev 只读定位已有 file-server，业务与 owner 未运行也可查。
    /// prod=唤醒后运行实例 IP
    /// （读日志是使用语义，闲置回收的 stopped 容器自动拉起——与文件族
    /// `app_files_base` prod 分支同款 wake）；dev=从
    /// `UserappDevLocator.dev_logs_file_server_addr`，不唤醒或创建开发容器。
    #[instrument(skip(self))]
    pub async fn log_api_base(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<String> {
        validate_app_id(app_id)?;
        if app_stage == shared_types::UserappStage::Prod {
            // 权威存在性检查前移：不存在的 app 直接 ERR_APP_NOT_FOUND。唤醒
            // 协调器现在会校验权威身份，对无记录 app 返回 Failed（runtime
            // 缺失），若先唤醒会把"应用不存在"吞成可重试的唤醒失败。
            self.get_app(app_id).await?;
            use shared_types::AppWakeControl;
            match self.activity.ensure_running(app_id).await {
                shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
                shared_types::WakeOutcome::Timeout => {
                    return Err(AppOperationError::InvalidState(format!(
                        "app {app_id} wake timed out; retry later"
                    )));
                }
                shared_types::WakeOutcome::Blocked { message, blocker } => {
                    return Err(AppOperationError::ConflictBlocked { message, blocker });
                }
                shared_types::WakeOutcome::Failed(e) => {
                    return Err(AppOperationError::InvalidState(format!(
                        "app {app_id} wake failed: {e}"
                    )));
                }
            }
            let runtime = self.get_app(app_id).await?;
            let ip = runtime
                .health
                .instance
                .map(|instance| instance.ip)
                .filter(|ip| !ip.is_empty())
                .ok_or_else(|| {
                    AppOperationError::InvalidState(format!(
                        "app {app_id} has no ready runtime IP for log access"
                    ))
                })?;
            return Ok(format!("http://{ip}:{}", shared_types::APP_CLI_ADMIN_PORT));
        }
        let locator = self
            .dev_locator
            .read()
            .map_err(|error| AppOperationError::Backend(format!("read dev log locator: {error}")))?
            .clone()
            .ok_or_else(|| AppOperationError::Backend("dev log locator is unavailable".into()))?;
        locator
            .dev_logs_file_server_addr(app_id)
            .await
            .map_err(|error| {
                AppOperationError::Backend(format!(
                    "locate development logs for app {app_id}: {error}"
                ))
            })
    }

    /// 获取应用事件（K8s Events API：调度/拉取/启动/崩溃）
    #[instrument(skip(self))]
    pub async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime.get_app_events(app_id).await.map_err(|e| {
            map_runtime_error(&format!("[APP] get_app_events failed app_id={app_id}"), e)
        })
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
