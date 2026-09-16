//! 应用操作 handler（start / stop / restart）

use std::sync::Arc;

use axum::extract::{Json, Path, Query, State};
use garde::Validate as _;
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{AppRuntimeInfo, RecyclePolicyRequest, StartAppRequest, StartAppResult};

/// 启动应用或轻量部署
///
/// 无 body = 传统启动；带 url = 轻量部署。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/start",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body(
        content = StartAppRequest,
        description = "user_id 必填（owner 分区与 metadata 注册）；其余可选——空对象 = 传统启动（app 不存在即创建空容器：基础设施形态，PG/ttyd/dbx 可用）。带 url 触发部署：deploy_mode 缺省 pod（app_stage 注入 → Recreate 换 Pod），hot = 容器内原地换应用（不换 Pod、PG/终端不断连；前置不满足自动回退 pod，等编排+bridge 就绪才返回）；release_id 缺省自动生成并在响应返回；sha256 可选校验；app_stage/idle_timeout_seconds 覆盖；pg 凭据自动对齐（不一致重置，失败不阻断部署）。同步等待边界 = 部署段完成（下载/sha256/解压成功、编排已启动）+ 包内 database SQL 执行——服务启动结果异步可见（GET /apps/{app_id} 或访问探活确认）；成功返回 ≠ 立即接流量（readiness 摘流窗口，配置 bridge_service 的应用摘流到后端就绪）。建议客户端读超时 ≥ 120s；超时 ≠ 失败（服务端继续收敛，先查状态再决定是否重试）"
    ),
    responses(
        (status = 200, description = "启动/部署成功（部署 = 制品已部署 + SQL 已执行，服务启动中）", body = HttpResult<StartAppResult>)
    ),
    tag = "Userapp · prod · 部署与启停"
)]
#[instrument(skip(state, body))]
pub async fn start_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Result<Json<StartAppRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<HttpResult<StartAppResult>>, AppError> {
    let Json(mut request) = body.map_err(|_| {
        AppError::validation_error("A valid start JSON body with user_id is required")
    })?;
    request
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] starting app: {} (deploy={}={})",
        app_id,
        request.url.is_some(),
        String::new()
    );
    // The accepted coordinator owns its lease even when the HTTP client disconnects.
    let request_id = request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    let owner = String::new();
    let worker_app_id = app_id.clone();
    let service = state.app_service.clone();
    let result = await_deployment_response(
        tokio::spawn(async move { service.start_app_enhanced(&worker_app_id, request).await }),
        std::time::Duration::from_secs(300),
    )
    .await;
    let result =
        correlate_deployment_response(&state, &app_id, &owner, &request_id, result).await?;
    let operation_id = result.operation_id.clone();
    let mut response = HttpResult::success(result);
    response.operation_id = operation_id;
    Ok(Json(response))
}

/// 停止应用（scale replicas = 0）
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/stop",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("user_id" = String, Query, description = "Application owner"),
        ("lifecycle_id" = Option<String>, Query, description = "Required after explicit recreation"),
        ("request_id" = Option<String>, Query, description = "Idempotent request token"),
    ),
    description = r#"
把运行容器缩到 0 副本停止应用：**数据卷 / 元数据全部保留**，随时可 `start` 重启
（区别于 delete 后的 storage 面）。

- 显式停止会阻断流量唤醒，后续需要显式 start；
- 闲置回收保留流量唤醒能力，与显式 stop 的策略不同；
- 需要"彻底销毁"走 delete → （可选）storage/clear | destroy。

- If another operation holds the lock, this request is rejected without waiting
  for that operation to finish and is not queued (envelope: HTTP 200 with
  success=false and code ERR_CONFLICT). Retry later after checking application state.
"#,
    responses(
        (status = 200, description = "停止成功", body = HttpResult<AppRuntimeInfo>)
    ),
    tag = "Userapp · prod · 部署与启停"
)]
#[instrument(skip(state))]
pub async fn stop_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    request: Result<
        Query<shared_types::UserAppControlRequest>,
        axum::extract::rejection::QueryRejection,
    >,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    let Query(mut request) = request
        .map_err(|_| AppError::validation_error("Valid stop query parameters are required"))?;
    let request_id = request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    info!("[APP] stopping app: {}", app_id);
    let result = state
        .app_service
        .stop_app_controlled(&app_id, request)
        .await;
    let runtime = super::control::control_result(&state, &app_id, &request_id, result).await?;
    let operation = state
        .app_service
        .get_control_operation_by_request(&app_id, &request_id)
        .await?
        .ok_or_else(|| {
            AppError::internal_server_error("Stop completed without a durable operation record")
        })?;
    Ok(Json(
        HttpResult::success(runtime).with_operation_id(operation.operation_id),
    ))
}

/// 重启应用
///
/// rollout restart；可选参数与 start 同款——带 url 即部署新版本并重启。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/restart",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body(
        content = StartAppRequest,
        description = "user_id 必填；其余可选——空对象 = 传统 rollout restart。带 url = 部署新版本（等待边界同 start：部署段完成 + SQL 执行，服务启动异步可见，成功 ≠ 立即接流量）；其余字段语义同 start"
    ),
    description = r#"
- If another operation holds the lock, this request is rejected without waiting
  for that operation to finish and is not queued (envelope: HTTP 200 with
  success=false and code ERR_CONFLICT). Retry later after checking application state.
"#,
    responses(
        (status = 200, description = "重启/部署成功（部署 = 制品已部署 + SQL 已执行，服务启动中）", body = HttpResult<StartAppResult>)
    ),
    tag = "Userapp · prod · 部署与启停"
)]
#[instrument(skip(state, body))]
pub async fn restart_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Result<Json<StartAppRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<HttpResult<StartAppResult>>, AppError> {
    let Json(mut request) = body.map_err(|_| {
        AppError::validation_error("A valid restart JSON body with user_id is required")
    })?;
    request
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] restarting app: {} (deploy={})",
        app_id,
        request.url.is_some()
    );
    // The accepted coordinator owns its lease even when the HTTP client disconnects.
    let request_id = request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    let owner = String::new();
    let worker_app_id = app_id.clone();
    let service = state.app_service.clone();
    let result = await_deployment_response(
        tokio::spawn(async move { service.restart_app_enhanced(&worker_app_id, request).await }),
        std::time::Duration::from_secs(300),
    )
    .await;
    let result =
        correlate_deployment_response(&state, &app_id, &owner, &request_id, result).await?;
    let operation_id = result.operation_id.clone();
    let mut response = HttpResult::success(result);
    response.operation_id = operation_id;
    Ok(Json(response))
}

/// 设置闲置回收策略
///
/// 动态、免重启（策略即时调整，无需重新部署）：strategic-merge Deployment 注解,不碰 pod template → 不触发 rollout,下个扫描 tick 生效。
/// 比 update 轻（无需 image）。三字段（recycle_enabled/idle_timeout_seconds/wake_on_traffic）皆 None → 400。
/// **仅 prod**：策略作用于运行容器 Deployment 注解，dev 开发环境无回收语义。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/recycle-policy",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `prod`（运行容器 Deployment 注解）")
    ),
    request_body = RecyclePolicyRequest,
    description = r#"
动态设置应用的闲置自动回收与流量唤醒策略，免重启、下个扫描 tick 生效：
- `recycle_enabled`：开关闲置回收
- `idle_timeout_seconds`：闲置阈值秒数
- `wake_on_traffic`：流量唤醒开关

三字段全 None → 400。

> **仅 prod**：传 `app_stage=dev` 返回 400（开发容器常驻自愈，无回收语义）。
"#,
    responses(
        (status = 200, description = "策略已更新（免重启）", body = HttpResult<AppRuntimeInfo>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state, request))]
pub async fn set_recycle_policy(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    request: Result<Json<RecyclePolicyRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    let Json(mut request) = request
        .map_err(|_| AppError::validation_error("A valid recycle policy JSON body is required"))?;
    if shared_types::UserappStage::parse(&app_stage) != Some(shared_types::UserappStage::Prod) {
        return Err(AppError::validation_error(
            "`recycle-policy` is a prod-runtime capability: pass app_stage=prod (dev environment has no recycle semantics)",
        ));
    }
    request
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] setting recycle policy: {} (user_id={})",
        app_id,
        String::new()
    );
    let request_id = request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    let result = state.app_service.set_recycle_policy(&app_id, request).await;
    let runtime = super::control::control_result(&state, &app_id, &request_id, result).await?;
    let operation = state
        .app_service
        .get_control_operation_by_request(&app_id, &request_id)
        .await?
        .ok_or_else(|| {
            AppError::internal_server_error("Policy completed without a durable operation record")
        })?;
    Ok(Json(
        HttpResult::success(runtime).with_operation_id(operation.operation_id),
    ))
}

/// Dropping the JoinHandle on timeout detaches the owned coordinator; it does not
/// cancel a remote mutation or release its application lease prematurely.
async fn correlate_deployment_response<T>(
    state: &AppManagerState,
    app_id: &str,
    _owner: &str,
    request_id: &str,
    result: Result<T, AppError>,
) -> Result<T, AppError> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state
            .app_service
            .get_control_operation_by_request(app_id, request_id),
    )
    .await
    {
        Ok(Ok(Some(operation))) => Err(error.with_operation_id(operation.operation_id)),
        _ => Err(error),
    }
}

async fn await_deployment_response<T>(
    task: tokio::task::JoinHandle<crate::error::AppResult<T>>,
    budget: std::time::Duration,
) -> Result<T, AppError> {
    tokio::time::timeout(budget, task).await
        .map_err(|_| AppError::internal_server_error("deployment confirmation timed out; reconciliation continues; inspect deployment status before retrying"))?
        .map_err(|error| AppError::internal_server_error(&format!("deployment coordinator failed: {error}")))?
        .map_err(Into::into)
}

#[cfg(test)]
mod deployment_response_tests {
    use super::*;

    #[tokio::test]
    async fn invalid_start_and_restart_bodies_use_the_business_envelope() {
        let directory = tempfile::tempdir().expect("handler test directory");
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let service =
            Arc::new(crate::test_support::test_service(directory.path(), runtime.clone()).await);
        let router = axum::Router::new()
            .route(
                "/api/v1/userapp/{app_id}/start",
                axum::routing::post(start_app),
            )
            .route(
                "/api/v1/userapp/{app_id}/restart",
                axum::routing::post(restart_app),
            )
            .layer(axum::middleware::from_fn(
                shared_types::userapp_http::envelope_errors,
            ))
            .with_state(Arc::new(AppManagerState {
                app_service: service.clone(),
                http_client: reqwest::Client::new(),
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _server = Server(tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("HTTP test server");
        }));
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("HTTP client");
        for action in ["start", "restart"] {
            for (content_type, body) in [
                ("application/json", "{"),
                ("application/json", r#"{"deploy_mode":"invalid"}"#),
                ("text/plain", r#"{}"#),
            ] {
                let response = client
                    .post(format!(
                        "http://{address}/api/v1/userapp/invalid-body/{action}"
                    ))
                    .header("content-type", content_type)
                    .body(body)
                    .send()
                    .await
                    .expect("HTTP response");
                assert_eq!(response.status(), reqwest::StatusCode::OK);
                let envelope: serde_json::Value =
                    response.json().await.expect("JSON error envelope");
                assert_eq!(envelope["code"], shared_types::error_codes::ERR_VALIDATION);
                assert!(envelope["message"].as_str().expect("message").is_ascii());
            }
        }
        assert_eq!(
            runtime
                .create_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(
            service
                .metadata
                .store
                .get_application("invalid-body")
                .await
                .expect("application query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn response_timeout_does_not_cancel_owned_coordinator() {
        let (release, proceed) = tokio::sync::oneshot::channel();
        let (completed, completion) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            proceed.await.unwrap();
            completed.send(()).unwrap();
            Ok(())
        });
        assert!(
            await_deployment_response(task, std::time::Duration::ZERO)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), completion)
            .await
            .unwrap()
            .unwrap();
    }
}
