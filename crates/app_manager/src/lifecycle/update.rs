//! Userapp desired-state 变更面（从 service.rs 拆出，extension-impl）。
//!
//! Configuration update admission, runtime application, and outcome persistence.

use std::sync::Arc;

use tracing::{debug, info, instrument};

use container_runtime_api::{ExposeType as RtExposeType, StorageResizeOutcome};

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    /// 更新应用配置
    /// 更新应用（v2 §5.2，全量替换 desired state）。
    ///
    /// rcoder 无状态：不持有旧 desired state，故本操作为**全量替换**——调用方需发送完整
    /// 新状态（`image` 必填）。K8s 走 SSA re-apply（幂等）+ orphan 端口/配置清理；
    /// Docker 重建容器（image/env/command 变化必须重建），工作空间目录保留。
    #[instrument(skip(self, request))]
    pub async fn update_app(
        &self,
        app_id: &str,
        request: UpdateAppRequest,
    ) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        if let Some(env) = &request.env {
            crate::release_flow::identity::ensure_business_env(env)?;
        }
        if let Some(secrets) = &request.secrets {
            crate::release_flow::identity::ensure_business_env(secrets)?;
        }

        // 与发布串行（同 create/delete 的 per-app 进程级发布锁），但**不排队傻等**——
        // activate 等就绪可达 30 分钟，update 等它没有意义；锁被占（发布进行中）立即
        // 409 让调用方稍后重试。stop/restart/delete 外部控制路径同为快失败语义；
        // start（无 url）与内部回收器保持排队等待。
        // 无并发发布时锁条目可能不存在 → entry 建立并立刻拿到（try 必成功）。
        let lock_arc = match self.release_locks.entry(app_id.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                entry.insert(lock.clone());
                lock
            }
        };
        let _update_lock = lock_arc.try_lock_owned().map_err(|_| {
            AppOperationError::Conflict(format!(
                "app {app_id} is being activated/published, retry after it finishes"
            ))
        })?;
        let _update_lock = self.operation_guard(app_id, _update_lock, false).await?;
        let result = self
            .update_app_with_guard(app_id, request, &_update_lock)
            .await;
        if result.is_ok() || !_update_lock.has_unfinished_mutation() {
            _update_lock.finish().await?;
        }
        result?;
        self.get_app(app_id).await
    }

    /// Trusted deployment assembly calls this while retaining the application lease
    /// through stage confirmation. This method never re-enters the operation lock.
    pub(crate) async fn update_app_with_guard(
        &self,
        app_id: &str,
        request: UpdateAppRequest,
        _update_lock: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        self.metadata
            .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
            .await?;
        use sha2::Digest as _;
        let fingerprint = hex::encode(sha2::Sha256::digest(
            shared_types::encode_userapp_intent(&request).map_err(|error| {
                AppOperationError::Backend(format!("Encode update intent: {error}"))
            })?,
        ));
        if let Some(request_id) = &request.request_id
            && let Some(previous) = self
                .metadata
                .store
                .get_operation_by_request(app_id, request_id)
                .await?
        {
            if previous.kind != shared_types::UserAppOperationKind::Update
                || previous.request_fingerprint != fingerprint
            {
                return Err(AppOperationError::Conflict(
                    "Request identity was reused with different parameters".into(),
                ));
            }
            let identity = self
                .metadata
                .store
                .get_application(app_id)
                .await?
                .ok_or_else(|| {
                    AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
                })?;
            if previous.lifecycle_id != identity.lifecycle_id {
                return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
            }
            return match previous.state {
                shared_types::UserAppOperationState::Succeeded => Ok(()),
                shared_types::UserAppOperationState::Failed => {
                    Err(AppOperationError::Backend(format!(
                        "Application operation {} failed: {}",
                        previous.operation_id,
                        previous
                            .error_message
                            .as_deref()
                            .unwrap_or("No failure details recorded")
                    )))
                }
                _ => Err(AppOperationError::Conflict(format!(
                    "Application operation {} is not complete ({:?})",
                    previous.operation_id, previous.state
                ))),
            };
        }
        let current = self.fetch_runtime_status_or_err(app_id).await?;
        // 乐观锁：expected_resource_version 不匹配 → 409 Conflict
        // （Docker resource_version=None → 跳过校验，开发环境 last-write-wins 可接受）
        if let Some(expected) = &request.expected_resource_version
            && let Some(actual) = &current.resource_version
            && expected != actual
        {
            let error = AppOperationError::Conflict(format!(
                "resource version mismatch: expected={expected}, actual={actual}"
            ));
            _update_lock.mark_completed();
            return Err(error);
        }
        let params = self
            .build_container_params_from_update(app_id, &request, &current)
            .await?;
        // storage 扩容前置（pingora unregister 之前——失败零副作用直接返回）：
        // K8s 下 resources.storage 是 per-app PVC 扩容目标（仅扩不缩、在线生效
        // 不重建 Pod）；Docker no-op。失败阻断整个 update——该字段对外承诺生效，
        // 静默降级会让调用方以为已扩容。
        let identity = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
            })?;
        let input = super::config_input::encode(&params, Some(&current))?;
        let mut operation = crate::service::OwnedOperation::admit_with_input(
            self.metadata.store.clone(),
            shared_types::UserAppAdmission {
                runtime_policy_on_success: Some(shared_types::UserAppRuntimePolicy {
                    recycle_enabled: params.recycle_enabled,
                    idle_timeout_seconds: params.idle_timeout_seconds,
                    wake_on_traffic: current.wake_on_traffic,
                }),
                command: Some(shared_types::UserAppControlCommand::Update {
                    input_digest: input.digest(),
                }),
                app_id: app_id.into(),
                lifecycle_id: request.lifecycle_id.clone(),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: request.request_id.clone(),
                request_fingerprint: fingerprint,
                kind: shared_types::UserAppOperationKind::Update,
                metadata: Some(shared_types::UserAppMetadataPatch {
                    app_id: app_id.into(),
                    lifecycle_id: identity.lifecycle_id,
                    expected_revision: identity.metadata_revision,
                    name: request.name.clone().map(Some),
                    tenant_id: request.tenant_id.clone().map(Some),
                    space_id: request.space_id.clone().map(Some),
                }),
            },
            Some(&input),
        )
        .await?;
        let result = self
            .execute_update(app_id, params, current, &mut operation, _update_lock)
            .await;
        match result {
            Ok(()) => {
                operation.succeed().await?;
                _update_lock.mark_completed();
                Ok(())
            }
            Err(error) => {
                if _update_lock.has_unfinished_mutation() {
                    operation.fail(&error).await?;
                } else {
                    operation.reject_without_mutation(&error).await?;
                }
                Err(error)
            }
        }
    }
    pub(crate) async fn execute_update(
        &self,
        app_id: &str,
        mut params: container_runtime_api::ContainerCreateParams,
        current: container_runtime_api::DeploymentStatus,
        operation: &mut crate::service::OwnedOperation,
        _update_lock: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        operation.bind_lease(_update_lock).await?;
        let observed = self.fetch_runtime_status_or_err(app_id).await?;
        if observed.resource_version != current.resource_version
            || observed.created_at != current.created_at
        {
            return Err(AppOperationError::Conflict(
                "Application update target changed after admission".into(),
            ));
        }
        let context = operation.execution_context();
        params.execution_context = Some(context.clone());
        params
            .validate_execution_context()
            .map_err(|error| map_runtime_error("Validate update execution", error))?;
        let target = self
            .runtime
            .capture_app_mutation_target(&context, current.resource_version.as_deref())
            .await
            .map_err(|error| map_runtime_error("Capture application update target", error))?;
        let storage_target = if params.storage_size.is_some() {
            self.runtime
                .capture_app_storage_resize(&context)
                .await
                .map_err(|error| map_runtime_error("Capture application storage resize", error))?
        } else {
            None
        };
        let mut checkpoint = serde_json::json!({"target":target,"storage_target":storage_target,"requested_storage_size":params.storage_size});
        operation
            .checkpoint("updating_runtime", checkpoint.clone())
            .await?;
        params.mutation_target = Some(target);
        let mut storage_changed = false;
        _update_lock.mark_mutating()?;
        if let Some(new_size) = params.storage_size.as_deref() {
            let resize = match &storage_target {
                Some(target) => {
                    self.runtime
                        .resize_app_storage_target(target, new_size)
                        .await
                }
                None => Ok(StorageResizeOutcome::Noop),
            };
            match resize {
                Ok(StorageResizeOutcome::Resized { from, to }) => {
                    storage_changed = true;
                    info!(
                        "[APP] storage resized app_id={app_id}: {from} -> {to} (external-resizer async)"
                    );
                }
                Ok(StorageResizeOutcome::AlreadyEqual) => {
                    info!(
                        "[APP] storage resize no-op app_id={app_id}: requested {new_size} equals current"
                    );
                }
                Ok(StorageResizeOutcome::Noop) => {
                    debug!(
                        "[APP] storage resize no-op (runtime without PVC capacity) app_id={app_id}: {new_size}"
                    );
                }
                Ok(StorageResizeOutcome::ShrinkRejected {
                    current: cur,
                    requested,
                }) => {
                    _update_lock.mark_completed();
                    return Err(AppOperationError::Validation(format!(
                        "K8s PVC supports expansion only: app {app_id} requested {requested} < current {cur}"
                    )));
                }
                Err(e) => {
                    return Err(map_runtime_error(
                        &format!("[APP] resize_app_storage failed app_id={app_id}"),
                        e,
                    ));
                }
            }
        }
        // 恢复依据先取出（unregister 会移除注册表条目）：pingora_ports 里的是当前
        // 实际生效的 Http 端口——比 current.ports 反推可靠（Docker 后端的状态 ports
        // 只含 TCP，反推恒空会让恢复分支注册了个寂寞）。
        let registered_http_ports = self.registered_http_ports(app_id);
        // 先注销旧 Pingora backend（K8s/Docker 都执行：Docker 旧 container_ip 失效；
        // K8s 下方按本次 http_ports 重新注册到 Service FQDN，注销-重注成对保证一致）。
        self.unregister_pingora_backends(app_id).await;
        // http_ports 在 move 前从 params 提取：优先本次回退后的完整 ports（live 回退
        // 后含全部端口的权威 desired）；读失败降级（params.ports=None）时退当前注册值。
        let http_ports: Vec<u16> = params
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter(|p| matches!(p.expose_type, RtExposeType::Http))
                    .map(|p| p.port)
                    .collect()
            })
            .unwrap_or_else(|| registered_http_ports.clone());
        let info = match self
            .runtime
            .patch_deployment_if_version(
                params,
                shared_types::AppMutationPrecondition {
                    resource_version: current.resource_version.clone(),
                },
            )
            .await
        {
            Ok(info) => info,
            Err(e) => {
                // patch 失败：Deployment 原样仍在运行，恢复 pingora 路由（对齐 delete_app
                // 的失败恢复分支）——否则应用还在跑但 /api/v1/userapp/proxy/app/prod/{id} 502，直到
                // 下次成功 update 或进程重启。
                let previous_host = current.pod_ip.clone().unwrap_or_default();
                self.register_pingora_backends(app_id, &registered_http_ports, &previous_host)
                    .await;
                if !storage_changed
                    && self.config.access_mode == crate::config::AppAccessMode::Docker
                    && matches!(
                        &e,
                        container_runtime_api::ContainerRuntimeError::PreparationFailed(_)
                    )
                {
                    _update_lock.mark_completed();
                }
                return Err(map_runtime_error(
                    &format!("[APP] patch_deployment failed app_id={app_id}"),
                    e,
                ));
            }
        };
        // 重新注册 Pingora backend（与上面 unregister 对称——否则部分更新会丢
        // Pingora 路由，app 经 /api/v1/userapp/proxy/app/prod/{id} 变 502）。
        // 注：register 在 K8s 模式并非 no-op，会把 backend 指到 Service FQDN（与 create 一致）。
        self.register_pingora_backends(app_id, &http_ports, &info.container_ip)
            .await;
        // Completion adds its result without discarding the captured mutation
        // and storage identities needed to audit or reconcile this operation.
        checkpoint["resource"] = serde_json::to_value(info).map_err(|error| {
            AppOperationError::Backend(format!("Encode updated resource identity: {error}"))
        })?;
        operation.checkpoint("runtime_updated", checkpoint).await?;
        info!("[APP] app updated: {}", app_id);
        self.remove_unused_process_release_lock(app_id);
        self.invalidate_deploy_cache().await;
        Ok(())
    }
}
