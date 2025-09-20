use crate::{SharedState, HttpResult, get_trace_id};
use acp_adapter::types::{Plan, PlanStats};
use axum::{
    extract::{Path, State, Query},
    response::{Sse, sse::Event},
};
use serde::{Deserialize, Serialize};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use tracing::{info, error};
use std::collections::HashMap;

/// Plan API响应结构
#[derive(Debug, Serialize)]
pub struct PlanResponse {
    pub session_id: String,
    pub plan: Option<Plan>,
    pub stats: Option<PlanStats>,
}

/// Plan查询参数
#[derive(Debug, Deserialize)]
pub struct PlanQuery {
    pub include_stats: Option<bool>,
}

/// 获取指定会话的Plan - 供前端查询使用
pub async fn get_plan(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
    Query(query): Query<PlanQuery>,
) -> HttpResult<PlanResponse> {
    let trace_id = get_trace_id();
    
    info!("Getting plan for session: {}, trace_id={:?}", session_id, trace_id);
    
    // 从 PlanManager 获取 Plan
    let plan = state.plan_manager.get_plan(&session_id).await;
    let stats = if query.include_stats.unwrap_or(true) {
        state.plan_manager.get_plan_stats(&session_id).await
    } else {
        None
    };
    
    let plan_response = PlanResponse {
        session_id: session_id.clone(),
        plan,
        stats,
    };
    
    HttpResult::success(plan_response, trace_id)
}

/// 获取所有活跃Plan的统计信息 - 供前端监控使用
pub async fn get_all_plans_stats(
    State(state): State<SharedState>,
) -> HttpResult<HashMap<String, PlanStats>> {
    let trace_id = get_trace_id();
    
    info!("Getting all plans stats, trace_id={:?}", trace_id);
    
    // 从PlanManager获取所有活跃plan的统计信息
    let stats = state.plan_manager.list_active_plans().await;
    
    HttpResult::success(stats, trace_id)
}

/// Plan实时更新SSE端点 - 核心功能：推送agent生成的Plan更新给前端
pub async fn plan_updates_stream(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> Sse<impl futures::Stream<Item = Result<Event, axum::Error>>> {
    info!("New plan updates SSE connection for session: {}", session_id);
    
    // 从PlanManager订阅更新
    let update_rx = state.plan_manager.subscribe_updates().await;
    
    // 创建stream，只推送指定session的Plan更新
    let stream = UnboundedReceiverStream::new(update_rx)
        .filter_map(move |event| {
            // 只返回指定session的事件
            if event.session_id == session_id {
                let json_data = serde_json::to_string(&event).unwrap_or_default();
                Some(Ok(Event::default()
                    .event("plan-update")
                    .data(&json_data)))
            } else {
                None
            }
        });
    
    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(std::time::Duration::from_secs(30))
                .text("keep-alive")
        )
}

/// Plan路由配置 - 只提供查询和SSE推送，Plan由agent自动维护
pub fn plan_routes() -> axum::Router<SharedState> {
    axum::Router::new()
        // 获取Plan详情 - 前端查询用
        .route("/api/plans/{session_id}", axum::routing::get(get_plan))
        // 获取所有Plan统计 - 前端监控用
        .route("/api/plans/stats", axum::routing::get(get_all_plans_stats))
        // SSE实时推送Plan更新 - 核心功能
        .route("/api/plans/{session_id}/updates", axum::routing::get(plan_updates_stream))
}