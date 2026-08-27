//! `POST /api/userapp/db/{dev|prod}/align-credentials`：PG 凭据对齐。
//!
//! 统一前缀 `/api/userapp/db/*`（路径段区分环境，可滤镜、可扩展）：
//! - `dev` → 该 app 的 UserAppBuilder 开发容器（经容器内 file-server
//!   `execute-command` HTTP 通道执行 psql）
//! - `prod` → UserApp 运行容器（app_manager runtime exec 通道）
//!
//! 流程单头 [`shared_types::align_pg_credentials`]（验证 scram → 角色存在 →
//! trust 重置 → 复验）；密码不落日志。

use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, State};
use serde_json::json;
use tracing::info;

// ExecRunner 方法语法调用所需（trait 本体经 shared_types 全路径引用）
use shared_types::PgCommandRunner as _;

use crate::router::AppState;
use crate::userapp_builder::{dev_file_server_addr, ensure_userapp_builder};
use crate::{AppError, HttpResult};

/// 目标环境路径段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbEnv {
    Dev,
    Prod,
}

impl DbEnv {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "dev" => Some(Self::Dev),
            "prod" => Some(Self::Prod),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Prod => "prod",
        }
    }
}

/// 开发容器执行通道：容器内 file-server `execute-command`（cwd=workspace 须已存在，
/// 故本 handler 前置幂等 ensure-workspace）。
struct DevHttpRunner<'a> {
    addr: &'a str,
    app_id: &'a str,
    user_id: &'a str,
}

#[async_trait]
impl shared_types::PgCommandRunner for DevHttpRunner<'_> {
    async fn run(&self, command: &str) -> Result<shared_types::CommandOutcome, String> {
        // 30s 客户端超时: 容器内 execute-command 的服务端超时默认 1800s(为长构建
        // 设计), psql 秒级命令若 PG hang 会拖死对齐接口——传输层兜底
        let resp = crate::http_client::shared_client()
            .post(format!("{}/api/userapp/execute-command", self.addr))
            .timeout(std::time::Duration::from_secs(30))
            .json(&json!({"appId": self.app_id, "userId": self.user_id, "command": command}))
            .send()
            .await
            .map_err(|e| format!("dev container execute-command failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "dev container execute-command returned {status}: {text}"
            ));
        }
        // 响应 {success, stdout, stderr, exitCode}（TS 外层恒 success=true，结果由 exitCode 表达）
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("decode execute-command response: {e}"))?;
        Ok(shared_types::CommandOutcome {
            exit_code: body["exitCode"].as_i64().unwrap_or(-1),
            stdout: body["stdout"].as_str().unwrap_or_default().to_string(),
            stderr: body["stderr"].as_str().unwrap_or_default().to_string(),
        })
    }
}

/// `POST /api/userapp/db/{env}/align-credentials`
#[utoipa::path(
    post,
    path = "/api/userapp/db/{env}/align-credentials",
    request_body = shared_types::AlignCredentialsRequest,
    params(
        ("env" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）内的 PG；`prod`=运行容器（UserApp）内的 PG")
    ),
    responses(
        (status = 200, description = "对齐完成（aligned=true；reset_performed 表示是否执行了重置）", body = HttpResult<shared_types::AlignCredentialsOutcome>),
        (status = 400, description = "参数校验失败（env/username/password）", body = HttpResult<String>),
        (status = 404, description = "prod 环境 app 不存在或未运行", body = HttpResult<String>),
        (status = 502, description = "开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp · 数据库",
    operation_id = "align_userapp_db_credentials",
    summary = "PG 凭据对齐（检查 dev/prod 容器内 PG 密码，不一致则重置）"
)]
pub(crate) async fn align_credentials(
    State(state): State<Arc<AppState>>,
    Path(env): Path<String>,
    Json(body): Json<shared_types::AlignCredentialsRequest>,
) -> Result<HttpResult<shared_types::AlignCredentialsOutcome>, AppError> {
    let env = DbEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request("path segment `env` must be `dev` or `prod`"))?;
    shared_types::validate_identifier(&body.app_id, "app_id")
        .map_err(|e| AppError::bad_request(&e))?;

    let outcome = match env {
        DbEnv::Dev => {
            // 开发容器：ensure（幂等）+ ensure-workspace（execute-command 的 cwd 前置）
            let info = ensure_userapp_builder(&state, &body.app_id)
                .await
                .map_err(|e| {
                    tracing::error!(
                        "[USERAPP_DB_ALIGN] ensure dev container failed: app_id={}: {e:#}",
                        body.app_id
                    );
                    AppError::with_message(
                        shared_types::error_codes::ERR_CONTAINER_ERROR,
                        format!("ensure dev container failed: {e:#}"),
                    )
                })?;
            let addr = dev_file_server_addr(&state, &info);
            // 审计 user_id：metadata owner（create-workspace/publish 落库的事实源）
            // 优先，缺省 app_id（不参与定位）。
            let user_id = state
                .app_service
                .get_app_owner(&body.app_id)
                .await
                .unwrap_or_else(|| body.app_id.clone());
            super::ensure_workspace_via_dev(&addr, &body.app_id, &user_id)
                .await
                .map_err(|e| {
                    AppError::with_message(shared_types::error_codes::ERR_CONTAINER_ERROR, e)
                })?;
            let runner = DevHttpRunner {
                addr: &addr,
                app_id: &body.app_id,
                user_id: &user_id,
            };
            shared_types::align_pg_credentials(&runner, &body.username, &body.password)
                .await
                .map_err(|e| AppError::with_message(align_error_code(&e), e.to_string()))?
        }
        DbEnv::Prod => state
            .app_service
            .align_db_credentials(&body.app_id, body.clone())
            .await
            .map_err(|e| {
                // 与 dev 分支同一错误码语义：Validation（输入问题）→ ERR_VALIDATION；
                // 其余（容器侧执行失败）→ ERR_CONTAINER_ERROR
                let code = match &e {
                    app_manager::AppOperationError::Validation(_) => {
                        shared_types::error_codes::ERR_VALIDATION
                    }
                    _ => shared_types::error_codes::ERR_CONTAINER_ERROR,
                };
                AppError::with_message(code, format!("prod align failed: {e}"))
            })?,
    };

    info!(
        "[USERAPP_DB_ALIGN] aligned: env={}, app_id={}, username={}, reset_performed={}",
        env.as_str(),
        body.app_id,
        body.username,
        outcome.reset_performed
    );
    Ok(HttpResult::success(outcome))
}

/// 对齐流程错误的错误码映射（类型化 variant 匹配）：
/// [`shared_types::AlignError::InvalidInput`] / [`RoleMissing`] 为调用方输入问题
/// （400 语义）；[`Command`] 为容器侧执行失败（ERR_CONTAINER_ERROR）。
fn align_error_code(err: &shared_types::AlignError) -> &'static str {
    match err {
        shared_types::AlignError::InvalidInput(_) | shared_types::AlignError::RoleMissing(_) => {
            shared_types::error_codes::ERR_VALIDATION
        }
        shared_types::AlignError::Command { .. } => shared_types::error_codes::ERR_CONTAINER_ERROR,
    }
}

// ── 账号/库管理（reset-password / create-database；与 align 同域扩展） ──────────

/// rcoder 侧 PG 命令执行通道：`ContainerRuntime::exec`（容器内 `sh -c`）。
/// dev 目标 = UserAppBuilder 容器名；prod 目标 = app_id（pod 解析在 runtime
/// 内部，与 app_manager 的 RuntimeExecRunner 同款）。
struct ExecRunner<'a> {
    runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
    target: &'a str,
}

#[async_trait]
impl shared_types::PgCommandRunner for ExecRunner<'_> {
    async fn run(&self, command: &str) -> Result<shared_types::CommandOutcome, String> {
        let r = self
            .runtime
            .exec(
                self.target,
                vec!["sh".to_string(), "-c".to_string(), command.to_string()],
            )
            .await
            .map_err(|e| format!("exec failed: {e}"))?;
        Ok(shared_types::CommandOutcome {
            exit_code: r.exit_code,
            stdout: r.stdout,
            stderr: r.stderr,
        })
    }
}

/// 解析 exec 目标并做存在性/就绪校验：
/// - dev：`ensure_userapp_builder`（幂等，与 align 同款——容器不在则创建）
/// - prod：`get_app` 存在性校验（**不自动唤醒**——stopped 的 app 由 exec 失败
///   显式报错，Java 需先 ensure；与 pod ensure prod 的 get_app 前置同款语义）
async fn resolve_exec_target(
    state: &AppState,
    env: DbEnv,
    app_id: &str,
) -> Result<String, AppError> {
    match env {
        DbEnv::Dev => {
            let info = ensure_userapp_builder(state, app_id)
                .await
                .map_err(|e| {
                    tracing::error!(
                        "[USERAPP_DB_ADMIN] ensure dev container failed: env=dev, app_id={app_id}: {e:#}"
                    );
                    AppError::with_message(
                        shared_types::error_codes::ERR_CONTAINER_ERROR,
                        format!("ensure dev container failed: {e:#}"),
                    )
                })?;
            Ok(info.container_name)
        }
        DbEnv::Prod => {
            if let Err(e) = state.app_service.get_app(app_id).await {
                tracing::error!("[USERAPP_DB_ADMIN] prod app not found: app_id={app_id}: {e:#}");
                return Err(AppError::not_found(&format!(
                    "userapp prod app not found: {e:#}"
                )));
            }
            Ok(app_id.to_string())
        }
    }
}

/// 账号/库管理流程错误的错误码映射（与 `align_error_code` 同构）。
fn db_admin_error_code(err: &shared_types::DbAdminError) -> &'static str {
    use shared_types::DbAdminError as E;
    match err {
        E::InvalidInput(_) => shared_types::error_codes::ERR_VALIDATION,
        E::AlreadyExists(_) => shared_types::error_codes::ERR_CONFLICT,
        E::Command { .. } => shared_types::error_codes::ERR_CONTAINER_ERROR,
    }
}

/// `POST /api/userapp/db/{env}/reset-password`
#[utoipa::path(
    post,
    path = "/api/userapp/db/{env}/reset-password",
    request_body = shared_types::UserappDbResetPasswordRequest,
    params(
        ("env" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）内的 PG；`prod`=运行容器（UserApp）内的 PG")
    ),
    responses(
        (status = 200, description = "密码已设置（message 区分\"账号已创建并设置密码\"/\"密码已重置\"）", body = HttpResult<String>),
        (status = 400, description = "参数校验失败（env/app_id/new_password/username 非法）", body = HttpResult<String>),
        (status = 404, description = "prod 环境 app 不存在", body = HttpResult<String>),
        (status = 500, description = "容器侧执行失败（PG 未就绪/SQL 失败）", body = HttpResult<String>)
    ),
    tag = "UserApp · 数据库",
    operation_id = "userapp_db_reset_password",
    summary = "重置/创建 userApp 容器内 PG 账号密码（dev/prod）"
)]
pub(crate) async fn reset_password(
    State(state): State<Arc<AppState>>,
    Path(env): Path<String>,
    Json(body): Json<shared_types::UserappDbResetPasswordRequest>,
) -> Result<HttpResult<String>, AppError> {
    let env = DbEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request("path segment `env` must be `dev` or `prod`"))?;
    shared_types::validate_identifier(&body.app_id, "app_id")
        .map_err(|e| AppError::bad_request(&e))?;
    if body.new_password.is_empty() {
        return Err(AppError::bad_request("new_password must not be empty"));
    }

    let target = resolve_exec_target(&state, env, &body.app_id).await?;
    let runtime = state.runtime().clone();
    let runner = ExecRunner {
        runtime: &runtime,
        target: &target,
    };

    // username 缺省 → 重置 superuser（CURRENT_USER 语义，与 computer 版/app_manager
    // 版同源）；指定 → 账号 upsert（存在 ALTER / 不存在 CREATE ROLE 建号）
    let message = match body.username.as_deref() {
        None => {
            let cmd =
                shared_types::pg_utils::pg_alter_current_user_password_cmd(&body.new_password);
            let r = runner.run(&cmd).await.map_err(|e| {
                AppError::with_message(shared_types::error_codes::ERR_CONTAINER_ERROR, e)
            })?;
            if r.exit_code != 0 {
                return Err(AppError::with_message(
                    shared_types::error_codes::ERR_CONTAINER_ERROR,
                    format!(
                        "reset superuser password failed: exit {} {}",
                        r.exit_code,
                        r.stderr.trim()
                    ),
                ));
            }
            // dbx 预置连接同步（best-effort）：重置目标即 local-pg 在用账号，
            // 恒同步；失败仅 warn——密码已生效，不阻断响应。
            if let Err(e) =
                shared_types::sync_dbx_after_password_change(&runner, None, &body.new_password)
                    .await
            {
                tracing::warn!(
                    "[USERAPP_DB_ADMIN] dbx connection sync failed (password already applied): env={}, app_id={}: {e}",
                    env.as_str(),
                    body.app_id
                );
            }
            "密码已重置".to_string()
        }
        Some(username) => {
            let outcome = shared_types::upsert_pg_user(&runner, username, &body.new_password)
                .await
                .map_err(|e| AppError::with_message(db_admin_error_code(&e), e.to_string()))?;
            // dbx 预置连接同步（best-effort，条件内建）：仅当指定账号 ==
            // $POSTGRES_USER（local-pg 在用账号）时才动 local-pg；重置业务账号跳过。
            if let Err(e) = shared_types::sync_dbx_after_password_change(
                &runner,
                Some(username),
                &body.new_password,
            )
            .await
            {
                tracing::warn!(
                    "[USERAPP_DB_ADMIN] dbx connection sync failed (password already applied): env={}, app_id={}, username={username}: {e}",
                    env.as_str(),
                    body.app_id
                );
            }
            match outcome {
                shared_types::DbUserUpsertOutcome::Created => "账号已创建并设置密码".to_string(),
                shared_types::DbUserUpsertOutcome::Reset => "密码已重置".to_string(),
            }
        }
    };
    // 密码不落日志（只记 app_id/env/username/结果）
    info!(
        "[USERAPP_DB_ADMIN] password set: env={}, app_id={}, username={}, result={}",
        env.as_str(),
        body.app_id,
        body.username.as_deref().unwrap_or("<superuser>"),
        message
    );
    Ok(HttpResult::success(message))
}

/// `POST /api/userapp/db/{env}/create-database`
#[utoipa::path(
    post,
    path = "/api/userapp/db/{env}/create-database",
    request_body = shared_types::UserappDbCreateDatabaseRequest,
    params(
        ("env" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）内的 PG；`prod`=运行容器（UserApp）内的 PG")
    ),
    responses(
        (status = 200, description = "数据库已创建", body = HttpResult<String>),
        (status = 400, description = "参数校验失败（env/app_id/database/owner 非标识符）", body = HttpResult<String>),
        (status = 404, description = "prod 环境 app 不存在", body = HttpResult<String>),
        (status = 409, description = "数据库已存在（含并发创建竞态复检）", body = HttpResult<String>),
        (status = 500, description = "容器侧执行失败（PG 未就绪/SQL 失败）", body = HttpResult<String>)
    ),
    tag = "UserApp · 数据库",
    operation_id = "userapp_db_create_database",
    summary = "在 userApp 容器内 PG 新建数据库（dev/prod）"
)]
pub(crate) async fn create_database(
    State(state): State<Arc<AppState>>,
    Path(env): Path<String>,
    Json(body): Json<shared_types::UserappDbCreateDatabaseRequest>,
) -> Result<HttpResult<String>, AppError> {
    let env = DbEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request("path segment `env` must be `dev` or `prod`"))?;
    shared_types::validate_identifier(&body.app_id, "app_id")
        .map_err(|e| AppError::bad_request(&e))?;

    let target = resolve_exec_target(&state, env, &body.app_id).await?;
    let runtime = state.runtime().clone();
    let runner = ExecRunner {
        runtime: &runtime,
        target: &target,
    };
    shared_types::create_pg_database(&runner, &body.database, body.owner.as_deref())
        .await
        .map_err(|e| AppError::with_message(db_admin_error_code(&e), e.to_string()))?;
    info!(
        "[USERAPP_DB_ADMIN] database created: env={}, app_id={}, database={}, owner={:?}",
        env.as_str(),
        body.app_id,
        body.database,
        body.owner
    );
    Ok(HttpResult::success("数据库已创建".to_string()))
}
