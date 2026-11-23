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
//! - `GET  /api/migration/{user_id}/status`            查 PVC/标记/agent 是否存在
//! - `POST /api/migration/{user_id}/phase-a`           建 PVC + 全量预同步（不翻标记）
//! - `POST /api/migration/{user_id}/cutover`           停 agent → 增量 → 翻标记 → 恢复 agent
//! - `POST /api/migration/{user_id}/rollback`          翻回共享卷 → 恢复 agent
//! - `POST /api/migration/{user_id}/migration-state`   只翻标记（body `{state}`；排障用）
//!
//! 编排（原子完成；失败语义与已知窗口见 k8s/migration README §10）在
//! `computer_forward::orchestrate`：
//! ```text
//! phase-a: ensure PVC → sync(Full)                                  → NotMigrated
//! cutover: stop agent → 等 detach → sync(Incremental) → set Migrated → 恢复 agent
//! rollback: stop agent → set NotMigrated → 恢复 agent（共享卷）
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

/// 入口准入（五个端点共用，先便宜后贵）。返回 `Some(响应)` = 请求被拒。
///
/// 1. `user_id` 形态：全仓统一的标识符白名单（与 file-server 解析路径、迁移
///    Job 的源路径校验是同一份规则），外加迁移 Job 的命名限制（见
///    [`invalid_user_id`]）。白名单为 `*` 的批量窗口里任何字符串都能到这里；
///    不拦的话，非法值要到 cutover **停完 agent** 之后才在同步步骤被拒，留下
///    停机残留。
/// 2. 必须是 Kubernetes 运行时。
/// 3. 用户白名单。
fn ensure_allowed(user_id: &str) -> Option<Response> {
    invalid_user_id(user_id)?;
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

/// `user_id` 路径参数的形态校验（fail fast）。文案默认英文；用 `{:?}` 回显
/// 原始入参，控制字符被转义，不会污染日志或终端。
///
/// 两段规则：标识符白名单（非法字符/长度），以及"能拼出合法的迁移 Job 名与
/// 标签值"（小写、≤43 位、首尾字母数字）。后者不过关的 id 本可以建 PVC，
/// 但 Job 建不出来——同样必须在停 agent 之前拒绝。
fn invalid_user_id(user_id: &str) -> Option<Response> {
    if shared_types::validate_identifier(user_id, "userId").is_err() {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid user_id {user_id:?}: \
                     expected 1-64 characters of letters, digits, '_' or '-'"
                ),
            )
                .into_response(),
        );
    }
    Some(
        (
            StatusCode::BAD_REQUEST,
            shared_types::validate_migration_user_id(user_id).err()?,
        )
            .into_response(),
    )
}

/// 运行时/编排错误 → HTTP 状态：走全仓统一映射（运行时变体 → 错误码 →
/// 状态），本模块不另写一份。前置不满足（已迁移、门禁被占、PVC 不存在）是
/// 4xx，"稍后重试"是 503，超时 504，部署配置问题与上游失败是 5xx。
fn status_for(error: &container_runtime_api::ContainerRuntimeError) -> StatusCode {
    shared_types::error_status_from_code(container_runtime_api::runtime_error_code(error).0)
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
        Err(error) => (
            status_for(&error),
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
        Err(error) => (status_for(&error), format!("phase-a failed: {error}")).into_response(),
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
        Err(error) => (status_for(&error), format!("cutover failed: {error}")).into_response(),
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
        Err(error) => (status_for(&error), format!("rollback failed: {error}")).into_response(),
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
    use container_runtime_api::ContainerRuntimeError;

    fn allowed(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// 反例：白名单为 `*` 的批量窗口里，非法 `user_id` 此前可以一路走到编排，
    /// 到 cutover 停完 agent 之后才在同步步骤被拒。必须在入口就 400。
    #[test]
    fn malformed_user_id_is_rejected_at_the_boundary() {
        for id in [
            "", " ", "a/b", "../etc", "a.b", "u;id", "u\n", "用户", "u id",
        ] {
            let response = invalid_user_id(id).unwrap_or_else(|| panic!("{id:?} 必须在入口被拒"));
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{id:?}");
        }
        for id in ["6", "mig-u1", "user_42", "rbd-v2-u1", &"u".repeat(43)] {
            assert!(invalid_user_id(id).is_none(), "{id:?} 是合法 user_id");
        }
    }

    /// 反例（部署评审）：这些 id 能通过标识符白名单，却拼不出合法的迁移 Job 名
    /// 或标签值（对象名只认小写、≤63 位；标签值首尾必须是字母数字）。不拦的话
    /// cutover 会先停 agent、再在建 Job 时被 apiserver 拒绝（422）。
    #[test]
    fn user_ids_that_cannot_name_the_migration_job_are_rejected_at_the_boundary() {
        for id in ["ABC123", "-mig", "mig-", "_mig", "mig_", &"u".repeat(44)] {
            let response = invalid_user_id(id).unwrap_or_else(|| panic!("{id:?} 必须在入口被拒"));
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{id:?}");
        }
    }

    /// 此前编排错误一律 502：调用方分不清"重试有用"与"重试没用"。
    #[test]
    fn runtime_errors_map_to_actionable_status_codes() {
        let cases = [
            (
                ContainerRuntimeError::Conflict(String::new()),
                StatusCode::CONFLICT,
            ),
            (
                ContainerRuntimeError::ContainerNotFound(String::new()),
                StatusCode::NOT_FOUND,
            ),
            (
                ContainerRuntimeError::ConnectionError(String::new()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ContainerRuntimeError::Timeout(String::new()),
                StatusCode::GATEWAY_TIMEOUT,
            ),
            (
                ContainerRuntimeError::ConfigurationError(String::new()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ContainerRuntimeError::K8sError(String::new()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(status_for(&error), expected, "{error:?}");
        }
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
