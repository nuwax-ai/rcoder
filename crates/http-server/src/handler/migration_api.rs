//! per-agent workspace 迁移编排 API（测试期门面）。
//!
//! 边界（重要）：本 API 是**测试阶段**用的编排入口——env
//! `RCODER_MIGRATION_TEST_USERS`（逗号分隔）白名单外的 user_id 一律 403。
//! 生产批量迁移由 rcoder 内部的迁移编排器驱动（P2 后续），不经 HTTP。
//!
//! 端点：
//! - `GET  /api/migration/{user_id}/status`           查 PVC/标记/agent 卷形态
//! - `POST /api/migration/{user_id}/migration-state`  翻标记（body `{state}`）
//!
//! 数据流（两阶段迁移, 详见 k8s/migration README 与 plan §5.6）：
//! ```text
//! phase-a: 外部工具(rclone Job)建 PVC + 全量 copy —— **不翻标记**
//! cutover: 停 agent → 增量 sync → POST migration-state {migrated} → 起 agent
//! rollback: POST migration-state {not-migrated} → 用户回共享卷
//! ```
//! 判据层（转发路由 + agent 卷选择）以 `migrated` 标记为唯一开关——
//! phase-a 后标记未翻，用户照常走共享卷，预同步期双份共存不裂。

use std::collections::HashSet;
use std::sync::OnceLock;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::router::AppState;
use shared_types::ServiceType;

/// 白名单（env `RCODER_MIGRATION_TEST_USERS`, 逗号分隔; 未设 = 空 = 全部拒绝）。
fn allowed_users() -> &'static HashSet<String> {
    static USERS: OnceLock<HashSet<String>> = OnceLock::new();
    USERS.get_or_init(|| {
        std::env::var("RCODER_MIGRATION_TEST_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn ensure_allowed(user_id: &str) -> Option<Response> {
    if allowed_users().contains(user_id) {
        return None;
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            format!(
                "user '{user_id}' is not in RCODER_MIGRATION_TEST_USERS \
                 (migration API is test-phase only)"
            ),
        )
            .into_response(),
    )
}

/// GET /api/migration/{user_id}/status
pub async fn status(
    State(state): State<std::sync::Arc<AppState>>,
    Path(user_id): Path<String>,
) -> Response {
    if let Some(resp) = ensure_allowed(&user_id) {
        return resp;
    }
    let service_type = ServiceType::ComputerAgentRunner;
    let migration_state = match state
        .runtime()
        .workspace_migration_state(&user_id, &service_type)
        .await
    {
        Ok(s) => s,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("query migration state failed: {error}"),
            )
                .into_response();
        }
    };
    let pvc_exists = matches!(
        state
            .runtime()
            .workspace_pvc_exists(&user_id, &service_type)
            .await,
        Ok(true)
    );
    let body = serde_json::json!({
        "user_id": user_id,
        "service_type": service_type.to_string(),
        "pvc_exists": pvc_exists,
        "migration_state": match migration_state {
            shared_types::WorkspaceMigrationState::NotMigrated => "not-migrated",
            shared_types::WorkspaceMigrationState::Migrated => "migrated",
        },
        "routing": match migration_state {
            shared_types::WorkspaceMigrationState::Migrated => "rbd-forward",
            _ => "shared-volume",
        },
    });
    Json(body).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationStateBody {
    state: String,
}

/// POST /api/migration/{user_id}/migration-state  body: `{"state":"migrated"|"not-migrated"}`
pub async fn set_state(
    State(state): State<std::sync::Arc<AppState>>,
    Path(user_id): Path<String>,
    Json(body): Json<MigrationStateBody>,
) -> Response {
    if let Some(resp) = ensure_allowed(&user_id) {
        return resp;
    }
    let target = match body.state.as_str() {
        "migrated" => shared_types::WorkspaceMigrationState::Migrated,
        "not-migrated" => shared_types::WorkspaceMigrationState::NotMigrated,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid state '{other}' (expected migrated|not-migrated)"),
            )
                .into_response();
        }
    };
    match state
        .runtime()
        .set_workspace_migration_state(&user_id, &ServiceType::ComputerAgentRunner, target)
        .await
    {
        Ok(()) => Json(serde_json::json!({
            "user_id": user_id,
            "migration_state": body.state,
            "updated": true,
        }))
        .into_response(),
        Err(container_runtime_api::ContainerRuntimeError::ContainerNotFound(message)) => {
            (StatusCode::CONFLICT, message).into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            format!("set migration state failed: {error}"),
        )
            .into_response(),
    }
}

/// 路由组装（与 userapp_forward 等族平行; 挂主 Router, 调用处 with_state）。
pub fn routes() -> axum::Router<std::sync::Arc<AppState>> {
    axum::Router::new()
        .route(
            "/api/migration/{user_id}/status",
            axum::routing::get(status),
        )
        .route(
            "/api/migration/{user_id}/migration-state",
            axum::routing::post(set_state),
        )
}
