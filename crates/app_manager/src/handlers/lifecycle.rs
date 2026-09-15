//! 应用生命周期 handler（create / query / get / update / delete）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use garde::Validate as _;
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{
    AppRuntimeInfo, DeleteAppRequest, OwnerParams, PaginatedResponse, PurgeAppRequest,
    QueryAppsRequest, UpdateAppRequest,
};

// create REST 面已删除（统一走 POST /{app_id}/start：不存在则由发布链/ url 部署自动创建）。

/// 查询应用列表
///
/// 实时查集群 + 过滤/分页；仅 status/app_ids 过滤生效。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/query",
    request_body = QueryAppsRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<PaginatedResponse<AppRuntimeInfo>>)
    ),
    tag = "Userapp · prod · 应用查询"
)]
#[instrument(skip(state, request))]
pub async fn query_apps(
    State(state): State<Arc<AppManagerState>>,
    Json(request): Json<QueryAppsRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<AppRuntimeInfo>>>, AppError> {
    request
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!("[APP] querying apps (user_id={})", request.user_id);
    let response = state.app_service.query_apps(request).await?;
    Ok(Json(HttpResult::success(response)))
}

/// 列出应用运行时状态
///
/// 对账接口：列出集群中所有 rcoder 托管的应用运行时状态，供 Java 在 rcoder/自身重启后对账（rcoder 不持久化 app 元数据）。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/runtime",
    params(OwnerParams),
    responses(
        (status = 200, description = "对账成功（仅该 user_id 归属的应用）", body = HttpResult<Vec<AppRuntimeInfo>>)
    ),
    tag = "Userapp · prod · 应用查询"
)]
#[instrument(skip(state))]
pub async fn list_app_runtimes(
    State(state): State<Arc<AppManagerState>>,
    Query(owner): Query<OwnerParams>,
) -> Result<Json<HttpResult<Vec<AppRuntimeInfo>>>, AppError> {
    owner
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] reconcile: listing app runtimes (user_id={})",
        owner.user_id
    );
    let runtimes = state
        .app_service
        .list_app_runtimes(owner.user_id.trim())
        .await?;
    Ok(Json(HttpResult::success(runtimes)))
}

/// 获取应用运行时详情（实时查集群）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        OwnerParams,
    ),
    description = r#"
实时查询单个应用的运行时全量快照：phase / replicas / 健康状态（含实例 IP）、
端口与访问地址（`access.external.http` 等，Pingora 模式返回
`/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}` 形态）、release 与制品摘要。

- 与 `GET /runtime` 的区别：本接口是**单条实时**查询，对账列表是集群全量；
- 不存在 → 404；查询失败（集群不可达）→ 500。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<AppRuntimeInfo>)
    ),
    tag = "Userapp · prod · 应用查询"
)]
#[instrument(skip(state))]
pub async fn get_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    owner: Result<Query<OwnerParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    let Query(owner) = owner
        .map_err(|_| AppError::validation_error("A valid user_id query parameter is required"))?;
    owner
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] getting app runtime: {} (user_id={})",
        app_id, owner.user_id
    );
    let runtime = state
        .app_service
        .get_app_for_owner(&app_id, owner.user_id.trim())
        .await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 更新应用（部分更新，`None` 字段沿用 live 值）
///
/// `user_id` 必填（宿主机数据卷分区定位——compose 挂载路径组成段）；其余字段
/// 可选：`env`/`secrets` 显式传 = 整段替换，`image` 缺省 = 平台默认运行时镜像，
/// 回收字段缺省沿用既有值。K8s SSA re-apply 幂等，Docker 重建容器；工作空间
/// 目录保留。携带 `expected_resource_version` 启用乐观锁（不匹配 → 409；
/// Docker 模式 resource_version 为 None，忽略校验）。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/update",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = UpdateAppRequest,
    responses(
        (status = 200, description = "更新成功", body = HttpResult<AppRuntimeInfo>)
    ),
    tag = "Userapp · prod · 部署与启停"
)]
#[instrument(skip(state, request))]
pub async fn update_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    request: Result<Json<UpdateAppRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    let Json(request) =
        request.map_err(|_| AppError::validation_error("A valid update JSON body is required"))?;
    request
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!("[APP] updating app: {}", app_id);
    let mut request = request;
    let request_id = request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    let owner = request.user_id.clone();
    let result = state.app_service.update_app(&app_id, request).await;
    let runtime =
        super::control::control_result(&state, &app_id, &owner, &request_id, result).await?;
    let operation = state
        .app_service
        .get_control_operation_by_request(&app_id, &owner, &request_id)
        .await?
        .ok_or_else(|| {
            AppError::internal_server_error("Update completed without a durable operation record")
        })?;
    Ok(Json(
        HttpResult::success(runtime).with_operation_id(operation.operation_id),
    ))
}

/// 删除应用
///
/// 删计算资源并注销运行态；默认保留持久存储，body `{"purge": true}` 连数据面
/// 一起清空。**仅 prod**：dev 开发环境的销毁走 storage 面的
/// `{app_stage=dev}` destroy（builder 容器自愈重建语义不适合"删除"操作）。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `prod`（运行容器删除）")
    ),
    request_body = DeleteAppRequest,
    description = r#"
删除应用：停容器 → 注销 pingora backend → 删除 Deployment/Service/HTTPRoute
等计算资源（元数据行保留，误删找回可用）；`purge=true` 时连持久存储一起销毁。

- **仅 prod**：传 `app_stage=dev` 返回 400——开发环境的销毁由 storage 面
  （`{app_stage=dev}` destroy）承担，builder 容器常驻自愈无"删除"语义；
- Docker compose 下 purge 按 `user_id` 精确清理宿主机目录
  `prod/{user_id}/data/{app_id}` 分区（缺省回退归属元数据→通配兜底），
  **建议始终携带 user_id**；
- 乐观锁：`expected_resource_version` 不匹配 → 409。
"#,
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state, body))]
pub async fn delete_app(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    body: Result<Json<DeleteAppRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<HttpResult<String>>, AppError> {
    if shared_types::UserappStage::parse(&app_stage) != Some(shared_types::UserappStage::Prod) {
        return Err(AppError::validation_error(
            "`delete` is a prod-runtime capability: pass app_stage=prod (to tear down a dev environment use the storage destroy endpoint with app_stage=dev)",
        ));
    }
    let Json(mut body) = body.map_err(|error| {
        AppError::validation_error(&format!("Invalid application deletion request: {error}"))
    })?;
    let purge = body.purge.unwrap_or(false);
    let user_id = body.user_id.trim().to_string();
    body.validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    // Deletion validates an existing identity; it never registers or changes ownership.
    match state.app_service.get_app_owner(&app_id).await? {
        Some(owner) if owner == user_id => {}
        Some(_) => {
            return Err(crate::models::AppOperationError::Conflict(
                "Application ownership conflict".into(),
            )
            .into());
        }
        None => {
            return Err(crate::models::AppOperationError::NotFound(format!(
                "Application identity not found: {app_id}"
            ))
            .into());
        }
    }
    info!(
        "[APP] deleting app: {} (purge={}, user_id={})",
        app_id, purge, user_id
    );
    let request_id = body
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    let result = state.app_service.delete_app_controlled(&app_id, body).await;
    super::control::control_result(&state, &app_id, &user_id, &request_id, result).await?;
    let operation = state
        .app_service
        .get_control_operation_by_request(&app_id, &user_id, &request_id)
        .await?
        .ok_or_else(|| {
            AppError::internal_server_error("Delete completed without a durable operation record")
        })?;
    Ok(Json(
        HttpResult::success("Application resources deleted".to_string())
            .with_operation_id(operation.operation_id),
    ))
}

/// 彻底删除应用（永久删除，幂等）
///
/// 只给 app_id，一步删除：prod 容器/Deployment → prod PVC 与目录数据 →
/// dev 开发环境（builder 容器+dev 卷+目录）→ 元数据行。**不可恢复**。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/delete/app",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = PurgeAppRequest,
    description = r#"
Delete captured dev and prod compute/storage resources and retain a durable lifecycle tombstone.
The body requires user_id. Recreated applications additionally require lifecycle_id.
request_id deduplicates the same intent; reusing it with different parameters is a conflict.
Successful deletion is idempotent within the same lifecycle. Rebuilding requires the explicit
recreate endpoint. Unknown identities are rejected without touching runtime resources.
Failures retain durable recovery evidence; uncertain writes cannot be retried as a new deletion.
Business responses use HTTP 200 and HttpResult; inspect code for the result.
"#,
    responses(
        (status = 200, description = "Deletion completed; lifecycle tombstone retained", body = HttpResult<String>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state, body))]
pub async fn purge_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Result<Json<PurgeAppRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<HttpResult<String>>, AppError> {
    let Json(mut request) = body.map_err(|error| {
        AppError::validation_error(&format!("Invalid full deletion request: {error}"))
    })?;
    request
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    let request_id = request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    let user_id = request.user_id.clone();
    let control = shared_types::UserAppControlRequest {
        user_id: user_id.clone(),
        lifecycle_id: request.lifecycle_id,
        request_id: Some(request_id.clone()),
    };
    info!(app_id, user_id, "Deleting application lifecycle");
    // Keep the entire admitted purge alive across HTTP caller cancellation.
    // The service owns conditional metadata/cache cleanup and mutation completion.
    let service = state.app_service.clone();
    let purge_id = app_id.clone();
    let result = tokio::spawn(async move {
        let result = service.purge_app_controlled(&purge_id, control).await;
        if let Err(error) = &result {
            tracing::error!(app_id = %purge_id, %error, "Owned application purge failed");
        }
        result
    })
    .await
    .map_err(|error| {
        crate::models::AppOperationError::Backend(format!(
            "Application purge worker failed: {error}"
        ))
    })
    .and_then(std::convert::identity);
    super::control::control_result(&state, &app_id, &user_id, &request_id, result).await?;
    let operation = state
        .app_service
        .get_control_operation_by_request(&app_id, &user_id, &request_id)
        .await?;
    let mut response = HttpResult::success("Application deleted".to_string());
    if let Some(operation) = operation {
        response = response.with_operation_id(operation.operation_id);
    }
    Ok(Json(response))
}

#[cfg(test)]
mod deletion_ownership_tests {
    use super::*;
    use crate::AppServiceTrait as _;
    use crate::test_support::{MockRuntime, test_service};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn failed_purge_returns_its_admitted_operation_without_retrying_cleanup() {
        let directory = tempfile::tempdir().expect("directory");
        let runtime = Arc::new(MockRuntime::default());
        let service = Arc::new(test_service(directory.path(), runtime.clone()).await);
        service
            .record_dev_registration("purgefailure", "owner")
            .await
            .expect("identity");
        let dev = Arc::new(crate::test_support::StubDevCleanup::default());
        service.set_dev_cleanup(dev.clone()).expect("dev cleanup");
        runtime.deployments.insert(
            "purgefailure".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "purgefailure".into(),
                replicas: 1,
                ready_replicas: 1,
                phase: "Running".into(),
                ..Default::default()
            },
        );
        runtime.delete_fails.store(true, Ordering::SeqCst);
        let state = Arc::new(AppManagerState {
            app_service: service.clone(),
            http_client: reqwest::Client::new(),
        });
        let request: PurgeAppRequest = serde_json::from_value(serde_json::json!({
            "user_id":"owner", "request_id":"purge-failure-request"
        }))
        .expect("purge body");
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            purge_app(State(state), Path("purgefailure".into()), Ok(Json(request))),
        )
        .await
        .expect("bounded purge")
        .expect_err("injected deletion failure");
        let record = service
            .metadata
            .store
            .get_operation_by_request("purgefailure", "purge-failure-request")
            .await
            .expect("operation query")
            .expect("admitted operation");
        match error {
            AppError::Structured { operation_id, .. } => {
                assert_eq!(operation_id.as_deref(), Some(record.operation_id.as_str()))
            }
            other => panic!("Expected correlated error: {other:?}"),
        }
        assert_eq!(
            record.state,
            shared_types::UserAppOperationState::RecoveryRequired
        );
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(dev.calls.load(Ordering::SeqCst), 0);
        assert!(runtime.deployments.contains_key("purgefailure"));
    }

    #[tokio::test]
    async fn runtime_query_requires_the_persisted_owner_before_runtime_access() {
        let directory = tempfile::tempdir().expect("directory");
        let runtime = Arc::new(MockRuntime::default());
        let service = Arc::new(test_service(directory.path(), runtime.clone()).await);
        service
            .record_dev_registration("queryowner", "original-owner")
            .await
            .expect("owner");
        runtime.deployments.insert(
            "queryowner".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "queryowner".into(),
                phase: "Running".into(),
                ..Default::default()
            },
        );
        // A runtime read consumes this injected failure. Unauthorized access must
        // be rejected before touching the backend, rather than fail incidentally there.
        runtime.status_fails.store(1, Ordering::SeqCst);
        let state = Arc::new(AppManagerState {
            app_service: service.clone(),
            http_client: reqwest::Client::new(),
        });
        let request: OwnerParams =
            serde_json::from_value(serde_json::json!({"user_id":"different-owner"}))
                .expect("owner query");
        assert!(
            get_app(
                State(state.clone()),
                Path("queryowner".into()),
                Ok(Query(request))
            )
            .await
            .is_err()
        );
        assert_eq!(runtime.status_fails.load(Ordering::SeqCst), 1);
        runtime.status_fails.store(0, Ordering::SeqCst);
        let request: OwnerParams =
            serde_json::from_value(serde_json::json!({"user_id":"original-owner"}))
                .expect("owner query");
        assert!(
            get_app(State(state), Path("queryowner".into()), Ok(Query(request)))
                .await
                .is_ok()
        );
        assert!(
            service.release_locks.is_empty(),
            "read must not acquire a mutation lease"
        );
        assert!(
            service
                .metadata
                .store
                .unfinished_operations(None, 10)
                .await
                .expect("operations")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn runtime_query_rejects_lifecycle_recreated_during_runtime_read() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let directory = tempfile::tempdir().expect("directory");
            let runtime = Arc::new(MockRuntime::default());
            let service = Arc::new(test_service(directory.path(), runtime.clone()).await);
            let before = service
                .metadata
                .store
                .ensure_identity("queryrace", "owner")
                .await
                .expect("identity");
            runtime.deployments.insert(
                "queryrace".into(),
                container_runtime_api::DeploymentStatus {
                    app_id: "queryrace".into(),
                    phase: "Running".into(),
                    ..Default::default()
                },
            );
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            *runtime.status_barrier.lock().expect("barrier") = Some(barrier.clone());
            // join! keeps cancellation scoped to this test, without a detached task.
            let read = service.get_app_for_owner("queryrace", "owner");
            let replace = async {
                barrier.wait().await;
                let deletion = crate::service::OwnedOperation::admit(
                    service.metadata.store.clone(),
                    shared_types::UserAppAdmission {
                        runtime_policy_on_success: None,
                        command: None,
                        app_id: "queryrace".into(),
                        user_id: "owner".into(),
                        lifecycle_id: Some(before.lifecycle_id.clone()),
                        operation_id: "query-race-delete".into(),
                        request_id: Some("query-race-delete".into()),
                        kind: shared_types::UserAppOperationKind::DeleteApplication,
                        request_fingerprint: "a".repeat(64),
                        metadata: None,
                    },
                )
                .await
                .expect("test deletion admission");
                crate::test_support::complete_empty_deletion_fixture(deletion, "owner").await;
                service
                    .metadata
                    .store
                    .recreate(
                        "queryrace",
                        "owner",
                        &before.lifecycle_id,
                        "query-race-recreate",
                    )
                    .await
                    .expect("new generation");
                barrier.wait().await;
            };
            let (result, ()) = tokio::join!(read, replace);
            assert!(
                matches!(result, Err(crate::models::AppOperationError::Conflict(_))),
                "{result:?}"
            );
            assert!(service.release_locks.is_empty());
        })
        .await
        .expect("bounded lifecycle query race");
    }

    #[tokio::test]
    async fn delete_validates_owner_without_registering_or_deleting_foreign_resources() {
        for existing_owner in [None, Some("original-owner")] {
            let directory = tempfile::tempdir().expect("directory");
            let runtime = Arc::new(MockRuntime::default());
            let service = Arc::new(test_service(directory.path(), runtime.clone()).await);
            if let Some(owner) = existing_owner {
                service
                    .record_dev_registration("delete-owner", owner)
                    .await
                    .expect("owner");
            }
            runtime.deployments.insert(
                "delete-owner".into(),
                container_runtime_api::DeploymentStatus {
                    app_id: "delete-owner".into(),
                    phase: "Running".into(),
                    ..Default::default()
                },
            );
            let state = Arc::new(AppManagerState {
                app_service: service.clone(),
                http_client: reqwest::Client::new(),
            });
            let body: DeleteAppRequest = serde_json::from_value(
                serde_json::json!({"user_id":"request-owner","purge":false}),
            )
            .expect("body");
            assert!(
                delete_app(
                    State(state),
                    Path(("delete-owner".into(), "prod".into())),
                    Ok(Json(body))
                )
                .await
                .is_err()
            );
            assert_eq!(
                service
                    .metadata
                    .lookup("delete-owner")
                    .await
                    .expect("metadata")
                    .and_then(|row| row.user_id)
                    .as_deref(),
                existing_owner
            );
            assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
        }
    }
}
