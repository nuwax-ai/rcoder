//! 持久存储管理（query/clear/destroy storage + orphan 检测，`app_stage` 显式分派 dev/prod）
//!
//! RBD 卷形态（rcoder 零挂载）：`exists` = PVC 归属（K8s label 集）/目录存在
//! （Docker）；`path` = PVC 名（K8s，非可挂载路径）/bind 源目录（Docker）；
//! `modified_at` 仅 Docker 可得（K8s 需容器运行，降级 None）。
//! - `prod`：`clear` K8s = 删 PVC（数据清空语义等价——RBD 不可挂载无法逐文件清，
//!   卷在下次 create 自动重建）；Docker = 清目录内容。
//! - `dev`：`clear` = 经容器 file-server 清空 workspace 内容（留容器留卷——
//!   开发容器常驻，"重置开发工作区"语义）；`destroy` = UserappDevCleanup 四步
//!   回收整个开发环境（容器+PVC+目录+注册），不动 metadata。

use tracing::{info, warn};

use shared_types::ServiceType;
use shared_types::UserappStage;

use crate::models::*;
use crate::utils::*;

/// Keep captured local guards alive through the terminal SQL commit, including
/// failure commits. Moving execution to a helper must not shorten their lifetime.
#[derive(Default)]
pub(crate) struct StorageClearLeases {
    development: Option<Box<dyn shared_types::UserappDevDeletion>>,
    directories: Vec<shared_types::storage_contents::StorageDirectoryLease>,
}

/// app_stage → 卷形态的 ServiceType（K8s PVC label / Docker 目录树都按它分形）。
fn service_type_of(app_stage: UserappStage) -> ServiceType {
    match app_stage {
        UserappStage::Dev => ServiceType::UserappBuilder,
        UserappStage::Prod => ServiceType::Userapp,
    }
}

impl crate::service::AppService {
    // ===== 持久存储管理（v2 §5.4）=====
    // 删应用默认保留数据；这组接口让 Java 显式管理残留存储。
    // StorageInfo 不含 size_bytes——需容器运行时 exec du，跨面语义不稳（见设计文档 §5.4）。

    /// 查询单个应用的持久存储状态（prod=运行卷；dev=开发卷）。
    pub async fn get_app_storage(
        &self,
        app_stage: UserappStage,
        app_id: &str,
    ) -> AppResult<StorageInfo> {
        validate_app_id(app_id)?;
        // 权威生命周期记录缺失 = 应用从未存在（墓碑也保留 owner）：per-app
        // 存储只能随应用创建产生，直接判定不存在。存在记录但 owner 缺失仍走
        // 下游 InvalidState（异常数据不得静默）。存储读取错误照常上抛——
        // 只有权威"查无"能证明不存在，不能把存储故障当不存在。
        let authoritative_absent = self.metadata.lookup(app_id).await?.is_none();
        if authoritative_absent {
            return Ok(StorageInfo {
                app_id: app_id.to_string(),
                exists: false,
                path: String::new(),
                modified_at: None,
                is_orphan: false,
            });
        }
        let is_orphan = match app_stage {
            UserappStage::Prod => self.is_storage_orphan(app_id).await?,
            UserappStage::Dev => self.is_dev_storage_orphan(app_id).await?,
        };
        let (exists, path) = self.storage_path_info(app_stage, app_id).await?;
        let modified_at = if shared_types::is_kubernetes_runtime() {
            None // RBD 卷不可挂载，无路径视角；容器 exec stat 属运行时依赖，降级
        } else {
            tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        };
        Ok(StorageInfo {
            app_id: app_id.to_string(),
            exists,
            path: path.to_string_lossy().to_string(),
            modified_at,
            is_orphan,
        })
    }

    /// 存储标识 + 存在性（K8s：PVC 名 + label 集含否；Docker：bind 目录 + 本地 stat）。
    async fn storage_path_info(
        &self,
        app_stage: UserappStage,
        app_id: &str,
    ) -> AppResult<(bool, std::path::PathBuf)> {
        let service_type = service_type_of(app_stage);
        let path = self
            .runtime
            .workspace_volume_name(app_id, &service_type)
            .await
            .map_err(|e| map_runtime_error("[APP] workspace_volume_name failed", e))?;
        let path = std::path::PathBuf::from(path);
        if shared_types::is_kubernetes_runtime() {
            // PVC 归属 = label 集（list_workspace_identifiers 枚举 per-app PVC，
            // 含已 delete 的孤儿）；path 字段是 PVC 名而非可挂载路径
            let exists = self
                .runtime
                .list_workspace_identifiers(&service_type)
                .await
                .map_err(|e| map_runtime_error("[APP] list_workspace_identifiers failed", e))?
                .contains(&app_id.to_string());
            Ok((exists, path))
        } else {
            // Docker：workspace_volume_name 返回的是展示标识（通配串，非可 stat
            // 路径）——存在性用元数据 uid 精确定位对应树的 workspace 段。
            let ws_dir = match app_stage {
                UserappStage::Prod => self.app_prod_dirs(app_id).await?[0].clone(),
                UserappStage::Dev => self.app_dev_dirs(app_id).await?[0].clone(),
            };
            let exists = tokio::fs::try_exists(&ws_dir)
                .await
                .map_err(|error| map_io_error("inspect application storage", error, true))?;
            Ok((exists, path))
        }
    }

    /// 校验 app 计算资源已不存在（clear/destroy 共用前置：必须先 delete app，否则 INVALID_STATE）。
    /// `op` 用于错误描述（如 "clearing storage" / "destroying PVC"）。
    async fn ensure_app_deleted(&self, app_id: &str, op: &str) -> AppResult<()> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(_)) => Err(AppOperationError::InvalidState(format!(
                "app {app_id} still exists, delete it before {op}"
            ))),
            Ok(None) => Ok(()),
            Err(e) => {
                warn!("[APP] query app status failed app_id={}: {}", app_id, e);
                Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )))
            }
        }
    }

    /// per-app prod 四目录的 rcoder 容器内锚点路径（`{锚点}/prod/{user_id}/` 下
    /// `{app_id}/ + data/{app_id}/ + logs/{app_id}/ + agent-store/{app_id}/`，bind
    /// 双向同步宿主——Docker 模式 clear 的四目录定位；布局单一事实源
    /// [`shared_types::paths::userapp_prod_subpaths`]）。owner user_id 查元数据，
    /// Missing ownership is an error; never derive a destructive path from app_id as user_id.
    async fn app_prod_dirs(&self, app_id: &str) -> AppResult<[std::path::PathBuf; 4]> {
        let uid = self.app_owner_uid(app_id).await?;
        Ok(
            shared_types::paths::userapp_prod_subpaths(&uid, app_id).map(|sub| {
                std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT).join(sub)
            }),
        )
    }

    /// per-app dev 四目录的 rcoder 容器内锚点路径（`{锚点}/dev/{user_id}/` 下与
    /// prod 同构四段；布局单一事实源 [`shared_types::paths::userapp_dev_subpaths`]）。
    async fn app_dev_dirs(&self, app_id: &str) -> AppResult<[std::path::PathBuf; 4]> {
        let uid = self.app_owner_uid(app_id).await?;
        Ok(
            shared_types::paths::userapp_dev_subpaths(&uid, app_id).map(|sub| {
                std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT).join(sub)
            }),
        )
    }

    /// Resolve the stored owner; missing ownership cannot authorize storage access.
    async fn app_owner_uid(&self, app_id: &str) -> AppResult<String> {
        self.metadata
            .lookup(app_id)
            .await?
            .and_then(|r| r.user_id)
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| {
                AppOperationError::InvalidState("Application owner is unavailable".into())
            })
    }

    /// 清空应用持久存储内容（数据语义；卷对象去留按运行时能力与环境语义）。
    /// - `prod`：安全约束仅当 app 计算资源已不存在时允许（否则 INVALID_STATE）。
    ///   K8s/RBD：rcoder 不可挂载无法逐文件清 → 删 PVC（下次 create 自动重建空卷，
    ///   数据清空语义等价；handbook 注明）。Docker：清 prod 四目录内容（留目录本身）。
    /// - `dev`：经开发容器 file-server 清空 workspace 内容（**留容器留卷**——
    ///   "重置开发工作区"语义；开发容器常驻，卷重建要求先销毁容器，得不偿失）。
    ///   幂等；容器内为旧镜像（无 clear 端点）时 404 上抛 Backend。
    pub async fn clear_app_storage(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<()> {
        self.clear_app_storage_controlled(
            app_stage,
            app_id,
            ClearStorageRequest {
                user_id: user_id.into(),
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
        .map(|_| ())
    }

    /// Execute only an already admitted clear under the caller's operation lease.
    /// Recovery observes an existing builder; it never ensures a builder while
    /// its own clear operation occupies the application's durable operation slot.
    pub(crate) async fn execute_storage_clear(
        &self,
        app_id: &str,
        user_id: &str,
        production: bool,
        operation: &mut crate::service::OwnedOperation,
        guard: &crate::service::AppOperationGuard,
        leases: &mut StorageClearLeases,
    ) -> AppResult<()> {
        operation.bind_lease(guard, user_id).await?;
        let workspace_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|error| {
                AppOperationError::Backend(format!("Create direct workspace client: {error}"))
            })?;
        let target = if !production {
            let mut ticket = self.capture_dev_deletion(app_id).await?;
            let receipt = ticket.receipt();
            let context = operation.execution_context(user_id);
            let endpoint = ticket
                .workspace_endpoint(&context)
                .await
                .map_err(AppOperationError::Backend)?;
            let base_url = endpoint.base_url();
            let response = workspace_client
                .get(format!("{base_url}/api/v1/userapp/app-files/clear-target"))
                .query(&shared_types::UserAppWorkspaceClearProbe {
                    app_id: app_id.into(),
                    user_id: user_id.to_owned(),
                })
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .map_err(|error| {
                    AppOperationError::Backend(format!("Observe workspace clear target: {error}"))
                })?;
            let response =
                crate::ops::files::check_status(response, "observe-clear-target", app_id).await?;
            let target: shared_types::UserAppWorkspaceClearTarget =
                response.json().await.map_err(|error| {
                    AppOperationError::Backend(format!("Decode workspace clear target: {error}"))
                })?;
            if target.app_id != app_id || target.instance_id.is_empty() {
                return Err(AppOperationError::Conflict(
                    "Workspace clear target identity mismatch".into(),
                ));
            }
            // The HTTP probe must be bracketed by uncached observations
            // of the same physical pod/container, under the builder lease.
            if ticket
                .workspace_endpoint(&context)
                .await
                .map_err(AppOperationError::Backend)?
                != endpoint
            {
                return Err(AppOperationError::Conflict(
                    "Builder endpoint changed during workspace target observation".into(),
                ));
            }
            leases.development = Some(ticket);
            shared_types::UserAppStorageClearTarget::Development {
                base_url,
                instance_id: target.instance_id,
                endpoint,
                receipt: Box::new(receipt),
            }
        } else {
            self.ensure_app_deleted(app_id, "clearing storage").await?;
            let snapshot = self
                .runtime
                .capture_app_deletion(app_id, None)
                .await
                .map_err(|error| map_runtime_error("Capture storage clear resources", error))?;
            if self.config.access_mode == crate::config::AppAccessMode::Docker {
                for path in self.app_prod_dirs(app_id).await? {
                    leases.directories.push(
                        shared_types::storage_contents::StorageDirectoryLease::capture(&path)
                            .await
                            .map_err(|error| {
                                map_io_error("Capture storage directory", error, false)
                            })?,
                    );
                }
            }
            let directories = leases
                .directories
                .iter()
                .map(|lease| lease.receipt.clone())
                .collect();
            shared_types::UserAppStorageClearTarget::Production {
                snapshot,
                directories,
            }
        };
        let evidence = shared_types::UserAppStorageClear {
            context: operation.execution_context(user_id),
            target,
        };
        evidence.validate().map_err(AppOperationError::Conflict)?;
        let checkpoint = serde_json::to_value(&evidence).map_err(|error| {
            AppOperationError::Backend(format!("Encode storage clear target: {error}"))
        })?;
        operation
            .checkpoint("clear_target_captured", checkpoint.clone())
            .await?;
        match &evidence.target {
            shared_types::UserAppStorageClearTarget::Development {
                base_url,
                instance_id,
                ..
            } => {
                let ticket = leases.development.as_mut().ok_or_else(|| {
                    AppOperationError::Backend("Captured builder lease is unavailable".into())
                })?;
                ticket
                    .begin_external_mutation()
                    .map_err(AppOperationError::Backend)?;
                guard.mark_mutating()?;
                let response = workspace_client
                    .post(format!("{base_url}/api/v1/userapp/app-files/clear"))
                    .timeout(std::time::Duration::from_secs(120))
                    .json(&shared_types::UserAppWorkspaceClearRequest {
                        app_id: app_id.into(),
                        user_id: user_id.to_owned(),
                        expected_instance_id: instance_id.clone(),
                    })
                    .send()
                    .await
                    .map_err(|error| {
                        AppOperationError::Backend(format!("Clear development workspace: {error}"))
                    })?;
                let response =
                    crate::ops::files::check_status(response, "clear-dev-workspace", app_id)
                        .await?;
                let result: shared_types::UserAppWorkspaceClearResult =
                    response.json().await.map_err(|error| {
                        AppOperationError::Backend(format!(
                            "Decode workspace clear result: {error}"
                        ))
                    })?;
                if !result.confirms(instance_id) {
                    return Err(AppOperationError::Backend(
                        "Workspace clear did not confirm success".into(),
                    ));
                }
                ticket
                    .finish_external_mutation()
                    .await
                    .map_err(AppOperationError::Backend)?;
            }
            shared_types::UserAppStorageClearTarget::Production {
                snapshot,
                directories,
            } => {
                self.ensure_app_deleted(app_id, "clearing captured storage")
                    .await?;
                if self.config.access_mode == crate::config::AppAccessMode::Docker {
                    if leases
                        .directories
                        .iter()
                        .map(|lease| &lease.receipt)
                        .ne(directories.iter())
                    {
                        return Err(AppOperationError::Conflict(
                            "Storage directory leases differ from captured evidence".into(),
                        ));
                    }
                    for directory in &leases.directories {
                        directory.validate_current().await.map_err(|error| {
                            map_io_error("Validate captured storage directory", error, false)
                        })?;
                    }
                }
                guard.mark_mutating()?;
                if self.config.access_mode == crate::config::AppAccessMode::Kubernetes {
                    self.runtime
                        .destroy_app_storage_snapshot(snapshot)
                        .await
                        .map_err(|error| map_runtime_error("Clear captured storage", error))?;
                } else {
                    for directory in &leases.directories {
                        directory.clear().await.map_err(|error| {
                            map_io_error("Clear captured storage directory", error, false)
                        })?;
                    }
                }
            }
        }
        operation
            .checkpoint("storage_contents_cleared", checkpoint)
            .await?;
        Ok::<(), AppOperationError>(())
    }

    pub async fn clear_app_storage_controlled(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        request: ClearStorageRequest,
    ) -> AppResult<String> {
        use garde::Validate as _;
        use sha2::Digest as _;
        validate_app_id(app_id)?;
        request
            .validate()
            .map_err(|error| AppOperationError::Validation(error.to_string()))?;
        let guard = self.acquire_process_release_lock(app_id).await?;
        let result = async {
            self.metadata
                .validate_request_lifecycle(
                    app_id,
                    &request.user_id,
                    request.lifecycle_id.as_deref(),
                )
                .await?;
            self.get_lifecycle(app_id, &request.user_id).await?;
            let kind = match app_stage {
                UserappStage::Dev => shared_types::UserAppOperationKind::ClearDevStorage,
                UserappStage::Prod => shared_types::UserAppOperationKind::ClearProdStorage,
            };
            let fingerprint = hex::encode(sha2::Sha256::digest(
                shared_types::encode_userapp_intent(
                    &serde_json::json!({"stage":app_stage.as_str(), "request":request}),
                )
                .map_err(|error| {
                    AppOperationError::Backend(format!("Encode storage clear intent: {error}"))
                })?,
            ));
            let control = shared_types::UserAppControlRequest {
                user_id: request.user_id.clone(),
                lifecycle_id: request.lifecycle_id.clone(),
                request_id: request.request_id.clone(),
            };
            if let Some(existing) = self
                .replay_control(app_id, &control, kind, &fingerprint)
                .await?
            {
                return Ok(existing.operation_id);
            }
            // Ensure must finish before admitting clear: builder ensure has its
            // own durable operation and must not wait on the clear it is serving.
            if app_stage == UserappStage::Dev {
                self.app_files_base(app_stage, app_id, Some(&request.user_id))
                    .await?;
            }
            let operation_id = uuid::Uuid::new_v4().to_string();
            let mut operation = crate::service::OwnedOperation::admit(
                self.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    runtime_policy_on_success: None,
                    metadata: None,
                    command: Some(shared_types::UserAppControlCommand::ClearStorage {
                        production: app_stage == UserappStage::Prod,
                    }),
                    kind,
                    app_id: app_id.into(),
                    user_id: request.user_id.clone(),
                    lifecycle_id: request.lifecycle_id,
                    request_id: request.request_id,
                    operation_id: operation_id.clone(),
                    request_fingerprint: fingerprint,
                },
            )
            .await?;
            let mut clear_leases = StorageClearLeases::default();
            let mutation = self
                .execute_storage_clear(
                    app_id,
                    &request.user_id,
                    app_stage == UserappStage::Prod,
                    &mut operation,
                    &guard,
                    &mut clear_leases,
                )
                .await;
            match mutation {
                Ok(()) => {
                    operation.succeed().await?;
                    guard.mark_completed();
                    Ok(operation_id)
                }
                Err(error) => {
                    if guard.has_unfinished_mutation() {
                        operation.fail(&error).await?;
                    } else {
                        operation.reject_without_mutation(&error).await?;
                    }
                    Err(error)
                }
            }
        }
        .await;
        if result.is_ok() || !guard.has_unfinished_mutation() {
            guard.finish().await?;
        }
        result
    }

    /// 销毁应用持久存储 PVC（高危·不可逆·释放配额）。
    ///
    /// - `prod`：安全约束 ① 仅当 app 计算资源已不存在（已 delete）时允许（否则
    ///   INVALID_STATE）；② body `confirm` 必须等于 app_id。K8s 删 PVC 对象，
    ///   Docker 等价删 bind 目录；保留应用生命周期和元数据。
    /// - `dev`：销毁**整个开发环境** = UserappDevCleanup 四步回收（builder 容器 +
    ///   dev PVC + Docker dev 目录 + 摘注册/探活缓存）；**不动 metadata**（owner
    ///   保留——create-workspace 幂等重建开发环境）。幂等：资源不存在视为成功。
    pub async fn destroy_app_storage(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        user_id: &str,
        confirm: &str,
    ) -> AppResult<()> {
        self.destroy_app_storage_controlled(
            app_stage,
            app_id,
            DestroyStorageRequest {
                user_id: user_id.into(),
                confirm: confirm.into(),
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
        .map(|_| ())
    }

    pub async fn destroy_app_storage_controlled(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        request: DestroyStorageRequest,
    ) -> AppResult<String> {
        use garde::Validate as _;
        use sha2::Digest as _;
        validate_app_id(app_id)?;
        request
            .validate()
            .map_err(|error| AppOperationError::Validation(error.to_string()))?;
        if request.confirm != app_id {
            return Err(AppOperationError::Validation(
                "confirm must equal app_id for destroy".into(),
            ));
        }
        let guard = self.acquire_process_release_lock(app_id).await?;
        let result = async {
            self.metadata
                .validate_request_lifecycle(
                    app_id,
                    &request.user_id,
                    request.lifecycle_id.as_deref(),
                )
                .await?;
            self.get_lifecycle(app_id, &request.user_id).await?;
            let production = app_stage == UserappStage::Prod;
            let command = shared_types::UserAppControlCommand::DestroyStorage { production };
            let fingerprint = hex::encode(sha2::Sha256::digest(
                shared_types::encode_userapp_intent(
                    &serde_json::json!({"stage":app_stage.as_str(), "request":request}),
                )
                .map_err(|error| {
                    AppOperationError::Backend(format!(
                        "Encode storage destruction intent: {error}"
                    ))
                })?,
            ));
            let control = shared_types::UserAppControlRequest {
                user_id: request.user_id.clone(),
                lifecycle_id: request.lifecycle_id.clone(),
                request_id: request.request_id.clone(),
            };
            if let Some(existing) = self
                .replay_control(app_id, &control, command.kind(), &fingerprint)
                .await?
            {
                return Ok(existing.operation_id);
            }
            let operation_id = uuid::Uuid::new_v4().to_string();
            let mut operation = crate::service::OwnedOperation::admit(
                self.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    runtime_policy_on_success: None,
                    metadata: None,
                    kind: command.kind(),
                    command: Some(command),
                    app_id: app_id.into(),
                    user_id: request.user_id.clone(),
                    lifecycle_id: request.lifecycle_id,
                    request_id: request.request_id,
                    operation_id: operation_id.clone(),
                    request_fingerprint: fingerprint,
                },
            )
            .await?;
            match self
                .execute_storage_destruction(
                    app_id,
                    &request.user_id,
                    production,
                    &mut operation,
                    &guard,
                )
                .await
            {
                Ok(()) => {
                    operation.succeed().await?;
                    guard.mark_completed();
                    self.invalidate_deploy_cache().await;
                    Ok(operation_id)
                }
                Err(error) => {
                    if guard.has_unfinished_mutation() {
                        operation.fail(&error).await?;
                    } else {
                        operation.reject_without_mutation(&error).await?;
                    }
                    Err(error)
                }
            }
        }
        .await;
        if result.is_ok() || !guard.has_unfinished_mutation() {
            guard.finish().await?;
        }
        result
    }

    pub(crate) async fn execute_storage_destruction(
        &self,
        app_id: &str,
        owner: &str,
        production: bool,
        operation: &mut crate::service::OwnedOperation,
        guard: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        operation.bind_lease(guard, owner).await?;
        if production {
            self.ensure_app_deleted(app_id, "destroying captured storage")
                .await?;
        }
        let development = self.capture_dev_deletion(app_id).await?;
        let snapshot = if production {
            Some(
                self.runtime
                    .capture_app_deletion(app_id, None)
                    .await
                    .map_err(|error| map_runtime_error("Capture storage destruction", error))?,
            )
        } else {
            None
        };
        let evidence = shared_types::UserAppStorageDestruction {
            context: operation.execution_context(owner),
            production: snapshot,
            development: development.receipt(),
        };
        evidence.validate().map_err(AppOperationError::Conflict)?;
        let checkpoint = serde_json::to_value(&evidence).map_err(|error| {
            AppOperationError::Backend(format!("Encode storage destruction checkpoint: {error}"))
        })?;
        operation
            .checkpoint("storage_captured", checkpoint.clone())
            .await?;
        guard.mark_mutating()?;
        if let Some(snapshot) = &evidence.production {
            self.ensure_app_deleted(app_id, "destroying captured storage")
                .await?;
            self.runtime
                .destroy_app_storage_snapshot(snapshot)
                .await
                .map_err(|error| map_runtime_error("Destroy captured storage", error))?;
            operation
                .checkpoint("production_storage_removed", checkpoint.clone())
                .await?;
        }
        development.cleanup().await.map_err(|error| {
            AppOperationError::Backend(format!("Destroy captured development storage: {error}"))
        })?;
        operation
            .checkpoint("development_storage_removed", checkpoint)
            .await?;
        Ok(())
    }

    pub(crate) async fn capture_dev_deletion(
        &self,
        app_id: &str,
    ) -> AppResult<Box<dyn shared_types::UserappDevDeletion>> {
        let cleanup = self
            .dev_cleanup
            .read()
            .map_err(|_| AppOperationError::Backend("dev cleanup lock poisoned".into()))?
            .clone()
            .ok_or_else(|| AppOperationError::Backend("userapp dev cleanup not injected".into()))?;
        let deletion = cleanup.capture(app_id).await.map_err(|error| {
            AppOperationError::Backend(format!("capture userapp dev deletion: {error}"))
        })?;
        let receipt = deletion.receipt();
        if receipt.runtime.app_id != app_id {
            return Err(AppOperationError::Conflict(
                "Captured development deletion belongs to another application".into(),
            ));
        }
        Ok(deletion)
    }

    /// 销毁持久存储但**保留业务元数据行**（delete_app 的 purge 分支专用）。
    ///
    /// Production purge and standalone storage destruction retain identity.
    /// Only full delete/app terminates the durable application lifecycle.
    /// Full deletion ends the lifecycle only after all captured resources are removed.
    /// A failed or cancelled execution retains its durable operation for recovery.
    pub async fn purge_app(&self, app_id: &str) -> AppResult<()> {
        let app = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
            })?;
        self.purge_app_controlled(
            app_id,
            shared_types::UserAppControlRequest {
                user_id: app.user_id,
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
    }

    pub async fn purge_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        let release_lock = self.acquire_process_release_lock(app_id).await?;
        let result = self
            .purge_app_admitted(app_id, request, &release_lock)
            .await;
        if result.is_ok() || !release_lock.has_unfinished_mutation() {
            release_lock.finish().await?;
        }
        result
    }

    async fn purge_app_admitted(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
        release_lock: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        // Read identity after acquiring the operation guard; a previous lifecycle
        // must never authorize deleting resources created while this caller waited.
        let app = self.get_lifecycle(app_id, &request.user_id).await?;
        if request
            .lifecycle_id
            .as_ref()
            .is_some_and(|id| id != &app.lifecycle_id)
            || (app.lifecycle_epoch > 1 && request.lifecycle_id.is_none())
        {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        use sha2::Digest as _;
        let fingerprint = hex::encode(sha2::Sha256::digest(
            shared_types::encode_userapp_intent(&request).map_err(|error| {
                AppOperationError::Backend(format!("Encode application deletion intent: {error}"))
            })?,
        ));
        if self
            .replay_control(
                app_id,
                &request,
                shared_types::UserAppOperationKind::DeleteApplication,
                &fingerprint,
            )
            .await?
            .is_some()
        {
            return Ok(());
        }
        if app.state == shared_types::UserAppLifecycleState::Deleted {
            return Ok(());
        }
        let mut operation = crate::service::OwnedOperation::admit(
            self.metadata.store.clone(),
            shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: Some(shared_types::UserAppControlCommand::DeleteApplication),
                metadata: None,
                app_id: app_id.into(),
                user_id: app.user_id,
                lifecycle_id: Some(app.lifecycle_id),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: request.request_id,
                request_fingerprint: fingerprint,
                kind: shared_types::UserAppOperationKind::DeleteApplication,
            },
        )
        .await?;
        match self
            .purge_app_resources(app_id, &request.user_id, &mut operation, release_lock)
            .await
        {
            Ok(()) => {
                operation.succeed().await?;
                release_lock.mark_completed();
                self.activity.forget_app(app_id);
                self.invalidate_deploy_cache().await;
                Ok(())
            }
            Err(error) => {
                operation.fail(&error).await.map_err(|persist| AppOperationError::Backend(
                    format!("Application deletion failed: {error}; recording failure also failed: {persist}")
                ))?;
                Err(error)
            }
        }
    }

    pub(crate) async fn purge_app_resources(
        &self,
        app_id: &str,
        owner: &str,
        operation: &mut crate::service::OwnedOperation,
        release_lock: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        operation.bind_lease(release_lock, owner).await?;
        // 与发布链（prepare/activate/confirm/delete-release）及 create/update/delete
        // 串行：purge 全程删计算+存储，不能与写版本包/切 code 并发。
        let dev_deletion = self.capture_dev_deletion(app_id).await?;
        let snapshot = self
            .runtime
            .capture_app_deletion(app_id, None)
            .await
            .map_err(|e| map_runtime_error("capture purge resources", e))?;
        let mut checkpoint = shared_types::UserAppDeletionCheckpoint {
            stage: shared_types::UserAppDeletionStage::Captured,
            schema_version: 1,
            context: operation.execution_context(owner),
            kind: shared_types::UserAppOperationKind::DeleteApplication,
            production: snapshot.clone(),
            development: Some(dev_deletion.receipt()),
        };
        checkpoint.validate().map_err(AppOperationError::Conflict)?;
        operation
            .checkpoint(
                "resources_captured",
                serde_json::to_value(&checkpoint).map_err(|error| {
                    AppOperationError::Backend(format!("Encode deletion checkpoint: {error}"))
                })?,
            )
            .await?;
        // 1. prod 计算面：存在才拆（防护序列 + 失败对称恢复见 tear_down_compute_plane）
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(previous)) => {
                release_lock.mark_mutating()?;
                self.tear_down_compute_plane(app_id, &previous, &snapshot)
                    .await?
            }
            Ok(None) => {
                if snapshot.resources.iter().any(|resource| {
                    resource.kind != shared_types::AppResourceKind::PersistentVolumeClaim
                }) {
                    release_lock.mark_mutating()?;
                    self.tear_down_compute_plane(
                        app_id,
                        &container_runtime_api::DeploymentStatus {
                            app_id: app_id.into(),
                            ..Default::default()
                        },
                        &snapshot,
                    )
                    .await?;
                }
                info!(
                    "[APP] purge: captured compute resources are absent: {}",
                    app_id
                );
            }
            Err(e) => {
                warn!(
                    "[APP] purge: query app status failed app_id={}: {}",
                    app_id, e
                );
                return Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )));
            }
        }
        operation
            .deletion_progress(
                &mut checkpoint,
                shared_types::UserAppDeletionStage::ComputeRemoved,
            )
            .await?;
        release_lock.mark_mutating()?;
        self.finish_captured_purge(app_id, operation, &mut checkpoint, dev_deletion)
            .await?;
        // Keep the file/runtime lease until the durable terminal record commits.
        Ok(())
    }

    /// Both purge entry points retain the captured targets through every stage.
    pub(crate) async fn finish_captured_purge(
        &self,
        app_id: &str,
        operation: &mut crate::service::OwnedOperation,
        checkpoint: &mut shared_types::UserAppDeletionCheckpoint,
        dev_deletion: Box<dyn shared_types::UserappDevDeletion>,
    ) -> AppResult<()> {
        if checkpoint.development.as_ref() != Some(&dev_deletion.receipt()) {
            return Err(AppOperationError::Conflict(
                "Development deletion ticket changed after capture".into(),
            ));
        }
        self.ensure_app_deleted(app_id, "destroying captured storage")
            .await?;
        self.runtime
            .destroy_app_storage_snapshot(&checkpoint.production)
            .await
            .map_err(|error| map_runtime_error("Destroy captured production storage", error))?;
        operation
            .deletion_progress(
                checkpoint,
                shared_types::UserAppDeletionStage::ProductionStorageRemoved,
            )
            .await?;
        dev_deletion.cleanup().await.map_err(|error| {
            AppOperationError::Backend(format!("destroy captured userapp dev resources: {error}"))
        })?;
        operation
            .deletion_progress(
                checkpoint,
                shared_types::UserAppDeletionStage::DevelopmentRemoved,
            )
            .await?;
        Ok(())
    }

    /// 分页查询持久存储（强制分页，无全量模式；prod=运行卷，dev=开发卷）。
    /// 过滤：orphan_only、app_ids 生效；tenant_id/space_id 在无状态下不支持（rcoder 不持
    /// app→租户映射），提供则 warn 忽略。
    ///
    /// dev 的在跑/orphan 判定走逐项 `dev_container_alive`（builder 非 Deployment，
    /// 无法像 prod 一次 list 拿全集）：`orphan_only` 过滤阶段对候选逐项探测
    /// （显式成本），否则仅对当前页条目探测（≤page_size 次）。
    pub async fn query_storage(
        &self,
        app_stage: UserappStage,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>> {
        if request.page == 0 {
            return Err(AppOperationError::Validation(
                "page starts from 1".to_string(),
            ));
        }
        if request.page_size == 0 || request.page_size > 100 {
            return Err(AppOperationError::Validation(
                "page_size must be in 1..=100".to_string(),
            ));
        }
        let filters = request.filters.unwrap_or_default();
        let metadata = self.metadata.snapshot().await?;
        let service_type = service_type_of(app_stage);
        // dev 逐项 alive 探测通道（仅 Dev 使用；未注入时保守判"在"→非 orphan）
        let dev_locator = if app_stage == UserappStage::Dev {
            Some(
                self.dev_locator
                    .read()
                    .map_err(|_| AppOperationError::Backend("Dev locator lock poisoned".into()))?
                    .clone()
                    .ok_or_else(|| {
                        AppOperationError::Backend("dev container locator not injected".to_string())
                    })?,
            )
        } else {
            None
        };
        // 现有 app 集合（供 prod 的 is_orphan），一次 list 调用；dev 不整集预取
        //（builder 非 Deployment，无整集接口——见上注释）
        let existing: std::collections::HashSet<String> = if app_stage == UserappStage::Prod {
            self.runtime
                .list_deployments()
                .await
                .map_err(|e| map_runtime_error("[APP] list_deployments failed", e))?
                .into_iter()
                .map(|s| s.app_id)
                .collect()
        } else {
            std::collections::HashSet::new()
        };
        // 候选 = 所有"有持久数据"的 app：枚举对应形态的 per-app PVC（含**已 delete
        // 但 PVC 保留的孤儿**）——这才是 orphan 检测的数据源。prod 再并入运行中的
        // app（existing 兜底；正常 running app 都有 PVC，已含）；dev 不并入（builder
        // 无卷属病态，注册表/容器清单不是卷事实源）。
        let mut entries: std::collections::HashSet<String> = self
            .runtime
            .list_workspace_identifiers(&service_type)
            .await
            .map_err(|e| map_runtime_error("[APP] list_workspace_identifiers failed", e))?
            .into_iter()
            .collect();
        for id in existing.iter() {
            entries.insert(id.clone());
        }
        let mut entries: Vec<String> = entries.into_iter().collect();
        entries.sort();
        let app_ids_filter = filters.app_ids.as_deref();
        let orphan_only = filters.orphan_only.unwrap_or(false);
        // dev 在跑判定（探测通道缺失时保守判"在"→非 orphan）
        async fn dev_alive(
            locator: &Option<std::sync::Arc<dyn shared_types::UserappDevLocator>>,
            app_id: &str,
        ) -> AppResult<bool> {
            let locator = locator.as_ref().ok_or_else(|| {
                AppOperationError::Backend("Dev locator is not configured".into())
            })?;
            locator
                .dev_container_alive(app_id)
                .await
                .map_err(AppOperationError::Backend)
        }
        let filtered: Vec<String> = entries
            .into_iter()
            .filter(|app_id| {
                let Some(owner) = metadata.get(app_id) else {
                    return false;
                };
                if owner.user_id.as_deref() != Some(request.user_id.as_str())
                    || filters
                        .tenant_id
                        .as_ref()
                        .is_some_and(|tenant| owner.tenant_id.as_ref() != Some(tenant))
                    || filters
                        .space_id
                        .as_ref()
                        .is_some_and(|space| owner.space_id.as_ref() != Some(space))
                {
                    return false;
                }
                if let Some(ids) = app_ids_filter
                    && !ids.iter().any(|x| x == app_id)
                {
                    return false;
                }
                true
            })
            .collect();
        // orphan_only 过滤（显式成本：dev 对全部候选逐项探测）
        let filtered: Vec<String> = if orphan_only {
            let mut kept = Vec::new();
            for app_id in filtered {
                let is_orphan = if app_stage == UserappStage::Prod {
                    !existing.contains(&app_id)
                } else {
                    !dev_alive(&dev_locator, &app_id).await?
                };
                if is_orphan {
                    kept.push(app_id);
                }
            }
            kept
        } else {
            filtered
        };
        let total = filtered.len() as u64;
        let page = request.page as usize;
        let page_size = request.page_size as usize;
        let start = page.saturating_sub(1) * page_size;
        let paged: Vec<String> = filtered.into_iter().skip(start).take(page_size).collect();

        let mut items = Vec::with_capacity(paged.len());
        for app_id in paged {
            let is_orphan = if app_stage == UserappStage::Prod {
                !existing.contains(&app_id)
            } else {
                !dev_alive(&dev_locator, &app_id).await?
            };
            let (exists, path) = self.storage_path_info(app_stage, &app_id).await?;
            let modified_at = if shared_types::is_kubernetes_runtime() {
                None
            } else {
                tokio::fs::metadata(&path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
            };
            items.push(StorageInfo {
                app_id,
                exists,
                path: path.to_string_lossy().to_string(),
                modified_at,
                is_orphan,
            });
        }
        let total_pages = if total == 0 {
            1
        } else {
            total.div_ceil(page_size as u64) as u32
        };
        Ok(PaginatedResponse {
            items,
            pagination: Pagination {
                page: request.page,
                page_size: request.page_size,
                total,
                total_pages,
            },
        })
    }

    /// Only a successful runtime query can establish absence.
    pub(super) async fn is_storage_orphan(&self, app_id: &str) -> AppResult<bool> {
        Ok(self
            .runtime
            .get_deployment_status(app_id)
            .await
            .map_err(|error| map_runtime_error("query storage compute ownership", error))?
            .is_none())
    }

    async fn is_dev_storage_orphan(&self, app_id: &str) -> AppResult<bool> {
        let locator = self
            .dev_locator
            .read()
            .map_err(|_| AppOperationError::Backend("Dev locator lock poisoned".into()))?
            .clone()
            .ok_or_else(|| AppOperationError::Backend("Dev locator is not configured".into()))?;
        Ok(!locator
            .dev_container_alive(app_id)
            .await
            .map_err(AppOperationError::Backend)?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::{MockRuntime, test_service};

    /// dev query 依赖 locator 做 builder 在跑探测——stub 恒"在"（非 orphan）
    struct StubDevLocator;
    #[async_trait::async_trait]
    impl shared_types::UserappDevLocator for StubDevLocator {
        async fn dev_file_server_addr(
            &self,
            _app_id: &str,
            _user_id: Option<&str>,
        ) -> Result<String, String> {
            Ok("http://127.0.0.1:60000".to_string())
        }
        async fn dev_container_alive(&self, _app_id: &str) -> Result<bool, String> {
            Ok(true)
        }
    }

    /// storage 的 app_stage 分派落点：workspace_volume_name / list_workspace_identifiers
    /// 必须按 app_stage 换 ServiceType（dev→UserappBuilder / prod→Userapp）——K8s 卷
    /// label 与 Docker 目录树都按它分形，分派错即查错卷。
    #[tokio::test]
    async fn storage_env_dispatches_service_type() {
        let runtime = Arc::new(MockRuntime::default());
        let root = tempfile::tempdir().expect("test root");
        let service = test_service(root.path(), runtime.clone()).await;
        service
            .set_dev_locator(Arc::new(StubDevLocator))
            .expect("locator");
        for app in ["app-1", "app-dev", "app-prod"] {
            service
                .metadata
                .record(app, None, Some("u1".into()), None, None)
                .await
                .expect("owner");
        }

        service
            .metadata
            .record("app-1", None, Some("u1".into()), None, None)
            .await
            .expect("register owner");
        service
            .get_app_storage(UserappStage::Prod, "app-1")
            .await
            .expect("prod storage");
        service
            .get_app_storage(UserappStage::Dev, "app-1")
            .await
            .expect("dev storage");

        let calls = runtime.volume_name_calls.get("app-1").expect("calls");
        assert_eq!(
            *calls,
            vec!["Userapp".to_string(), "UserappBuilder".to_string()],
            "prod 先查运行卷、dev 查开发卷（ServiceType 分派）"
        );
    }

    /// query 的 app_stage 分派：dev 清单枚举 UserappBuilder 卷（不并入 Deployment 集），
    /// prod 枚举 Userapp 卷（并入运行中 app 兜底）。
    #[tokio::test]
    async fn query_storage_env_selects_volume_family() {
        let runtime = Arc::new(MockRuntime::default());
        runtime
            .workspace_ids
            .insert("UserappBuilder".to_string(), vec!["app-dev".to_string()]);
        runtime
            .workspace_ids
            .insert("Userapp".to_string(), vec!["app-prod".to_string()]);
        let root = tempfile::tempdir().expect("test root");
        let service = test_service(root.path(), runtime.clone()).await;
        service
            .set_dev_locator(Arc::new(StubDevLocator))
            .expect("locator");
        for app in ["app-1", "app-dev", "app-prod"] {
            service
                .metadata
                .record(app, None, Some("u1".into()), None, None)
                .await
                .expect("owner");
        }
        *service.dev_locator.write().expect("dev_locator lock") = Some(Arc::new(StubDevLocator));

        let dev_resp = service
            .query_storage(
                UserappStage::Dev,
                QueryStorageRequest {
                    user_id: "u1".into(),
                    page: 1,
                    page_size: 10,
                    filters: None,
                },
            )
            .await
            .expect("dev query");
        assert_eq!(
            dev_resp
                .items
                .iter()
                .map(|i| i.app_id.as_str())
                .collect::<Vec<_>>(),
            vec!["app-dev"],
            "dev 清单只含开发卷"
        );

        let prod_resp = service
            .query_storage(
                UserappStage::Prod,
                QueryStorageRequest {
                    user_id: "u1".into(),
                    page: 1,
                    page_size: 10,
                    filters: None,
                },
            )
            .await
            .expect("prod query");
        assert_eq!(
            prod_resp
                .items
                .iter()
                .map(|i| i.app_id.as_str())
                .collect::<Vec<_>>(),
            vec!["app-prod"],
            "prod 清单只含运行卷"
        );
    }
}
