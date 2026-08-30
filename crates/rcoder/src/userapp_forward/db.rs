//! `POST /api/v1/userapp/db/{dev|prod}/align-credentials`：PG 凭据对齐。
//!
//! 统一前缀 `/api/v1/userapp/db/*`（路径段区分环境，可滤镜、可扩展）：
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
use shared_types::UserappStage;

use crate::router::AppState;
use crate::userapp_builder::{dev_file_server_addr, ensure_userapp_builder_probed};
use crate::{AppError, HttpResult};

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
            .post(format!("{}/api/v1/userapp/execute-command", self.addr))
            .timeout(std::time::Duration::from_secs(30))
            .json(&json!({"app_id": self.app_id, "user_id": self.user_id, "command": command}))
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
        // 响应 {success, stdout, stderr, exit_code}（userapp 域 snake wire；
        // 外层恒 success=true，结果由 exit_code 表达）
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("decode execute-command response: {e}"))?;
        Ok(shared_types::CommandOutcome {
            exit_code: body["exit_code"].as_i64().unwrap_or(-1),
            stdout: body["stdout"].as_str().unwrap_or_default().to_string(),
            stderr: body["stderr"].as_str().unwrap_or_default().to_string(),
        })
    }
}

/// `POST /api/v1/userapp/db/{app_stage}/align-credentials`
#[utoipa::path(
    post,
    path = "/api/v1/userapp/db/{app_stage}/align-credentials",
    request_body = shared_types::AlignCredentialsRequest,
    params(
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）内的 PG；`prod`=运行容器（UserApp）内的 PG")
    ),
    responses(
        (status = 200, description = "对齐完成（aligned=true；reset_performed 表示是否执行了重置）", body = HttpResult<shared_types::AlignCredentialsOutcome>),
        (status = 400, description = "参数校验失败（app_stage/username/password）", body = HttpResult<String>),
        (status = 404, description = "prod 环境 app 不存在或未运行", body = HttpResult<String>),
        (status = 500, description = "开发容器不可达（ERR_CONTAINER_ERROR 映射 500，非 502）", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 数据库",
    operation_id = "align_userapp_db_credentials",
    summary = "PG 凭据对齐",
    description = r#"
校验目标容器内 PG 的账号密码与传入值是否一致（TCP scram 探测），不一致则用
本地 trust 认证改密对齐并复验——**部署链 pg 凭据自动对齐的独立入口**。

- 定位：body `app_id` + path `{app_stage}`（dev=开发容器 / prod=运行容器）；
- `username` 指定目标账号（缺省 superuser）；`password` 为期望值；
- dev 环境：容器不存在时幂等 ensure 开发容器（builder）；
- prod 环境：app 不存在 → 404；stopped 自动唤醒并等待 PG 就绪
  （唤醒窗口约 60s，超时报 InvalidState 可重试）；
- 幂等：密码已一致时零改动作直接成功。

**密码不落日志**；结果 message 区分"已一致/已重置"。
"#,
)]
pub(crate) async fn align_credentials(
    State(state): State<Arc<AppState>>,
    Path(app_stage): Path<String>,
    Json(body): Json<shared_types::AlignCredentialsRequest>,
) -> Result<HttpResult<shared_types::AlignCredentialsOutcome>, AppError> {
    let app_stage = UserappStage::parse(&app_stage)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_app_stage_error(&app_stage)))?;
    shared_types::validate_identifier(&body.app_id, "app_id")
        .map_err(|e| AppError::bad_request(&e))?;
    shared_types::validate_identifier(&body.user_id, "user_id")
        .map_err(|e| AppError::bad_request(&e))?;

    let outcome = match app_stage {
        UserappStage::Dev => {
            // 开发容器：ensure（幂等 + 探活自愈，stopped/exited builder 自动重建；
            // owner 显式档=body user_id）+ ensure-workspace（execute-command 的 cwd 前置）
            let (info, _recreated) =
                ensure_userapp_builder_probed(&state, &body.app_id, Some(&body.user_id))
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
            super::ensure_workspace_via_dev(&addr, &body.app_id, &body.user_id)
                .await
                .map_err(|e| {
                    AppError::with_message(shared_types::error_codes::ERR_CONTAINER_ERROR, e)
                })?;
            let runner = DevHttpRunner {
                addr: &addr,
                app_id: &body.app_id,
                user_id: &body.user_id,
            };
            shared_types::align_pg_credentials(&runner, &body.username, &body.password)
                .await
                .map_err(|e| AppError::with_message(align_error_code(&e), e.to_string()))?
        }
        UserappStage::Prod => state
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
        "[USERAPP_DB_ALIGN] aligned: app_stage={}, app_id={}, username={}, reset_performed={}",
        app_stage.as_str(),
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

/// 解析 exec 目标并做存在性/就绪校验（"有请求即唤醒"平台语义）：
/// - dev：`ensure_userapp_builder_probed`（幂等 + 探活自愈——注册缓存指向
///   stopped/exited 的 Docker builder 时自动重建；pod ensure dev 同款）
/// - prod：`get_app` 前置（防 ensure_running 对不存在 app 的 AlreadyRunning
///   幻报）→ `activity.ensure_running` 自动唤醒（single-flight scale-up，
///   hold-and-wait ≤ wake_timeout 默认 60s；与文件透传/pod ensure prod 同款）
async fn resolve_exec_target(
    state: &AppState,
    app_stage: UserappStage,
    app_id: &str,
    user_id: &str,
) -> Result<String, AppError> {
    match app_stage {
        UserappStage::Dev => {
            let (info, _recreated) = ensure_userapp_builder_probed(state, app_id, Some(user_id))
                .await
                .map_err(|e| {
                    tracing::error!(
                        "[USERAPP_DB_ADMIN] ensure dev container failed: app_stage=dev, app_id={app_id}: {e:#}"
                    );
                    AppError::with_message(
                        shared_types::error_codes::ERR_CONTAINER_ERROR,
                        format!("ensure dev container failed: {e:#}"),
                    )
                })?;
            // builder 内 PG 可能刚 initdb（新容器/重建后），等就绪再执行改密命令
            let runner = ExecRunner {
                runtime: &state.runtime().clone(),
                target: &info.container_name,
            };
            let wait = runner
                .run(&shared_types::pg_utils::pg_wait_ready_cmd(60))
                .await
                .map_err(|e| {
                    tracing::error!(
                        "[USERAPP_DB_ADMIN] wait dev PG ready failed: app_id={app_id}: {e}"
                    );
                    AppError::with_message(
                        shared_types::error_codes::ERR_CONTAINER_ERROR,
                        format!("wait dev PG ready failed: {e}"),
                    )
                })?;
            if wait.exit_code != 0 {
                return Err(AppError::with_message(
                    shared_types::error_codes::ERR_CONTAINER_ERROR,
                    "dev builder postgres not ready after ensure",
                ));
            }
            Ok(info.container_name)
        }
        UserappStage::Prod => {
            if let Err(e) = state.app_service.get_app(app_id).await {
                tracing::error!("[USERAPP_DB_ADMIN] prod app not found: app_id={app_id}: {e:#}");
                // 与 align prod 侧同码（ERR_APP_NOT_FOUND）——同一"应用不存在"语义
                // 双码（ERR_NOT_FOUND）曾是历史不一致，未上线期统一
                return Err(AppError::with_message(
                    shared_types::error_codes::ERR_APP_NOT_FOUND,
                    format!("userapp prod app not found: {e:#}"),
                ));
            }
            use shared_types::AppWakeControl;
            match state.activity.ensure_running(app_id).await {
                shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
                shared_types::WakeOutcome::Timeout | shared_types::WakeOutcome::Failed(_) => {
                    tracing::error!("[USERAPP_DB_ADMIN] prod app wake failed: app_id={app_id}");
                    return Err(AppError::with_message(
                        shared_types::error_codes::ERR_CONTAINER_ERROR,
                        "userapp prod app wake failed or timeout (still starting), retry later",
                    ));
                }
            }
            // 唤醒后容器内 PG 启动窗口：等就绪再交还 exec 目标
            let runner = ExecRunner {
                runtime: &state.runtime().clone(),
                target: app_id,
            };
            let wait = runner
                .run(&shared_types::pg_utils::pg_wait_ready_cmd(60))
                .await
                .map_err(|e| {
                    tracing::error!(
                        "[USERAPP_DB_ADMIN] wait prod PG ready failed: app_id={app_id}: {e}"
                    );
                    AppError::with_message(
                        shared_types::error_codes::ERR_CONTAINER_ERROR,
                        format!("wait prod PG ready failed: {e}"),
                    )
                })?;
            if wait.exit_code != 0 {
                return Err(AppError::with_message(
                    shared_types::error_codes::ERR_CONTAINER_ERROR,
                    "userapp prod postgres not ready after wake",
                ));
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

/// `POST /api/v1/userapp/db/{app_stage}/reset-password`
#[utoipa::path(
    post,
    path = "/api/v1/userapp/db/{app_stage}/reset-password",
    request_body = shared_types::UserappDbResetPasswordRequest,
    params(
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）内的 PG；`prod`=运行容器（UserApp）内的 PG")
    ),
    responses(
        (status = 200, description = "密码已设置（message 区分\"账号已创建并设置密码\"/\"密码已重置\"）", body = HttpResult<String>),
        (status = 400, description = "参数校验失败（app_stage/app_id/new_password/username 非法）", body = HttpResult<String>),
        (status = 404, description = "prod 环境 app 不存在", body = HttpResult<String>),
        (status = 500, description = "容器侧执行失败（PG 未就绪/SQL 失败）", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 数据库",
    operation_id = "userapp_db_reset_password",
    summary = "重置/创建 PG 账号密码",
    description = r#"
设置目标容器内 PG 的账号密码，两种语义：

- **不带 username**：重置 superuser（SQL CURRENT_USER 语义，绕过"需要当前密码"
  死锁——用户忘记 pgweb 密码时的正解）；
- **带 username**：账号 upsert——角色存在则 ALTER USER 改密，不存在则 CREATE ROLE
  建号后再设密。

两者均 best-effort 同步 dbx 预置连接（重写 connections.json 并 restart dbx；
指定业务账号且非 local-pg 在用账号时自动跳过），同步失败不阻断响应（密码已生效）。
prod 环境目标容器 stopped 会自动唤醒并等待 PG 就绪。

**密码只出现在 exec 命令内，日志零落盘**（仅记 app_id/app_stage/username/结果）。
"#,
)]
pub(crate) async fn reset_password(
    State(state): State<Arc<AppState>>,
    Path(app_stage): Path<String>,
    Json(body): Json<shared_types::UserappDbResetPasswordRequest>,
) -> Result<HttpResult<String>, AppError> {
    let app_stage = UserappStage::parse(&app_stage)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_app_stage_error(&app_stage)))?;
    shared_types::validate_identifier(&body.app_id, "app_id")
        .map_err(|e| AppError::bad_request(&e))?;
    shared_types::validate_identifier(&body.user_id, "user_id")
        .map_err(|e| AppError::bad_request(&e))?;
    if body.new_password.is_empty() {
        return Err(AppError::bad_request("new_password must not be empty"));
    }

    let target = resolve_exec_target(&state, app_stage, &body.app_id, &body.user_id).await?;
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
            "密码已重置".to_string()
        }
        Some(username) => {
            let outcome = shared_types::upsert_pg_user(&runner, username, &body.new_password)
                .await
                .map_err(|e| AppError::with_message(db_admin_error_code(&e), e.to_string()))?;
            match outcome {
                shared_types::DbUserUpsertOutcome::Created => "账号已创建并设置密码".to_string(),
                shared_types::DbUserUpsertOutcome::Reset => "密码已重置".to_string(),
            }
        }
    };
    // 密码不落日志（只记 app_id/app_stage/username/结果）
    info!(
        "[USERAPP_DB_ADMIN] password set: app_stage={}, app_id={}, username={}, result={}",
        app_stage.as_str(),
        body.app_id,
        body.username.as_deref().unwrap_or("<superuser>"),
        message
    );
    Ok(HttpResult::success(message))
}

/// `POST /api/v1/userapp/db/{app_stage}/create-database`
#[utoipa::path(
    post,
    path = "/api/v1/userapp/db/{app_stage}/create-database",
    request_body = shared_types::UserappDbCreateDatabaseRequest,
    params(
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）内的 PG；`prod`=运行容器（UserApp）内的 PG")
    ),
    responses(
        (status = 200, description = "数据库已创建", body = HttpResult<String>),
        (status = 400, description = "参数校验失败（app_stage/app_id/database/owner 非标识符）", body = HttpResult<String>),
        (status = 404, description = "prod 环境 app 不存在", body = HttpResult<String>),
        (status = 409, description = "数据库已存在（含并发创建竞态复检）", body = HttpResult<String>),
        (status = 500, description = "容器侧执行失败（PG 未就绪/SQL 失败）", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 数据库",
    operation_id = "userapp_db_create_database",
    summary = "新建 PG 数据库",
    description = r#"
在目标容器的 PG 里建库（API 化建库，Java/CI 自动化场景免手工 psql）：

- 先查 `pg_database` 再 CREATE（check-then-act；409 已存在含并发竞态复检，
  不靠 stderr 文本判定）；
- `owner` 可选：库属主账号（须已存在）；缺省 = 执行者 superuser；
- 标识符白名单校验 `[A-Za-z0-9_]`（app_id/database/owner 全过，防注入）；
- prod 环境 stopped 自动唤醒并等待 PG 就绪。

普通数据操作建议走 pgweb / 业务迁移脚本，本接口面向"建库"这一步编排。
"#,
)]
pub(crate) async fn create_database(
    State(state): State<Arc<AppState>>,
    Path(app_stage): Path<String>,
    Json(body): Json<shared_types::UserappDbCreateDatabaseRequest>,
) -> Result<HttpResult<String>, AppError> {
    let app_stage = UserappStage::parse(&app_stage)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_app_stage_error(&app_stage)))?;
    shared_types::validate_identifier(&body.app_id, "app_id")
        .map_err(|e| AppError::bad_request(&e))?;
    shared_types::validate_identifier(&body.user_id, "user_id")
        .map_err(|e| AppError::bad_request(&e))?;

    let target = resolve_exec_target(&state, app_stage, &body.app_id, &body.user_id).await?;
    let runtime = state.runtime().clone();
    let runner = ExecRunner {
        runtime: &runtime,
        target: &target,
    };
    shared_types::create_pg_database(&runner, &body.database, body.owner.as_deref())
        .await
        .map_err(|e| AppError::with_message(db_admin_error_code(&e), e.to_string()))?;
    info!(
        "[USERAPP_DB_ADMIN] database created: app_stage={}, app_id={}, database={}, owner={:?}",
        app_stage.as_str(),
        body.app_id,
        body.database,
        body.owner
    );
    Ok(HttpResult::success("数据库已创建".to_string()))
}
