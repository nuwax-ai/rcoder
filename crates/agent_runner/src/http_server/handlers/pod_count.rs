//! Pod 容器数量查询（兼容接口）
//!
//! agent_runner 作为单个容器实例，固定返回数量 1。

use axum::Json;
use shared_types::{HttpResult, PodCountByServiceType, PodCountResponse};

/// 容器数量查询（兼容接口，agent_runner 固定返回 1）
///
/// agent_runner 运行在单个容器中，因此始终返回 1 个 computer_agent_runner 类型容器。
#[utoipa::path(
    get,
    path = "/computer/pod/count",
    responses(
        (status = 200, description = "容器数量统计", body = HttpResult<PodCountResponse>)
    ),
    tag = "pod"
)]
pub async fn handle_pod_count() -> Json<HttpResult<PodCountResponse>> {
    let response = PodCountResponse {
        total_count: 1,
        by_service_type: PodCountByServiceType {
            rcoder: 0,
            computer_agent_runner: 1,
        },
        timestamp: chrono::Utc::now().timestamp_millis() as u64,
    };
    Json(HttpResult::success(response))
}
