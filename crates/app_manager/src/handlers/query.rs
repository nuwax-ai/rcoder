//! 应用查询 handler（logs / health / stats / events / file-logs）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use garde::Validate as _;
use serde::Deserialize;
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{HealthInfo, ResourceStats};

/// 获取应用健康状态（由运行时状态派生）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/health",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）；`prod`=运行容器（Userapp）"),
    ),
    description = r#"
轻量探活面，返回运行时状态派生的健康快照（`HealthInfo`），适合轮询面板 /
网关前置检查。

- `prod`：集群实时查询派生（phase、就绪探针结果、实例 IP）；不深入容器内，
  容器内服务的深检查由 app-cli :3010 `/health` 承担（经日志/代理面访问）。
- `dev`：探活开发容器内 file-server `/health`（builder 常驻自愈，不在则幂等重建）。
- 应用不存在（prod）→ 404；`app_stage` 非法 → 400。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<HealthInfo>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_health(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
) -> Result<Json<HttpResult<HealthInfo>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    info!(
        "[APP] getting app health: {} app_stage={}",
        app_id,
        app_stage.as_str()
    );
    let health = state.app_service.get_app_health(app_stage, &app_id).await?;
    Ok(Json(HttpResult::success(health)))
}

/// readiness 查询参数
///
/// `user_id` 必填：userapp 业务调用方身份（对齐 stats 系接口的必填口径；当前
/// 就绪观察按 app_id 定位不消费该值，预留后续按用户维度的扩展使用）。
#[derive(Debug, Deserialize, utoipa::IntoParams, garde::Validate)]
#[into_params(parameter_in = Query)]
pub struct ReadinessParams {
    /// 用户 ID（必填；userapp 业务调用方身份，预留扩展使用）
    #[garde(pattern(shared_types::IDENTIFIER_RE))]
    pub user_id: String,
}

/// 获取应用业务就绪状态（只读观察：服务健康契约 + Pingap 入口/生效配置）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/readiness",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）；`prod`=运行容器（Userapp）"),
        ReadinessParams
    ),
    description = r#"
业务就绪查询（只读）：由当前物理实例内 app-cli 的业务观察（服务 HTTP 健康契约
+ Pingap 入口/生效配置）合并平台控制意图后的快照。与容器探针（`/health`、
`/ready`）分离。

- 查询成功恒 200（`success=true` 只表示观察完成），**`data.ready` 才表示业务可用**；
  未就绪/启动中/已停止/失败/未知/不支持都是合法观察结果。
- `status`：`not_deployed` / `starting` / `stopping` / `stopped` / `ready` /
  `degraded` / `failed` / `unknown` / `unsupported`；`reason_code` 为结构化原因。
- `container.status` 独立描述计算资源：`missing` / `starting` / `restarting` /
  `stopping` / `running` / `stopped` / `failed` / `recovery_required` / `unknown`。
  `running` 不表示业务可用；控制操作恢复未确认时为 `recovery_required`。
- `container.operation` 为当前 dev/prod 控制头关联的操作，包含终态及错误。
  容器 Restart 的停止、启动、验证阶段均显示 `restarting`；物理 Start 复用
  Restart 协调器时同样按该动作展示。业务热部署不伪装为容器重启。
- `container.operation=null` 不证明之前的操作成功；新请求可替换当前控制头，
  查询原操作请使用 `/computer/pod/operations/{app_id}/{operation_id}`。
  调用方应关联已受理的操作 ID，忽略旧请求迟到的观察，不因 `ready=false`
  或业务 `not_deployed` 自动重复部署。
- 只读保证：不启动/唤醒/停止容器，不刷新闲置计时，不阻塞 Stop/Restart。
- 已停止的计算资源（含 Stop 后删除的开发容器）→ `stopped`；调度中/创建中 → `starting`。
- 查询包含定位、控制意图复核的总预算为 8 秒，耗尽标记 `OBSERVE_INCOMPLETE`；
  保留本次已经读取的容器控制操作，未观察的物理状态为 `unknown`，不改变业务状态。
- 查询期间停止已受理 → `stopping`；旧运行时无新接口 → `unsupported`
  （`RUNTIME_UPGRADE_REQUIRED`）。
- 应用权威记录不存在或已删除、`app_stage` 非法、查询系统故障分别走错误信封
  （404/400/5xx）。
"#,
    responses(
        (status = 200, description = "观察完成（data.ready 才是业务可用）", body = HttpResult<shared_types::UserAppReadinessResponse>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_readiness(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(params): Query<ReadinessParams>,
) -> Response {
    let app_stage = match super::parse_app_stage_param(&app_stage) {
        Ok(stage) => stage,
        Err(error) => return error.into_response(),
    };
    // 校验调用方身份参数格式；实例定位仍只使用 app_id 与 app_stage。
    if let Err(error) = params
        .validate()
        .map_err(shared_types::garde_err_to_app_error)
    {
        return error.into_response();
    }
    info!(
        "[APP] getting app readiness: {} (user_id={})",
        app_id, params.user_id
    );
    match state
        .app_service
        .get_app_readiness(app_stage, &app_id)
        .await
    {
        Ok(readiness) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(HttpResult::success(readiness)),
        )
            .into_response(),
        Err(error) => AppError::from(error).into_response(),
    }
}

/// 获取 DBX 数据库界面的就绪状态
///
/// 只读不唤醒：计算资源缺失/停止 → `stopped`；创建中或 DBX 未应答 →
/// `starting`；DBX 应答 → `ready`；故障/无法确认 → `unknown`。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/dbx/readiness",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）；`prod`=运行容器（UserApp）"),
        ReadinessParams
    ),
    description = r#"
观察当前 dev/prod 实例的 DBX HTTP 服务，不启动/唤醒容器，也不刷新闲置计时。

- `user_id` 必填，观察按 `app_id` 与 `app_stage` 定位；不使用用户 ID 改变实例归属。
- `success=true` 表示查询完成；`data.ready` 才表示 DBX 可用。
- `status` 为 `ready` / `starting` / `stopped` / `unknown`；`reason_code` 为稳定原因码，
  `message` 为可选诊断信息。无法观察不等于服务停止。
- 启停、重启或恢复尚未确认完成时，不能用旧 DBX 应答宣布 `ready=true`。
  探测前后复核控制状态；总预算耗尽仍保留本次已读取的控制操作回执。
- `container` 与 `/readiness` 同源同口径（存储控制意图 + 运行时观察双源合并）：
  `status` 为 missing / starting / restarting / stopping / running / stopped / failed /
  recovery_required / unknown，`operation` 为当前控制操作回执（含错误码）。
  与 DBX 探测成败解耦——容器 running 不代表 DBX 已应答，反之探测失败不代表容器停止。
  前端据 `container.status` 分支（stopped→启动引导、stopping→停止中、
  recovery_required→异常提示）；旧响应缺该字段视为 unknown。
- 总预算为 3 秒，覆盖存储、运行时定位、HTTP/exec 探测及实例复核；耗尽返回 `unknown`。
- 容器部署使用实例 IP；宿主机使用 DBX 的实际发布端口或捕获实例的只读 exec。
- 查询本身不触发唤醒；需要打开 DBX 时沿用既有 DBX 访问入口。
"#,
    responses(
        (status = 200, description = "观察完成（data.ready 才是 dbx 可用）", body = HttpResult<shared_types::DbxReadinessResponse>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state, params))]
pub async fn get_app_dbx_readiness(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(params): Query<ReadinessParams>,
) -> Response {
    let app_stage = match super::parse_app_stage_param(&app_stage) {
        Ok(stage) => stage,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = params
        .validate()
        .map_err(shared_types::garde_err_to_app_error)
    {
        return error.into_response();
    }
    info!(
        "[APP] getting app dbx readiness: {} (user_id={})",
        app_id, params.user_id
    );
    match state
        .app_service
        .get_app_dbx_readiness(app_stage, &app_id)
        .await
    {
        Ok(readiness) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(HttpResult::success(readiness)),
        )
            .into_response(),
        Err(error) => AppError::from(error).into_response(),
    }
}

/// stats 查询参数
///
/// `user_id` 必填：宿主机数据卷分区归属目录名（Docker compose 挂载路径
/// `prod/{user_id}/data/{app_id}` 组成段）——容器未启动时按此自动唤醒后挂载。
#[derive(Debug, Deserialize, utoipa::IntoParams, garde::Validate)]
#[into_params(parameter_in = Query)]
pub struct StatsParams {
    /// 宿主机数据卷分区归属目录名（必填；Docker compose 挂载路径组成段——
    /// 容器未启动时按此自动唤醒后挂载）
    #[garde(pattern(shared_types::IDENTIFIER_RE))]
    pub user_id: String,
}

/// 获取应用资源使用
///
/// best-effort：restart_count 来自运行时；CPU/内存需 metrics-server（Docker/
/// compose 形态无 metrics → 用量降级 0）。dev 环境按开发容器（双键标签定位）采集。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/stats",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）；`prod`=运行容器（Userapp）"),
        StatsParams
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<ResourceStats>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state, params))]
pub async fn get_app_stats(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(params): Query<StatsParams>,
) -> Result<Json<HttpResult<ResourceStats>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    // 标识符白名单校验（user_id 为宿主机卷路径分区组成段，含 `/` 即逃逸）
    params
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] getting app stats: {} (user_id={})",
        app_id, params.user_id
    );
    let stats = state.app_service.get_app_stats(app_stage, &app_id).await?;
    Ok(Json(HttpResult::success(stats)))
}

/// 获取应用事件
///
/// best-effort：当前返回空，TODO 接 K8s events。**仅 prod**——K8s Events 绑定
/// 运行容器 Deployment，dev 开发环境无对应能力。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/events",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `prod`（运行容器 K8s Events）"),
    ),
    description = r#"
查运行容器的 Kubernetes Events（Pod 调度 / 拉取 / 启动 / 崩溃事件），用于
启动失败与重启排障。Docker 形态返回空列表。

> **仅 prod**：传 `app_stage=dev` 返回 400（开发环境无 Events 能力面）。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<container_runtime_api::AppEventInfo>>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_events(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
) -> Result<Json<HttpResult<Vec<container_runtime_api::AppEventInfo>>>, AppError> {
    if shared_types::UserappStage::parse(&app_stage) != Some(shared_types::UserappStage::Prod) {
        return Err(AppError::validation_error(
            "`events` is a prod-runtime capability: pass app_stage=prod (dev environment has no k8s events)",
        ));
    }
    info!("[APP] getting app events: {}", app_id);
    let events = state.app_service.get_app_events(&app_id).await?;
    Ok(Json(HttpResult::success(events)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// readiness 的 user_id 必填口径（userapp 业务）：标识符白名单拒绝
    /// 路径逃逸与空值；缺失由 Query 反序列化直接 400。
    #[test]
    fn readiness_params_user_id_must_be_identifier_shaped() {
        let mut params = ReadinessParams {
            user_id: "1754545591".to_string(),
        };
        assert!(params.validate().is_ok());

        for invalid in ["", "../escape", "a/b"] {
            params.user_id = invalid.to_string();
            assert!(params.validate().is_err(), "user_id={invalid:?} 应被拒绝");
        }
    }
}
