//! Userapp 创建链（从 service.rs 拆出，extension-impl）。
//!
//! create_app + validate/provision/runtime/assemble 创建流水。

use chrono::Utc;
use tracing::{info, instrument};
use uuid::Uuid;

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    /// 创建应用（公共入口：自动获取进程级发布锁）
    ///
    /// ⚠️ 调用方不得已持 `acquire_process_release_lock(app_id)` —— tokio Mutex
    /// 不可重入，已持锁调用须使用共享执行内核，不能再次调用公共入口。
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> AppResult<AppInfo> {
        // ⚠️ 调用方不得已持 `acquire_process_release_lock(app_id)` —— tokio Mutex
        // Non-reentrant: callers holding the application guard use the execution kernel.
        //
        // Multi-replica creation is resolved at the runtime create boundary.
        // Never compensate by the logical name: another replica may own that resource.
        let app_id = self.validate_create_request(&request).await?;
        // 与发布流水线/delete 串行: 防发布流水线 EnsureApp 建 Deployment 与并发
        // DELETE 互踩 (删成功但 Deployment 复活/半删半建脏状态)。
        let process_lock = self.acquire_process_release_lock(&app_id).await?;
        let result = self.create_app_locked(&app_id, request, process_lock).await;
        if result.is_ok() {
            self.invalidate_deploy_cache().await;
        }
        result
    }

    /// 已持锁内核：调用方持有该 app 的进程级发布锁（防止与发布流水线互踩），
    /// 本函数不再取锁；公共 create 入口将已取得的锁传入，避免重入。
    ///
    /// 入口统一解析默认镜像（单一收口）：image 缺省 → 平台默认运行时镜像
    /// （env `RCODER_RUNTIME_IMAGE_DIGEST`，测试/生产由部署注入；与发布链
    /// empty_runtime_request 同源）。填充后全链路（params/AppInfo）恒 Some。
    pub(crate) async fn create_app_locked(
        &self,
        app_id: &str,
        request: CreateAppRequest,
        _process_lock: crate::service::AppOperationGuard,
    ) -> AppResult<AppInfo> {
        let result = self
            .create_app_with_guard(app_id, request, &_process_lock)
            .await;
        if result.is_ok() || !_process_lock.has_unfinished_mutation() {
            _process_lock.finish().await?;
        }
        result
    }

    pub(crate) async fn create_app_with_guard(
        &self,
        app_id: &str,
        request: CreateAppRequest,
        _process_lock: &crate::service::AppOperationGuard,
    ) -> AppResult<AppInfo> {
        // 默认镜像单一收口（见函数 doc）：填充后 params/AppInfo 全链路恒 Some
        let mut request = request;
        if request
            .image
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            request.image = Some(crate::runtime::params::default_runtime_image(
                &std::env::var("RCODER_RUNTIME_IMAGE_DIGEST").ok(),
            )?);
        }
        info!(
            "[APP] creating app: {} ({}, mode={:?})",
            request.name, app_id, self.config.access_mode
        );
        self.metadata
            .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
            .await?;
        use sha2::Digest as _;
        let fingerprint = hex::encode(sha2::Sha256::digest(
            shared_types::encode_userapp_intent(&request).map_err(|error| {
                AppOperationError::Backend(format!("Encode creation intent: {error}"))
            })?,
        ));
        let control = shared_types::UserAppControlRequest {
            lifecycle_id: request.lifecycle_id.clone(),
            request_id: request.request_id.clone(),
        };
        // The first registration is read-only with respect to runtime resources.
        // Metadata changes and operation admission below commit atomically.
        let identity = self.metadata.store.ensure_identity(app_id).await?;
        self.activity
            .bind_lifecycle(app_id, &identity.lifecycle_id, identity.lifecycle_epoch);
        if self
            .replay_control(
                app_id,
                &control,
                shared_types::UserAppOperationKind::Create,
                &fingerprint,
            )
            .await?
            .is_some()
        {
            return Ok(self.assemble_app_info(app_id.to_owned(), request).await);
        }
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(_)) => {
                return Err(AppOperationError::AlreadyExists(format!(
                    "Application already exists: {app_id}"
                )));
            }
            Ok(None) => {}
            Err(error) => return Err(map_runtime_error("Read application creation target", error)),
        }
        let params = self.build_container_params(app_id, &request).await?;
        let input = super::config_input::encode(&params, None)?;
        let mut operation = crate::service::OwnedOperation::admit_with_input(
            self.metadata.store.clone(),
            shared_types::UserAppAdmission {
                runtime_policy_on_success: Some(shared_types::UserAppRuntimePolicy {
                    recycle_enabled: params.recycle_enabled,
                    idle_timeout_seconds: params.idle_timeout_seconds,
                    wake_on_traffic: Some(true),
                }),
                command: Some(shared_types::UserAppControlCommand::Create {
                    input_digest: input.digest(),
                }),
                app_id: app_id.into(),
                lifecycle_id: request.lifecycle_id.clone(),
                operation_id: Uuid::new_v4().to_string(),
                request_id: request.request_id.clone(),
                request_fingerprint: fingerprint,
                kind: shared_types::UserAppOperationKind::Create,
                metadata: Some(shared_types::UserAppMetadataPatch {
                    app_id: app_id.into(),
                    lifecycle_id: identity.lifecycle_id,
                    expected_revision: identity.metadata_revision,
                    name: Some(Some(request.name.clone())),
                    tenant_id: request.tenant_id.clone().map(Some),
                    space_id: request.space_id.clone().map(Some),
                }),
            },
            Some(&input),
        )
        .await?;
        let mutation = self
            .execute_creation(app_id, params, &mut operation, _process_lock)
            .await;
        // Ownership persists if the remote result or terminal commit is uncertain.
        // The operation record remains available for identity-aware recovery.
        match mutation {
            Ok(()) => operation.succeed().await?,
            Err(error) => {
                if _process_lock.has_unfinished_mutation() {
                    operation.fail(&error).await?;
                } else {
                    operation.reject_without_mutation(&error).await?;
                }
                return Err(error);
            }
        }
        _process_lock.mark_completed();
        self.activity.mark_running(app_id);
        Ok(self.assemble_app_info(app_id.to_owned(), request).await)
    }

    pub(crate) async fn execute_creation(
        &self,
        app_id: &str,
        mut params: container_runtime_api::ContainerCreateParams,
        operation: &mut crate::service::OwnedOperation,
        guard: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        operation.bind_lease(guard).await?;
        params.execution_context = Some(operation.execution_context());
        params
            .validate_execution_context()
            .map_err(|error| map_runtime_error("Validate creation execution", error))?;
        if self
            .runtime
            .get_deployment_status(app_id)
            .await
            .map_err(|error| map_runtime_error("Observe creation recovery target", error))?
            .is_some()
        {
            return Err(AppOperationError::AlreadyExists(
                "Application creation target already exists".into(),
            ));
        }
        let ports = params
            .ports
            .as_ref()
            .map(|ports| {
                ports
                    .iter()
                    .filter(|port| {
                        matches!(port.expose_type, container_runtime_api::ExposeType::Http)
                    })
                    .map(|port| port.port)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        operation
            .checkpoint(
                "creating_runtime",
                serde_json::json!({"context":params.execution_context}),
            )
            .await?;
        operation.authorize_mutation().await?;
        guard.mark_mutating()?;
        let resource = match self.runtime.create_deployment(params).await {
            Ok(resource) => resource,
            Err(error) => {
                // 整操作级安全结束证明（创建编排层累计进度 + 逐请求确定性
                // 结果）：失败发生在首个 generation 写之前且保留物仅幂等类
                // （PVC ensure/claim 注解）→ 复位变更标记，外层既有分类走
                // reject_without_mutation 落 Failed 并释放围栏。证明不是
                // 零变更——保留资源显式记录在错误消息中（R04：不冒充）。
                let safe_note = error.creation_safe_finish_note();
                if safe_note.is_some() {
                    guard.mark_rejected_before_mutation();
                }
                let ctx = match &safe_note {
                    Some(note) => format!("Create application runtime (safe failure; {note})"),
                    None => "Create application runtime".to_string(),
                };
                return Err(map_runtime_error(&ctx, error));
            }
        };
        self.register_pingora_backends(app_id, &ports, &resource.container_ip)
            .await;
        operation
            .checkpoint("runtime_created", serde_json::json!({"resource":resource}))
            .await?;
        Ok(())
    }

    /// 校验创建请求并解析 app_id（app_id 规范 + 唯一性 + 资源格式 + 端口）。
    /// 任一校验失败 Fail Fast 返回 ERR_VALIDATION / ERR_APP_ALREADY_EXISTS。
    async fn validate_create_request(&self, request: &CreateAppRequest) -> AppResult<String> {
        // app_id：外部指定（小写字母数字 ≤22，如数值 project_id；禁 '-'；
        // 校验 + 唯一性）or 自动生成（app{8hex}）
        let app_id = match &request.app_id {
            Some(id) => {
                validate_app_id(id)?;
                id.clone()
            }
            None => format!("app{}", &Uuid::new_v4().to_string()[..8]),
        };

        // 资源限制格式（K8s Quantity: storage / ephemeral_storage）→ ERR_VALIDATION
        if let Some(ref resources) = request.resources {
            if let Some(ref s) = resources.storage {
                validate_k8s_storage_size(s).map_err(|e| {
                    AppOperationError::Validation(format!("invalid storage '{}': {}", s, e))
                })?;
            }
            if let Some(ref es) = resources.ephemeral_storage {
                validate_k8s_storage_size(es).map_err(|e| {
                    AppOperationError::Validation(format!(
                        "invalid ephemeral_storage '{}': {}",
                        es, e
                    ))
                })?;
            }
        }

        // 端口校验：HTTP 端口数上限放开（app-runtime 镜像单容器带 ttyd 7681 + dbx 4224 + 用户应用端口）
        // Pingora 免端口路由 /api/v1/userapp/proxy/app/prod 按 (app_id, APP_ENTRY_PORT) 优先（多 HTTP 端口仍全量注册）
        // gateway 模式（HTTPRoute）仍只支持单 HTTP，在 k8s_deployment 侧单独拦截（这里不拦，让 Pingora 模式可用）
        let http_port_count = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter(|p| p.expose_type == ExposeType::Http)
                    .count()
            })
            .unwrap_or(0);
        const MAX_HTTP_PORTS: usize = 8;
        if http_port_count > MAX_HTTP_PORTS {
            return Err(AppOperationError::Validation(format!(
                "at most {MAX_HTTP_PORTS} HTTP ports allowed (got {http_port_count})"
            )));
        }
        // 端口号唯一：避免 K8s annotation 解码歧义（同 port 不同 type 会被 HashMap 折叠）
        // 及 Pingora backend key(port) 冲突。Fail Fast 在源头拒绝。
        if let Some(ports) = &request.ports {
            let mut seen = std::collections::HashSet::new();
            for p in ports {
                if !seen.insert(p.port) {
                    return Err(AppOperationError::Validation(format!(
                        "port {} duplicate: each port number must be unique",
                        p.port
                    )));
                }
                // Service 端口保留名：ttyd 由平台恒补（运行容器终端的代理上游），
                // 用户占用会挤掉恒补暴露（K8s 端口名唯一）
                if p.name == "ttyd" {
                    return Err(AppOperationError::Validation(format!(
                        "port name '{}' is reserved for platform builtin services (ttyd=7681)",
                        p.name
                    )));
                }
            }
        }
        Ok(app_id)
    }

    /// 创建运行时资源：build params → create_deployment → 注册 Pingora backend。
    ///
    /// 注: Userapp 是新开发逻辑 (application-management-service-v2-design.md), /app 路径
    /// 不涉及历史数据迁移 → 不调 lazy_migrate (新应用无旧数据)。Web/Computer 有历史数据才调。
    /// 装配 AppInfo：实时查运行时状态，合并端口 external_port（K8s node_port），构建 access/health/status。
    ///
    /// status 用运行时 phase 映射（不再硬编码 Running）——刚创建的 Pod 通常还是 Starting，甚至镜像
    /// 拉取失败已 Error；返回真实状态避免"status=Running 但 health=Starting/Error"自相矛盾。
    async fn assemble_app_info(&self, app_id: String, request: CreateAppRequest) -> AppInfo {
        let runtime_status = self.fetch_runtime_status(&app_id).await;

        // 端口状态：以请求端口为准（expose_type 语义完整），合并运行时返回的 external_port（K8s node_port）。
        // Docker 模式 get_deployment_status 不还原端口语义，Tcp 的 host_port 留空（已知限制）。
        let mut ports: Vec<AppPortStatus> = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| AppPortStatus {
                        name: p.name.clone(),
                        port: p.port,
                        expose_type: map_expose_type(&p.expose_type),
                        external_port: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(status) = &runtime_status {
            for rt_p in &status.ports {
                let Some(ep) = rt_p.external_port else {
                    continue;
                };
                // 按 port 匹配 external_port（Docker get_deployment_status 的 name 是
                // tcp-{port}，与请求 name 不一致；port 唯一，K8s/Docker 通用）
                if let Some(ap) = ports.iter_mut().find(|p| p.port == rt_p.port) {
                    ap.external_port = Some(ep);
                }
            }
        }

        let access = self.build_access_info(&app_id, &ports);
        let health = runtime_status
            .as_ref()
            .map(health_from_status)
            .unwrap_or(HealthInfo {
                status: "Unknown".to_string(),
                instance: None,
                probes: None,
            });
        let (status, message) = match &runtime_status {
            Some(s) => (phase_to_status(&s.phase), s.message.clone()),
            None => (AppStatus::Starting, None),
        };

        let now = Utc::now().to_rfc3339();
        AppInfo {
            app_id,
            name: request.name,
            status,
            message,
            image: request.image.clone().unwrap_or_default(),
            command: request.command.unwrap_or_default(),
            replicas: 1,
            access,
            health,
            resources: request.resources,
            env: request.env.unwrap_or_default(),
            created_at: now.clone(),
            updated_at: now,
        }
    }
}
