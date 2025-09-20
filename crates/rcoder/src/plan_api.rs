use crate::{SharedState, HttpResult, get_trace_id};
use acp_adapter::plan::{PlanManager, PlanUpdateEvent};
use acp_adapter::types::{Plan, PlanStats, PlanEntryStatus, PlanEntryPriority};
use axum::{
    extract::{Path, State, Query},
    response::{Json, Sse, sse::Event},
};
use serde::{Deserialize, Serialize};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use tracing::{info, warn, error};
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

/// Plan状态更新请求
#[derive(Debug, Deserialize)]
pub struct UpdateEntryStatusRequest {
    pub entry_id: String,
    pub status: PlanEntryStatus,
}

/// 获取指定会话的Plan
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

/// 获取所有活跃Plan的统计信息
pub async fn get_all_plans_stats(
    State(state): State<SharedState>,
) -> HttpResult<HashMap<String, PlanStats>> {
    let trace_id = get_trace_id();
    
    info!("Getting all plans stats, trace_id={:?}", trace_id);
    
    // 从PlanManager获取所有活跃plan的统计信息
    let stats = state.plan_manager.list_active_plans().await;
    
    HttpResult::success(stats, trace_id)
}

/// 更新Plan条目状态
pub async fn update_entry_status(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
    Json(request): Json<UpdateEntryStatusRequest>,
) -> HttpResult<PlanResponse> {
    let trace_id = get_trace_id();
    
    info!(
        "Updating entry {} status to {:?} for session: {}, trace_id={:?}", 
        request.entry_id, request.status, session_id, trace_id
    );
    
    // 实际更新条目状态
    match state.plan_manager.update_entry_status(&session_id, &request.entry_id, request.status).await {
        Ok(_) => {
            let plan = state.plan_manager.get_plan(&session_id).await;
            let stats = state.plan_manager.get_plan_stats(&session_id).await;
            
            let plan_response = PlanResponse {
                session_id: session_id.clone(),
                plan,
                stats,
            };
            
            HttpResult::success(plan_response, trace_id)
        }
        Err(e) => {
            error!("Failed to update entry status: {}", e);
            HttpResult::internal_error(&format!("Failed to update entry status: {}", e), trace_id)
        }
    }
}

/// 清理已完成的Plan条目
pub async fn cleanup_completed_entries(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> HttpResult<PlanResponse> {
    let trace_id = get_trace_id();
    
    info!("Cleaning up completed entries for session: {}, trace_id={:?}", session_id, trace_id);
    
    // 实际清理已完成条目
    match state.plan_manager.cleanup_completed_entries(&session_id).await {
        Ok(_) => {
            let plan = state.plan_manager.get_plan(&session_id).await;
            let stats = state.plan_manager.get_plan_stats(&session_id).await;
            
            let plan_response = PlanResponse {
                session_id: session_id.clone(),
                plan,
                stats,
            };
            
            HttpResult::success(plan_response, trace_id)
        }
        Err(e) => {
            error!("Failed to cleanup completed entries: {}", e);
            HttpResult::internal_error(&format!("Failed to cleanup completed entries: {}", e), trace_id)
        }
    }
}

/// Plan实时更新SSE端点
pub async fn plan_updates_stream(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> Sse<impl futures::Stream<Item = Result<Event, axum::Error>>> {
    info!("New plan updates SSE connection for session: {}", session_id);
    
    // 从PlanManager订阅更新
    let update_rx = state.plan_manager.subscribe_updates().await;
    
    // 创建stream
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

/// 创建一个演示Plan（用于测试）
pub async fn create_demo_plan(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> HttpResult<PlanResponse> {
    let trace_id = get_trace_id();
    
    info!("Creating demo plan for session: {}, trace_id={:?}", session_id, trace_id);
    
    // 创建演示Plan
    let mut plan = Plan::new();
    plan.add_entry("分析用户需求".to_string(), acp_adapter::types::PlanEntryPriority::High);
    plan.add_entry("设计系统架构".to_string(), acp_adapter::types::PlanEntryPriority::Normal);
    plan.add_entry("实现核心功能".to_string(), acp_adapter::types::PlanEntryPriority::Normal);
    plan.add_entry("编写单元测试".to_string(), acp_adapter::types::PlanEntryPriority::Low);
    plan.add_entry("部署到生产环境".to_string(), acp_adapter::types::PlanEntryPriority::High);
    
    // 将plan保存到PlanManager
    match state.plan_manager.update_plan(&session_id, plan.clone()).await {
        Ok(_) => {
            let stats = plan.stats();
            
            let plan_response = PlanResponse {
                session_id: session_id.clone(),
                plan: Some(plan),
                stats: Some(stats),
            };
            
            HttpResult::success(plan_response, trace_id)
        }
        Err(e) => {
            error!("Failed to create demo plan: {}", e);
            HttpResult::internal_error(&format!("Failed to create demo plan: {}", e), trace_id)
        }
    }
}

/// Plan路由配置
pub fn plan_routes() -> axum::Router<SharedState> {
    axum::Router::new()
        .route("/api/plans/{session_id}", axum::routing::get(get_plan))
        .route("/api/plans/{session_id}/status", axum::routing::put(update_entry_status))
        .route("/api/plans/{session_id}/cleanup", axum::routing::post(cleanup_completed_entries))
        .route("/api/plans/{session_id}/demo", axum::routing::post(create_demo_plan))
        .route("/api/plans/{session_id}/updates", axum::routing::get(plan_updates_stream))
        .route("/api/plans/stats", axum::routing::get(get_all_plans_stats))
}