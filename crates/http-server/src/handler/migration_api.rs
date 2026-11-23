//! per-agent workspace 迁移编排 API。
//!
//! 边界（重要）：本 API 是运维/测试用的编排入口，受两层约束——
//! 1. env `RCODER_MIGRATION_TEST_USERS`（逗号分隔）白名单，名单外的
//!    user_id 一律 403；值为 `*` 时对全部用户开放（夜间批量迁移窗口用，
//!    平时应留空）。批量脚本 `k8s/migration/migrate-batch.sh MODE=api` 经本
//!    API 驱动 phase-a / cutover / rollback。
//! 2. 启用 API key 鉴权的部署上，本路由与其他 `/api` 路由同受全局 x-api-key
//!    保护（不在放行名单）。
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

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::router::AppState;
use shared_types::ServiceType;

/// 白名单通配值：对全部用户开放（批量迁移窗口）。
const ALLOW_ALL: &str = "*";

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

fn user_allowed(allowed: &HashSet<String>, user_id: &str) -> bool {
    allowed.contains(user_id) || allowed.contains(ALLOW_ALL)
}

fn ensure_allowed(user_id: &str) -> Option<Response> {
    // Docker Compose 形态: 迁移编排只对 K8s 有意义(trait 默认 sync=Err),
    // 显式拒绝防误设(白名单与 flag 双误设时 cutover 会真停容器然后失败)。
    let kubernetes_runtime = shared_types::is_kubernetes_runtime()
        && docker_manager::runtime_selection::RuntimeType::from_env().is_kubernetes();
    if !kubernetes_runtime {
        return Some(
            (
                StatusCode::NOT_IMPLEMENTED,
                "workspace migration requires the Kubernetes runtime",
            )
                .into_response(),
        );
    }
    if user_allowed(allowed_users(), user_id) {
        return None;
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            format!(
                "user '{user_id}' is not in RCODER_MIGRATION_TEST_USERS \
                 (migration API is restricted to that list)"
            ),
        )
            .into_response(),
    )
}

/// GET /api/migration/{user_id}/status
///
/// `agent_present`：agent 工作负载存在（运行中或启动中）。批量脚本据此判断
/// 用户是否活跃——闲置回收后 agent 不在；存在即视为活跃，跳过本轮切换。
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
    // 查询失败按未知上抛（502）：误报“不存在”会让批量脚本切到活跃用户。
    let agent_present = match state
        .runtime()
        .find_container(&user_id, &service_type)
        .await
    {
        Ok(presence) => presence.is_some(),
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("query agent presence failed: {error}"),
            )
                .into_response();
        }
    };
    let body = serde_json::json!({
        "user_id": user_id,
        "service_type": service_type.to_string(),
        "pvc_exists": pvc_exists,
        "agent_present": agent_present,
        "migration_state": match migration_state {
            shared_types::WorkspaceMigrationState::NotMigrated => "not-migrated",
            shared_types::WorkspaceMigrationState::Migrated => "migrated",
        },
        "routing": match migration_state {
            // 穷尽匹配：WorkspaceMigrationState 只有两态，写 `_ =>` 会让
            // 第三态被静默当成共享卷（评审"规范 1"）。
            shared_types::WorkspaceMigrationState::Migrated => "rbd-forward",
            shared_types::WorkspaceMigrationState::NotMigrated => "shared-volume",
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

/// POST /api/migration/{user_id}/phase-a —— 建 PVC + 全量预同步（不停机,
/// 不翻标记; 用户照常共享卷读写）。
pub async fn phase_a(
    State(state): State<std::sync::Arc<AppState>>,
    Path(user_id): Path<String>,
) -> Response {
    if let Some(resp) = ensure_allowed(&user_id) {
        return resp;
    }
    let src_pvc = migration_src_pvc();
    match crate::computer_forward::orchestrate::phase_a(&state, &user_id, &src_pvc).await {
        Ok(outcome) => Json(serde_json::json!({
            "user_id": outcome.user_id,
            "action": outcome.action,
            "migration_state": "not-migrated",
            "steps": outcome.steps,
        }))
        .into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, format!("phase-a failed: {error}")).into_response(),
    }
}

/// POST /api/migration/{user_id}/cutover —— 停 agent → 防呆等 detach →
/// 增量 sync → 翻 Migrated → 恢复 agent 切换前形态（原子编排; 失败即整体失败）。
pub async fn cutover(
    State(state): State<std::sync::Arc<AppState>>,
    Path(user_id): Path<String>,
) -> Response {
    if let Some(resp) = ensure_allowed(&user_id) {
        return resp;
    }
    let src_pvc = migration_src_pvc();
    match crate::computer_forward::orchestrate::cutover(&state, &user_id, &src_pvc).await {
        Ok(outcome) => Json(serde_json::json!({
            "user_id": outcome.user_id,
            "action": outcome.action,
            "migration_state": "migrated",
            "steps": outcome.steps,
        }))
        .into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, format!("cutover failed: {error}")).into_response(),
    }
}

/// POST /api/migration/{user_id}/rollback —— 翻回 NotMigrated（共享卷数据
/// 全程保留; 下一次 agent 重建即回共享卷）。
pub async fn rollback(
    State(state): State<std::sync::Arc<AppState>>,
    Path(user_id): Path<String>,
) -> Response {
    if let Some(resp) = ensure_allowed(&user_id) {
        return resp;
    }
    match crate::computer_forward::orchestrate::rollback(&state, &user_id).await {
        Ok(outcome) => Json(serde_json::json!({
            "user_id": outcome.user_id,
            "action": outcome.action,
            "migration_state": "not-migrated",
            "steps": outcome.steps,
        }))
        .into_response(),
        Err(error) => {
            (StatusCode::BAD_GATEWAY, format!("rollback failed: {error}")).into_response()
        }
    }
}

/// 源共享 PVC 名（env `RCODER_MIGRATION_SRC_PVC`; 缺省回退 computer 共享卷名）。
fn migration_src_pvc() -> String {
    std::env::var("RCODER_MIGRATION_SRC_PVC")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("RCODER_COMPUTER_WORKSPACE_PVC_NAME").ok())
        .unwrap_or_else(|| "rcoder-computer-workspace".to_string())
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
        .route(
            "/api/migration/{user_id}/phase-a",
            axum::routing::post(phase_a),
        )
        .route(
            "/api/migration/{user_id}/cutover",
            axum::routing::post(cutover),
        )
        .route(
            "/api/migration/{user_id}/rollback",
            axum::routing::post(rollback),
        )
}

#[cfg(test)]
mod user_allowed_tests {
    use super::*;

    fn allowed(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn unset_list_rejects_everyone() {
        assert!(!user_allowed(&allowed(&[]), "u1"));
    }

    #[test]
    fn listed_user_passes_and_others_do_not() {
        let list = allowed(&["u1", "u2"]);
        assert!(user_allowed(&list, "u1"));
        assert!(!user_allowed(&list, "u3"));
    }

    #[test]
    fn wildcard_opens_all_users_for_the_batch_window() {
        let list = allowed(&["*"]);
        assert!(user_allowed(&list, "anyone"));
    }
}
