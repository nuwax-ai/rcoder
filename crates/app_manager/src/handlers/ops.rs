//! 应用操作 handler（start / stop / restart）

use std::sync::Arc;

use axum::extract::{Json, Path, Query, State};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{
    AppRuntimeInfo, OwnerParams, RecyclePolicyRequest, StartAppRequest, StartAppResult,
};

/// 启动应用或轻量部署
///
/// 无 body = 传统启动；带 url = 轻量部署。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/start",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body(
        content = StartAppRequest,
        description = "user_id 必填（owner 分区与 metadata 注册）；其余可选——空对象 = 传统启动（app 不存在即创建空容器：基础设施形态，PG/ttyd/dbx 可用）。带 url 触发部署：deploy_mode 缺省 pod（app_stage 注入 → Recreate 换 Pod），hot = 容器内原地换应用（不换 Pod、PG/终端不断连；前置不满足自动回退 pod）；release_id 缺省自动生成并在响应返回；sha256 可选校验；app_stage/idle_timeout_seconds 覆盖；pg 凭据自动对齐（不一致重置，失败不阻断部署）"
    ),
    responses(
        (status = 200, description = "启动/部署成功", body = HttpResult<StartAppResult>),
        (status = 400, description = "创建空容器缺 user_id / deploy_mode 非法值", body = HttpResult<String>),
        (status = 409, description = "release 幂等冲突（同 id 不同内容）", body = HttpResult<String>)
    ),
    tag = "UserApp · prod · 部署与启停"
)]
#[instrument(skip(state))]
pub async fn start_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Option<Json<StartAppRequest>>,
) -> Result<Json<HttpResult<StartAppResult>>, AppError> {
    let Some(Json(request)) = body else {
        return Err(AppError::validation_error(
            "request body with required `user_id` is required for start",
        ));
    };
    shared_types::validate_identifier(request.user_id.trim(), "user_id")
        .map_err(|e| AppError::validation_error(&e))?;
    info!(
        "[APP] starting app: {} (deploy={}, user_id={})",
        app_id,
        request.url.is_some(),
        request.user_id
    );
    let result = state
        .app_service
        .start_app_enhanced(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success(result)))
}

/// 停止应用（scale replicas = 0）
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/stop",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        OwnerParams,
    ),
    description = r#"
把运行容器缩到 0 副本停止应用：**数据卷 / 元数据全部保留**，随时可 `start` 重启
（区别于 delete 后的 storage 面）。

- 停止后进入 stopped 集合；生产面流量与文件操作会按"有请求即唤醒"语义自动
  scale 回 1（single-flight，唤醒窗口内返回 503+Retry-After）；
- 与闲置回收协同：`recycle-policy` 到期也会触发同一 stop 路径；
- 需要"彻底销毁"走 delete → （可选）storage/clear | destroy。
"#,
    responses(
        (status = 200, description = "停止成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · prod · 部署与启停"
)]
#[instrument(skip(state))]
pub async fn stop_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Query(owner): Query<OwnerParams>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    shared_types::validate_identifier(owner.user_id.trim(), "user_id")
        .map_err(|e| AppError::validation_error(&e))?;
    info!("[APP] stopping app: {} (user_id={})", app_id, owner.user_id);
    let runtime = state.app_service.stop_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime)))
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
        description = "user_id 必填；其余可选——空对象 = 传统 rollout restart。带 url = 部署新版本（activate 自带切流）；其余字段语义同 start"
    ),
    responses(
        (status = 200, description = "重启/部署成功", body = HttpResult<StartAppResult>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · prod · 部署与启停"
)]
#[instrument(skip(state))]
pub async fn restart_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Option<Json<StartAppRequest>>,
) -> Result<Json<HttpResult<StartAppResult>>, AppError> {
    let Some(Json(request)) = body else {
        return Err(AppError::validation_error(
            "request body with required `user_id` is required for restart",
        ));
    };
    shared_types::validate_identifier(request.user_id.trim(), "user_id")
        .map_err(|e| AppError::validation_error(&e))?;
    info!(
        "[APP] restarting app: {} (deploy={})",
        app_id,
        request.url.is_some()
    );
    let result = state
        .app_service
        .restart_app_enhanced(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success(result)))
}

/// 设置闲置回收策略
///
/// 动态、免重启（免费↔付费 tier 变更）：strategic-merge Deployment 注解,不碰 pod template → 不触发 rollout,下个扫描 tick 生效。
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
        (status = 200, description = "策略已更新（免重启）", body = HttpResult<AppRuntimeInfo>),
        (status = 400, description = "参数错误（三字段皆空 / app_stage 非法或 dev 不支持）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 生命周期"
)]
#[instrument(skip(state, request))]
pub async fn set_recycle_policy(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Json(request): Json<RecyclePolicyRequest>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    if shared_types::UserappStage::parse(&app_stage) != Some(shared_types::UserappStage::Prod) {
        return Err(AppError::validation_error(
            "`recycle-policy` is a prod-runtime capability: pass app_stage=prod (dev environment has no recycle semantics)",
        ));
    }
    let runtime = state
        .app_service
        .set_recycle_policy(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success(runtime)))
}
