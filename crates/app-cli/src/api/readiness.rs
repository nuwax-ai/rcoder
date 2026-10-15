//! `GET /v1/app/readiness` —— 业务就绪观察（只读）。
//!
//! 与 `/health`（liveness 探针）和 `/ready`（readiness 探针）分离：本端点
//! 回答「声明的服务集合 + Pingap 入口是否满足健康契约」，供 rcoder
//! `/{app_id}/{app_stage}/readiness` 转发消费。响应恒 200（观察完成），
//! `data.ready` 才表示业务可用——未就绪/失败/停止/不支持都是合法观察结果。
//!
//! 初始化恢复期照常应答（starting），不等待 kernel 恢复完成；探测不回写
//! 探针状态、不启动/停止任何进程。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use shared_types::UserAppBusinessReadiness;

use super::AppState;
use super::envelope;
use super::envelope::HttpResult;

#[utoipa::path(
    get,
    path = "/v1/app/readiness",
    responses(
        (status = 200, body = HttpResult<UserAppBusinessReadiness>, description = "Business readiness observation completed (data.ready indicates business availability; not-ready/failed/stopped/unsupported are all valid observations)"),
    ),
    tag = "Runtime Readiness"
)]
pub(super) async fn readiness(State(state): State<AppState>) -> Response {
    let observed = state.readiness.observe().await;
    envelope::ok(StatusCode::OK, observed.as_ref().clone())
}
