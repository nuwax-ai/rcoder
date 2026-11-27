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
//! - `POST /api/migration/{user_id}/backfill`           给已迁移用户的卷补缺失的文件（body `{paths, only}`）
//!
//! 编排（原子完成；失败语义与已知窗口见 k8s/migration README §10）在
//! `computer_forward::orchestrate`：
//! ```text
//! phase-a: ensure PVC → sync(Full)                                  → NotMigrated
//! cutover: stop agent → 等 detach → sync(Incremental) → set Migrated → 恢复 agent
//! rollback: stop agent → set NotMigrated → 恢复 agent（共享卷）
//! backfill: stop agent → 等 detach → 只补缺失的拷贝 + 软链校验 → 恢复 agent（标记不变）
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

/// 入口准入（六个端点共用，先便宜后贵）。返回 `Err(响应)` = 请求被拒。
///
/// 1. `user_id` 形态：全仓统一的标识符白名单（与 file-server 解析路径、迁移
///    Job 的源路径校验是同一份规则），外加迁移 Job 的命名限制（见
///    [`invalid_user_id`]）。白名单为 `*` 的批量窗口里任何字符串都能到这里；
///    不拦的话，非法值要到 cutover **停完 agent** 之后才在同步步骤被拒，留下
///    停机残留。
/// 2. 用户白名单。
/// 3. 必须是 Kubernetes 运行时。
///
/// 两个守卫都返回 `Result<(), Box<Response>>`（`Err` = 拒绝；装箱是为了过
/// `result_large_err`），不返回 `Option<Response>`：
/// 守卫曾经是 `Option`，调用处写成 `invalid_user_id(user_id)?;`——在同样返回
/// `Option<Response>` 的函数里，`?` 遇到 `None`（id 合法）是**提前返回 `None`，即
/// 放行**，并跳过后面所有检查；遇到 `Some`（id 非法）反而解包丢弃、继续往下走。
/// 结果合法 id 一律放行、白名单形同虚设（2026-11-24 在 31.45 上实测到的回归）。
/// 用 `Result` 之后，`?` 与 `Err` 的语义一致，这类反写不再能编译出错误行为。
fn ensure_allowed(user_id: &str) -> Result<(), Box<Response>> {
    invalid_user_id(user_id)?;
    if !user_allowed(allowed_users(), user_id) {
        return Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                format!(
                    "user '{user_id}' is not in RCODER_MIGRATION_TEST_USERS \
                     (migration API is restricted to that list)"
                ),
            )
                .into_response(),
        ));
    }
    // Docker Compose 形态: 迁移编排只对 K8s 有意义(trait 默认 sync=Err),
    // 显式拒绝防误设(白名单与 flag 双误设时 cutover 会真停容器然后失败)。
    let kubernetes_runtime = shared_types::is_kubernetes_runtime()
        && docker_manager::runtime_selection::RuntimeType::from_env().is_kubernetes();
    if !kubernetes_runtime {
        return Err(Box::new(
            (
                StatusCode::NOT_IMPLEMENTED,
                "workspace migration requires the Kubernetes runtime",
            )
                .into_response(),
        ));
    }
    Ok(())
}

/// `user_id` 路径参数的形态校验（fail fast）。文案默认英文；用 `{:?}` 回显
/// 原始入参，控制字符被转义，不会污染日志或终端。
///
/// 两段规则：标识符白名单（非法字符/长度），以及"能拼出合法的迁移 Job 名与
/// 标签值"（小写、≤43 位、首尾字母数字）。后者不过关的 id 本可以建 PVC，
/// 但 Job 建不出来——同样必须在停 agent 之前拒绝。
fn invalid_user_id(user_id: &str) -> Result<(), Box<Response>> {
    if shared_types::validate_identifier(user_id, "userId").is_err() {
        return Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid user_id {user_id:?}: \
                     expected 1-64 characters of letters, digits, '_' or '-'"
                ),
            )
                .into_response(),
        ));
    }
    if let Err(reason) = shared_types::validate_migration_user_id(user_id) {
        return Err(Box::new((StatusCode::BAD_REQUEST, reason).into_response()));
    }
    Ok(())
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
    if let Err(response) = ensure_allowed(&user_id) {
        return *response;
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
    // 不用 `Json<T>` 提取器：提取器在函数体之前运行，请求体不合法时名单外的调用方
    // 会先拿到 400/422 而不是 403（31.45 实测）。先过准入，再解析请求体。
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = ensure_allowed(&user_id) {
        return *response;
    }
    // 解析错误里只有位置信息，不回显请求体内容
    let body: MigrationStateBody = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid body ({err}): expected {{\"state\":\"migrated\"|\"not-migrated\"}}"
                ),
            )
                .into_response();
        }
    };
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
    if let Err(response) = ensure_allowed(&user_id) {
        return *response;
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
    if let Err(response) = ensure_allowed(&user_id) {
        return *response;
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
    if let Err(response) = ensure_allowed(&user_id) {
        return *response;
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

/// `POST /api/migration/{user_id}/backfill` 的请求体（两段都可省，省略即空数组）。
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct BackfillBody {
    /// 范围：用户目录下的相对路径，或两种通配写法——`.`（整棵树，切换后删掉的东西都会回来）、
    /// `*`（顶层非点开头的目录=项目，只取源与目标都有的）。空数组 = 顶层点目录里源与目标
    /// 都有的（pnpm 的版本切换目录与全局包、编辑器扩展…）。最多 64 个。
    pub paths: Vec<String>,
    /// 只补匹配这些模式的条目（相对范围之下任意层级，如 `node_modules/**`）。空数组 = 范围之内全部。
    /// 最多 16 个。
    pub only: Vec<String>,
}

/// `200` 的响应体。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct BackfillResponse {
    pub user_id: String,
    pub action: String,
    /// 补迁不翻标记：恒为 `migrated`。
    pub migration_state: String,
    /// 步骤轨迹（排障用）。
    pub steps: Vec<String>,
    /// 拷过的范围（相对用户目录）。
    pub copied: Vec<String>,
    /// 共享卷上没有、跳过的范围。
    pub skipped: Vec<String>,
}

/// 请求体 → 补迁请求：解析 + 校验（纯函数，供单测）。解析错误只带 serde 给的位置与原因，不回显
/// 请求体；校验错误带上出问题的那一项。文案都是单行（它原样就是 400 的响应体）。
fn parse_backfill_body(body: &[u8]) -> Result<shared_types::WorkspaceBackfill, String> {
    let body: BackfillBody = serde_json::from_slice(body)
        .map_err(|err| format!("invalid body ({err}): expected {{\"paths\":[…],\"only\":[…]}}"))?;
    let request = shared_types::WorkspaceBackfill {
        paths: body.paths,
        only: body.only,
    };
    request.validate()?;
    Ok(request)
}

/// 补迁：给已迁移用户的卷补缺失的文件
///
/// 停 agent → 等卷卸载 → 从共享卷只补目标卷上没有的文件（不覆盖、不删除）→ 对刚补过的
/// 范围做软链完整性校验 → 恢复 agent。不翻迁移标记。
#[utoipa::path(
    post,
    path = "/api/migration/{user_id}/backfill",
    tag = "migration",
    operation_id = "migration_backfill",
    params(("user_id" = String, Path, description = "已迁移到 per-agent RBD 卷的 user_id")),
    request_body = BackfillBody,
    responses(
        (status = 200, description = "补迁完成", body = BackfillResponse),
        (status = 400, description = "user_id 拼不出合法的迁移 Job 名，或请求体不合法：范围不是干净的相对路径、带了 `..`、数量超限…", body = String),
        (status = 403, description = "user_id 不在 RCODER_MIGRATION_TEST_USERS 白名单里", body = String),
        (status = 404, description = "该用户还没有 per-agent 卷（没跑过 phase-a）", body = String),
        (status = 409, description = "该用户未迁移（先跑 phase-a + cutover）", body = String),
        (status = 500, description = "Job 失败或超时：目标卷空间不够、补完仍有坏软链…原因带在返回里；目标卷上已有的文件没被动过，扩容/排障后重跑即可（幂等）", body = String),
        (status = 503, description = "同一用户的另一次编排还在收尾、agent 正在启动、在途写没排空、或读 Job 状态持续失败：稍后重试", body = String),
        (status = 501, description = "不是 Kubernetes 运行时", body = String),
    )
)]
pub async fn backfill(
    State(state): State<std::sync::Arc<AppState>>,
    Path(user_id): Path<String>,
    // 不用 `Json<T>` 提取器：原因同 set_state——准入先于请求体解析。
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = ensure_allowed(&user_id) {
        return *response;
    }
    let request = match parse_backfill_body(&body) {
        Ok(request) => request,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    let src_pvc = migration_src_pvc();
    match crate::computer_forward::orchestrate::backfill(&state, &user_id, &src_pvc, &request).await
    {
        Ok(outcome) => Json(BackfillResponse {
            user_id: outcome.migration.user_id,
            action: outcome.migration.action.to_string(),
            migration_state: "migrated".to_string(),
            steps: outcome.migration.steps,
            copied: outcome.report.copied,
            skipped: outcome.report.skipped,
        })
        .into_response(),
        Err(error) => (status_for(&error), format!("backfill failed: {error}")).into_response(),
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
        .route(
            "/api/migration/{user_id}/backfill",
            axum::routing::post(backfill),
        )
}

#[cfg(test)]
mod backfill_body_tests {
    use super::*;

    #[test]
    fn an_empty_body_is_the_default_scope() {
        assert_eq!(parse_backfill_body(b"{}"), Ok(Default::default()));
        assert_eq!(
            parse_backfill_body(br#"{"paths":["*"],"only":["node_modules/**","dist/**"]}"#),
            Ok(shared_types::WorkspaceBackfill {
                paths: vec!["*".to_string()],
                only: vec!["node_modules/**".to_string(), "dist/**".to_string()],
            })
        );
    }

    /// 反例：范围会作为字面量拼进 rclone 的放行规则——带 `..`、以 `/` 开头的值必须在入口就
    /// 400, 不能等到停完 agent 才暴露。
    #[test]
    fn escaping_or_malformed_scopes_are_rejected_before_any_side_effect() {
        for body in [
            r#"{"paths":["../other-user"]}"#,
            r#"{"paths":["/etc"]}"#,
            r#"{"paths":["proj/*/x"]}"#,
            r#"{"only":["../x/**"]}"#,
            // 写错的字段名（path / onlys）不能被当成"没给"静默放过
            r#"{"path":[".local"]}"#,
            r#"{"onlys":["dist/**"]}"#,
            r#"{"paths":".local"}"#,
            "not json",
            "",
        ] {
            let Err(reason) = parse_backfill_body(body.as_bytes()) else {
                panic!("{body:?} 必须被拒")
            };
            assert!(!reason.contains('\n'), "{body:?}: {reason}");
        }
    }

    #[test]
    fn too_many_scopes_or_patterns_are_rejected() {
        let paths = vec!["proj"; shared_types::validation::WORKSPACE_BACKFILL_MAX_PATHS + 1];
        let body = serde_json::json!({ "paths": paths }).to_string();
        let reason = parse_backfill_body(body.as_bytes()).unwrap_err();
        assert!(reason.contains("at most 64 paths"), "{reason}");
    }

    #[test]
    fn backfill_is_in_the_openapi_document() {
        let document = <crate::router_docs::ApiDoc as utoipa::OpenApi>::openapi();
        let operation = document
            .paths
            .paths
            .get("/api/migration/{user_id}/backfill")
            .and_then(|path| path.post.as_ref())
            .expect("补迁端点要登记进主文档");
        assert_eq!(
            operation.operation_id.as_deref(),
            Some("migration_backfill")
        );
        assert_eq!(operation.tags, Some(vec!["migration".to_string()]));
        let utoipa::openapi::RefOr::T(request) = operation.request_body.as_ref().expect("有请求体")
        else {
            panic!("请求体不该是引用");
        };
        let utoipa::openapi::RefOr::T(content) = &request.content["application/json"] else {
            panic!("application/json 的内容不该是引用");
        };
        assert!(
            matches!(content.schema, Some(utoipa::openapi::RefOr::Ref(_))),
            "请求体引用 BackfillBody schema"
        );
    }
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
            let Err(response) = invalid_user_id(id) else {
                panic!("{id:?} 必须在入口被拒")
            };
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{id:?}");
        }
        for id in ["6", "mig-u1", "user_42", "rbd-v2-u1", &"u".repeat(43)] {
            assert!(invalid_user_id(id).is_ok(), "{id:?} 是合法 user_id");
        }
    }

    /// 反例（部署评审）：这些 id 能通过标识符白名单，却拼不出合法的迁移 Job 名
    /// 或标签值（对象名只认小写、≤63 位；标签值首尾必须是字母数字）。不拦的话
    /// cutover 会先停 agent、再在建 Job 时被 apiserver 拒绝（422）。
    #[test]
    fn user_ids_that_cannot_name_the_migration_job_are_rejected_at_the_boundary() {
        for id in ["ABC123", "-mig", "mig-", "_mig", "mig_", &"u".repeat(44)] {
            let Err(response) = invalid_user_id(id) else {
                panic!("{id:?} 必须在入口被拒")
            };
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

/// 走真实路由的准入回归。
///
/// 反例（2026-11-24，31.45 实测）：守卫调用处曾写成 `invalid_user_id(user_id)?;`，
/// 合法 id 一律放行、白名单被跳过——名单外用户能查状态、能发起 cutover。当时的单测
/// 只直接调 `invalid_user_id`，函数本身是对的，调用方反了却全绿。所以这里的用例经过
/// 路由和五个真实 handler，不碰私有函数。
///
/// 白名单是进程级 `OnceLock`（首次调用时读 env），一个进程里只能验证一份名单：
/// 每个场景在子进程里跑——父进程用自己的可执行文件加 `--exact` 拉起本用例，并用
/// 环境变量选中场景。
#[cfg(test)]
mod guard_route_tests {
    use super::routes;
    use crate::handler::userapp_agent_target::test_support::ControlledRuntime;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use std::sync::{Arc, Mutex, atomic::AtomicUsize};
    use tower::ServiceExt as _;

    const CHILD_CASE: &str = "RCODER_MIGRATION_GUARD_TEST_CASE";
    const TEST_NAME: &str =
        "handler::migration_api::guard_route_tests::whitelist_gates_every_endpoint";
    const ENDPOINTS: &[(&str, &str)] = &[
        ("GET", "/api/migration/{u}/status"),
        ("POST", "/api/migration/{u}/migration-state"),
        ("POST", "/api/migration/{u}/phase-a"),
        ("POST", "/api/migration/{u}/cutover"),
        ("POST", "/api/migration/{u}/rollback"),
        ("POST", "/api/migration/{u}/backfill"),
    ];

    /// 够用的最小 [`AppState`]：守卫在任何状态被用到之前就把请求拒了；名单内的请求在
    /// 非 K8s 构建里停在"需要 Kubernetes 运行时"。
    ///
    /// 不用 `test_support::Fixture`——它会绑定一个固定端口，全量并行时别的用例正占着
    /// 这个端口，本用例的子进程里 `AddrInUse` 直接崩。
    async fn bare_state() -> (tempfile::TempDir, Arc<crate::app_state::AppState>) {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &root.path().join("control.db"),
            )
            .await
            .unwrap(),
        );
        let runtime: Arc<dyn container_runtime_api::ContainerRuntime> =
            Arc::new(ControlledRuntime {
                builder: Mutex::new(None),
                lookups: Mutex::new(vec![]),
                writes: AtomicUsize::new(0),
            });
        let activity = Arc::new(app_manager::AppActivityRegistry::new(
            std::time::Duration::from_secs(300),
        ));
        let app_service = Arc::new(
            app_manager::service::AppService::new(
                app_manager::config::AppManagerConfig {
                    access_mode: app_manager::config::AppAccessMode::Docker,
                    ..Default::default()
                },
                runtime.clone(),
                activity.clone(),
                None,
                store.clone(),
            )
            .await
            .unwrap(),
        );
        let projects = Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
            crate::storage::ProjectAdapter::new("guard-test".into(), "cluster.local".into()),
        )));
        let (pod_created_tx, _) = tokio::sync::broadcast::channel(8);
        let state = Arc::new(crate::app_state::AppState {
            userapp_store: store.clone(),
            userapp_store_control: store,
            userapp_op_flight: Arc::new(Default::default()),
            userapp_recovery_handle: Arc::new(Mutex::new(None)),
            config: crate::config::AppConfig::default(),
            projects,
            pingora_service: None,
            userapp_error_page: None,
            grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
            session_stream_registry: Arc::new(crate::grpc::SessionStreamRegistry::new()),
            api_key_config: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::config::ApiKeyAuthConfig::default(),
            )),
            pod_creating: Arc::new(dashmap::DashMap::new()),
            pod_created_tx: Arc::new(pod_created_tx),
            container_prefix_rcoder: "guard-test-web".into(),
            container_prefix_computer: "guard-test-computer".into(),
            runtime,
            agent_download_manager: Arc::new(
                agent_provisioning::AgentDownloadManager::new(root.path()).unwrap(),
            ),
            app_service,
            activity,
            cluster_domain: "cluster.local".into(),
        });
        (root, state)
    }

    /// 对五个端点各发一次请求，返回状态码（按 `ENDPOINTS` 的顺序）。
    /// 对每个端点各发一次请求（请求体按端点由 `body` 给），返回状态码（按 `ENDPOINTS` 的顺序）。
    async fn statuses(
        router: &axum::Router,
        user: &str,
        body: fn(&str) -> &'static str,
    ) -> Vec<StatusCode> {
        let mut statuses = Vec::with_capacity(ENDPOINTS.len());
        for (method, path) in ENDPOINTS {
            let uri = path.replace("{u}", user);
            let request = Request::builder()
                .method(*method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body(path)))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            statuses.push(response.status());
            drop(to_bytes(response.into_body(), 64 * 1024).await.unwrap());
        }
        statuses
    }

    /// 各端点各自的合法请求体：migration-state 要 `{state}`；backfill 不认这个字段（未知字段
    /// 是 400）, 它的合法请求体是 `{}`（缺省范围）；其余端点不读请求体。
    fn valid_body(path: &str) -> &'static str {
        if path.contains("migration-state") {
            r#"{"state":"migrated"}"#
        } else {
            "{}"
        }
    }

    /// 对所有端点都不合法的请求体：migration-state 缺 `state`，backfill 的 `paths` 不是数组。
    fn bad_body() -> &'static str {
        r#"{"paths":".local"}"#
    }

    /// 每个场景对应的白名单 env 值。
    fn whitelist_for(case: &str) -> &'static str {
        match case {
            "list" => "member-1,member-2",
            "empty" => "",
            "star" => "*",
            other => panic!("unknown guard case {other}"),
        }
    }

    #[tokio::test]
    async fn whitelist_gates_every_endpoint() {
        let Ok(case) = std::env::var(CHILD_CASE) else {
            for case in ["list", "empty", "star"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST_NAME, "--nocapture"])
                    .env(CHILD_CASE, case)
                    .env("RCODER_MIGRATION_TEST_USERS", whitelist_for(case))
                    .env("CONTAINER_RUNTIME", "kubernetes")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "guard case '{case}' failed:\n{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        assert_eq!(
            std::env::var("RCODER_MIGRATION_TEST_USERS").unwrap(),
            whitelist_for(&case),
            "子进程必须带着本场景的白名单启动（名单在首次调用时读取）"
        );

        let (_root, state) = bare_state().await;
        let router = routes().with_state(state);
        let forbidden = StatusCode::FORBIDDEN;
        let bad_request = StatusCode::BAD_REQUEST;

        let outsider = statuses(&router, "outsider", valid_body).await;
        let shaped = statuses(&router, "ABC123", valid_body).await;
        let member = statuses(&router, "member-1", valid_body).await;
        // 请求体不合法也不能让名单外的调用方绕过 403（Json 提取器曾先于守卫运行）
        let outsider_bad_body = statuses(&router, "outsider", |_| bad_body()).await;
        let member_bad_body = statuses(&router, "member-1", |_| bad_body()).await;
        // backfill 带正式的范围与模式：过了准入和请求体校验, 进到编排（非 K8s 构建停在 501）
        let scoped = statuses(
            &router,
            "member-1",
            |_| r#"{"paths":["*"],"only":["node_modules/**"]}"#,
        )
        .await;
        for (index, (method, path)) in ENDPOINTS.iter().enumerate() {
            let (outsider, shaped, member) = (outsider[index], shaped[index], member[index]);
            eprintln!(
                "{case} {method} {path}: outsider={outsider} ABC123={shaped} member={member}"
            );
            match case.as_str() {
                "list" => {
                    assert_eq!(outsider, forbidden, "名单外: {method} {path}");
                    assert_eq!(
                        outsider_bad_body[index], forbidden,
                        "名单外 + 不合法请求体: 仍然 403, {method} {path}"
                    );
                    // 名单内的判定不依赖请求体；migration-state 过了准入之后才解析它
                    assert_ne!(
                        member_bad_body[index], forbidden,
                        "名单内 + 不合法请求体不该被白名单拒: {method} {path}"
                    );
                    if member != StatusCode::NOT_IMPLEMENTED
                        && (path.contains("migration-state") || path.contains("backfill"))
                    {
                        assert_eq!(
                            member_bad_body[index], bad_request,
                            "过了准入之后不合法请求体是 400: {method} {path}"
                        );
                    }
                    if path.contains("backfill") {
                        assert_ne!(
                            scoped[index], forbidden,
                            "带了范围与模式的正式请求体不该被准入拒绝: {method} {path} → {}",
                            scoped[index]
                        );
                        assert!(
                            scoped[index] != bad_request,
                            "正式请求体不该被请求体校验拒绝: {method} {path} → {}",
                            scoped[index]
                        );
                    }
                    assert_eq!(shaped, bad_request, "id 拼不出合法 Job 名: {method} {path}");
                    assert!(
                        member != forbidden && member != bad_request,
                        "名单内不该被准入拒绝: {method} {path} → {member}"
                    );
                }
                "empty" => assert_eq!(member, forbidden, "名单未设 = 全拒: {method} {path}"),
                "star" => assert_ne!(outsider, forbidden, "批量窗口 `*`: {method} {path}"),
                _ => {}
            }
        }
    }
}
