//! Computer Agent-runner 容器的 PG 管理接口（重置密码 / 新建库）。
//!
//! 与 app_manager 给 UserApp 提供的 `POST /api/v1/apps/{app_id}/db/*` 同源、同语义，
//! 区别仅在此处针对 computer agent-runner 容器（按 user_id 解析，一用户一容器）。
//! PG 逻辑（psql 命令、SQL 转义、退出码判定）镜像 `app_manager::service::reset_db_password`
//! 与 `create_database`（crates/app_manager/src/service.rs:461-546）。
//!
//! 为什么 rcoder 侧 exec 而非 agent_runner 加接口: rcoder 目前没有把 `/computer/*`
//! HTTP 反代到 agent-runner pod:8086 的通道（每条 `/computer/*` 都是 rcoder 自己的 handler），
//! 而 agent-runner 容器自带本地 PG（POSTGRES_USER 烤进 ENV、psql 在 PATH、initdb
//! --auth-local=trust 本地免密），rcoder 经 `ContainerRuntime::exec` 在容器内跑 psql 即可，
//! agent_runner 二进制零改动。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use shared_types::pg_utils::{pg_escape_literal, pg_quote_ident, validate_pg_identifier};
use shared_types::{AppError, HttpResult, ServiceType};
use tracing::{info, instrument};

use app_manager::models::{CreateDatabaseRequest, ResetDbPasswordRequest};

use crate::router::AppState;

/// 重置 computer agent-runner 容器内 PG 的用户密码。
///
/// 用途: 用户忘记 pgweb 密码时重置 —— psql 走容器内本地 trust 认证免密连上,
/// 直接 ALTER USER 改密, 绕过"需要当前密码"的死锁。
#[utoipa::path(
    post,
    path = "/computer/db/{user_id}/reset-password",
    params(("user_id" = String, Path, description = "用户 ID（computer agent-runner 容器标识，一用户一容器）")),
    request_body = ResetDbPasswordRequest,
    responses(
        (status = 200, description = "密码已重置", body = HttpResult<String>),
        (status = 400, description = "参数错误（new_password 为空）"),
        (status = 404, description = "agent-runner 容器不存在"),
        (status = 500, description = "后端错误（psql 执行失败）")
    ),
    tag = "Computer Agent"
)]
#[instrument(skip(state))]
pub async fn computer_db_reset_password(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    Json(req): Json<ResetDbPasswordRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    if req.new_password.is_empty() {
        return Err(AppError::validation_error("new_password must not be empty"));
    }
    let container_name = resolve_computer_container(&state, &user_id).await?;

    // 镜像 app_manager::service::reset_db_password:
    // 容器内 sh 展开 $POSTGRES_USER(镜像 ENV); psql 本地 trust 免密;
    // safe_pw 经 pg_escape_literal (SQL 标准 ' → '' 转义) 防注入到 SQL 字符串字面量;
    // 密码本身不做标识符白名单校验(允许任意字符含特殊符号)。
    let safe_pw = pg_escape_literal(&req.new_password);
    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            r#"psql -U "$POSTGRES_USER" -d postgres -v ON_ERROR_STOP=1 -c "ALTER USER \"$POSTGRES_USER\" WITH PASSWORD '{safe_pw}'""#,
        ),
    ];
    let r = state
        .runtime()
        .exec(&container_name, cmd)
        .await
        .map_err(|e| {
            AppError::internal_server_error(&format!(
                "[COMPUTER_DB] reset-password exec failed user_id={user_id}: {e}"
            ))
        })?;
    if r.exit_code != 0 {
        return Err(AppError::internal_server_error(&format!(
            "[COMPUTER_DB] reset-password failed user_id={user_id}: exit {} {}",
            r.exit_code,
            r.stderr.trim()
        )));
    }
    info!("[COMPUTER_DB] PG password reset: user_id={}", user_id);
    Ok(Json(HttpResult::success("密码已重置".to_string())))
}

/// 在 computer agent-runner 容器的 PG 里新建数据库。
#[utoipa::path(
    post,
    path = "/computer/db/{user_id}/create-database",
    params(("user_id" = String, Path, description = "用户 ID（computer agent-runner 容器标识）")),
    request_body = CreateDatabaseRequest,
    responses(
        (status = 200, description = "数据库已创建", body = HttpResult<String>),
        (status = 400, description = "参数错误（库名/owner 非法标识符）"),
        (status = 404, description = "agent-runner 容器不存在"),
        (status = 409, description = "数据库已存在"),
        (status = 500, description = "后端错误（psql 执行失败）")
    ),
    tag = "Computer Agent"
)]
#[instrument(skip(state))]
pub async fn computer_db_create_database(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    Json(req): Json<CreateDatabaseRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    validate_pg_identifier(&req.database)
        .map_err(|e| AppError::validation_error(&e))?;
    if let Some(owner) = &req.owner {
        validate_pg_identifier(owner)
            .map_err(|e| AppError::validation_error(&e))?;
    }
    let container_name = resolve_computer_container(&state, &user_id).await?;

    // 先查是否已存在(check-then-act): PG 不支持 CREATE DATABASE IF NOT EXISTS,
    // 也不能在事务/DO 块里跑 CREATE DATABASE。故先 SELECT pg_database 判定存在性,
    // 避免靠 CREATE 失败后的 stderr 文本(随 PG 版本/locale 变, 不稳定)判"已存在"。
    if database_exists(&state, &container_name, &req.database).await? {
        return Err(AppError::conflict(&format!(
            "database {} already exists",
            req.database
        )));
    }

    // CREATE DATABASE :owner 经 pg_quote_ident 转义 (双引号引用,内部 " → "")。
    // req.database / owner 已在入口 validate_pg_identifier 校验,此处为纵深防御。
    let safe_db = pg_quote_ident(&req.database);
    let owner_clause = req
        .owner
        .as_ref()
        .map(|o| format!(" OWNER {}", pg_quote_ident(o)))
        .unwrap_or_default();
    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            r#"psql -U "$POSTGRES_USER" -d postgres -v ON_ERROR_STOP=1 -c 'CREATE DATABASE {safe_db}{owner_clause}'"#,
        ),
    ];
    let ctx = format!("[COMPUTER_DB] create-database failed user_id={user_id}");
    let r = state
        .runtime()
        .exec(&container_name, cmd)
        .await
        .map_err(|e| AppError::internal_server_error(&format!("{ctx}: {e}")))?;
    if r.exit_code != 0 {
        // 罕见竞态(SELECT 时不存在、CREATE 时已被并发创建): 再查一次精确判定, 仍不靠 stderr 文本。
        if database_exists(&state, &container_name, &req.database).await? {
            return Err(AppError::conflict(&format!(
                "database {} already exists",
                req.database
            )));
        }
        return Err(AppError::internal_server_error(&format!(
            "{ctx}: exit {} {}",
            r.exit_code,
            r.stderr.trim()
        )));
    }
    info!(
        "[COMPUTER_DB] database created: {} (user_id={})",
        req.database, user_id
    );
    Ok(Json(HttpResult::success("数据库已创建".to_string())))
}

/// 按 user_id 解析 computer agent-runner 容器名。未找到 → 404。
async fn resolve_computer_container(state: &AppState, user_id: &str) -> Result<String, AppError> {
    if user_id.trim().is_empty() {
        return Err(AppError::validation_error("user_id is required"));
    }
    let info = state
        .runtime()
        .get_container_info_by_identifier(user_id, &ServiceType::ComputerAgentRunner)
        .await
        .map_err(|e| {
            AppError::internal_server_error(&format!(
                "[COMPUTER_DB] resolve container failed user_id={user_id}: {e}"
            ))
        })?
        .ok_or_else(|| {
            AppError::not_found(&format!(
                "agent-runner container not found for user_id={user_id}"
            ))
        })?;
    Ok(info.container_name)
}

/// 查询 PG 里某库是否已存在（容器内 psql `-tAc SELECT pg_database`）。
/// `-tAc` 取无表头纯输出：命中输出 `1`、未命中输出空 → 比 CREATE 失败后解析 stderr 稳定。
/// `db` 在入口处已 `validate_pg_identifier` 白名单校验,此处 pg_escape_literal 作纵深防御。
async fn database_exists(
    state: &AppState,
    container_name: &str,
    db: &str,
) -> Result<bool, AppError> {
    let safe_db = pg_escape_literal(db);
    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            r#"psql -U "$POSTGRES_USER" -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname='{safe_db}'""#,
        ),
    ];
    let r = state
        .runtime()
        .exec(container_name, cmd)
        .await
        .map_err(|e| {
            AppError::internal_server_error(&format!(
                "[COMPUTER_DB] check database exists failed: {e}"
            ))
        })?;
    if r.exit_code != 0 {
        return Err(AppError::internal_server_error(&format!(
            "[COMPUTER_DB] check database exists failed: exit {} {}",
            r.exit_code,
            r.stderr.trim()
        )));
    }
    Ok(r.stdout.trim() == "1")
}
