//! Computer Agent Runner 域路由（从 router.rs 拆出）。

use std::sync::Arc;

use crate::router::AppState;
use axum::Router;
use axum::routing::{get, post};

use crate::handler;

pub(super) fn computer_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/computer/chat", post(handler::handle_computer_chat))
        .route("/computer/agent/stop", post(handler::computer_agent_stop))
        .route(
            "/computer/agent/status",
            post(handler::computer_agent_status),
        ) // 🆕 新增
        .route(
            "/computer/agent/session/cancel",
            post(handler::computer_agent_session_cancel),
        )
        .route(
            "/computer/notify-resolved",
            post(handler::computer_notify_resolved),
        )
        // 进度流复用现有的 agent_session_notification
        .route(
            "/computer/progress/{session_id}",
            get(handler::computer_agent_progress_notification),
        )
        // VNC 桌面访问说明接口
        .route(
            "/computer/desktop/{user_id}/{project_id}",
            get(handler::computer_desktop_vnc),
        )
        .route(
            "/computer/desktop-proxy/{user_id}/{project_id}/{*path}",
            get(handler::computer_desktop_proxy),
        )
        .route(
            "/computer/ttyd/{user_id}/{project_id}/{*path}",
            get(handler::computer_ttyd_proxy),
        )
        // Pod 容器管理接口
        .route("/computer/pod/count", get(handler::pod_count))
        .route("/computer/pod/list", get(handler::pod_list))
        .route("/computer/pod/ensure", post(handler::pod_ensure))
        .route("/computer/pod/keepalive", post(handler::pod_keepalive))
        .route("/computer/pod/restart", post(handler::pod_restart))
        .route("/computer/pod/status", get(handler::pod_status))
        .route("/computer/pod/vnc-status", get(handler::pod_vnc_status))
        // 🆕 音频代理路由（用于 OpenAPI 文档）
        .route(
            "/computer/audio/{user_id}/{project_id}/{*path}",
            get(handler::computer_audio_proxy),
        )
        // 🆕 IME 代理路由（用于 OpenAPI 文档）
        .route(
            "/computer/ime/{user_id}/{project_id}/{*path}",
            get(handler::computer_ime_proxy),
        )
        // 🆕 Computer Agent-runner 容器 PG 管理（重置密码 / 建库; rcoder exec 容器内 psql）
        .route(
            "/computer/db/{user_id}/reset-password",
            post(handler::computer_db_reset_password),
        )
        .route(
            "/computer/db/{user_id}/create-database",
            post(handler::computer_db_create_database),
        )
        .route("/computer/cache/clean", post(handler::computer_cache_clean))
        .with_state(state)
}
