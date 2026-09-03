//! `POST /api/v1/userapp/db/{dev|prod}/reset-password|create-database`：
//! Userapp PG 账号/库管理。
//!
//! 统一前缀 `/api/v1/userapp/db/*`（路径段区分环境，可滤镜、可扩展）：
//! - `dev` → 该 app 的 UserappBuilder 开发容器（exec 直达 builder 容器，
//!   含 PG 就绪等待）
//! - `prod` → Userapp 运行容器（app_manager runtime exec 通道，stopped 自动唤醒）
//!
//! 流程单头 [`shared_types::upsert_pg_user`]/[`create_pg_database`]；密码不落日志。
//! （PG 凭据对齐不在此面——start 部署链内嵌（请求 `pg.username`/`pg.password`
//! → 响应 `pg_aligned`），流程单头 `shared_types::align_pg_credentials`
//! 供 app_manager 函数级消费，独立 HTTP 入口已下线。）

use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, State};
use tracing::info;

// ExecRunner 方法语法调用所需（trait 本体经 shared_types 全路径引用）
use shared_types::PgCommandRunner as _;
use shared_types::UserappStage;

use crate::router::AppState;
use crate::userapp_builder::ensure_userapp_builder_probed;
use crate::{AppError, HttpResult};

/// rcoder 侧 PG 命令执行通道：`ContainerRuntime::exec`（容器内 `sh -c`）。
/// rcoder 侧 PG 命令执行通道（对齐 shared_types::db_align 模块契约注释）：
/// - dev：开发容器内 file-server `execute-command`（HTTP，容器内 `sh -c`
///   同语义）——`ContainerRuntime::exec` 是 **Userapp 运行容器**的 app_id
///   语义（目标拼 `rcoder-app-{id}`），传 builder 完整容器名会被再拼一层
///   前缀致 404，不能用于 dev
/// - prod：`ContainerRuntime::exec`（app_id → Userapp 运行容器，与
///   app_manager 的 RuntimeExecRunner 同款）
enum ExecChannel<'a> {
    DevHttp {
        /// dev 容器 file-server 基址（`dev_file_server_addr` 产出）
        base: String,
        app_id: String,
        user_id: String,
    },
    ProdRuntime {
        runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
        app_id: String,
    },
}

#[async_trait]
impl shared_types::PgCommandRunner for ExecChannel<'_> {
    async fn run(&self, command: &str) -> Result<shared_types::CommandOutcome, String> {
        match self {
            Self::DevHttp {
                base,
                app_id,
                user_id,
            } => {
                let resp = crate::http_client::shared_client()
                    .post(format!("{base}/api/v1/userapp/execute-command"))
                    .json(&serde_json::json!({
                        "app_id": app_id,
                        "user_id": user_id,
                        "command": command,
                    }))
                    .timeout(std::time::Duration::from_secs(90))
                    .send()
                    .await
                    .map_err(|e| format!("exec http failed: {e}"))?;
                let status = resp.status();
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| format!("exec http body: {e}"))?;
                // execute-command 契约：外层恒 success=true（命令结果由
                // exit_code 表示）；非 2xx / success=false 是通道层问题
                if !status.is_success() || body["success"].as_bool() != Some(true) {
                    return Err(format!(
                        "execute-command rejected: HTTP {status}: {}",
                        serde_json::to_string(&body).unwrap_or_else(|_| "<unserializable>".into())
                    ));
                }
                // userapp 域 execute-command 响应键为 snake（exit_code——契约
                // 测试 userapp_dev.rs:357 锁定）；-1 兜底 = 响应缺字段视为执行失败
                Ok(shared_types::CommandOutcome {
                    exit_code: body["exit_code"].as_i64().unwrap_or(-1),
                    stdout: body["stdout"].as_str().unwrap_or_default().to_string(),
                    stderr: body["stderr"].as_str().unwrap_or_default().to_string(),
                })
            }
            Self::ProdRuntime { runtime, app_id } => {
                let r = runtime
                    .exec(
                        app_id,
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
    }
}

/// 解析 exec 目标并做存在性/就绪校验（"有请求即唤醒"平台语义）：
/// - dev：`ensure_userapp_builder_probed`（幂等 + 探活自愈——注册缓存指向
///   stopped/exited 的 Docker builder 时自动重建；pod ensure dev 同款）
/// - prod：`get_app` 前置（防 ensure_running 对不存在 app 的 AlreadyRunning
///   幻报）→ `activity.ensure_running` 自动唤醒（single-flight scale-up，
///   hold-and-wait ≤ wake_timeout 默认 60s；与文件透传/pod ensure prod 同款）
async fn resolve_exec_target<'a>(
    state: &'a AppState,
    app_stage: UserappStage,
    app_id: &str,
    user_id: &str,
) -> Result<ExecChannel<'a>, AppError> {
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
            // dev 通道：dev 容器 file-server execute-command（契约见 ExecChannel）
            let channel = ExecChannel::DevHttp {
                base: crate::userapp_builder::dev_file_server_addr(state, &info),
                app_id: app_id.to_string(),
                user_id: user_id.to_string(),
            };
            // builder 内 PG 可能刚 initdb（新容器/重建后），等就绪再执行改密命令
            let wait = channel
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
            Ok(channel)
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
            // 唤醒后容器内 PG 启动窗口：等就绪再交还 exec 通道
            let channel = ExecChannel::ProdRuntime {
                runtime: state.runtime(),
                app_id: app_id.to_string(),
            };
            let wait = channel
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
            Ok(ExecChannel::ProdRuntime {
                runtime: state.runtime(),
                app_id: app_id.to_string(),
            })
        }
    }
}

/// 账号/库管理流程错误的错误码映射（类型化 variant 匹配，与 db_align 的
/// 对齐错误映射同构）。
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
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）内的 PG；`prod`=运行容器（Userapp）内的 PG")
    ),
    responses(
        (status = 200, description = "密码已设置（message 区分\"账号已创建并设置密码\"/\"密码已重置\"）", body = HttpResult<String>),
        (status = 400, description = "参数校验失败（app_stage/app_id/password/username 非法）", body = HttpResult<String>),
        (status = 404, description = "prod 环境 app 不存在", body = HttpResult<String>),
        (status = 500, description = "容器侧执行失败（PG 未就绪/SQL 失败）", body = HttpResult<String>)
    ),
    tag = "Userapp · 双态 · 数据库",
    operation_id = "userapp_db_reset_password",
    summary = "重置/创建 PG 账号密码",
    description = r#"
设置目标容器内 PG 的账号密码，两种语义：

- **不带 username**：重置 superuser（SQL CURRENT_USER 语义，绕过"需要当前密码"
  死锁——用户忘记数据库密码时的正解）；
- **带 username**：账号 upsert——角色存在则 ALTER USER 改密，不存在则 CREATE ROLE
  建号后再设密。

dbx 预置连接为容器内 local-pg socket 免密（与改密链解耦——改密不影响 dbx 访问）。
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
    if body.password.is_empty() {
        return Err(AppError::bad_request("password must not be empty"));
    }

    let runner = resolve_exec_target(&state, app_stage, &body.app_id, &body.user_id).await?;

    // username 缺省 → 重置 superuser（CURRENT_USER 语义，与 computer 版/app_manager
    // 版同源）；指定 → 账号 upsert（存在 ALTER / 不存在 CREATE ROLE 建号）
    let message = match body.username.as_deref() {
        None => {
            let cmd = shared_types::pg_utils::pg_alter_current_user_password_cmd(&body.password);
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
            let outcome = shared_types::upsert_pg_user(&runner, username, &body.password)
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
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）内的 PG；`prod`=运行容器（Userapp）内的 PG")
    ),
    responses(
        (status = 200, description = "数据库已创建", body = HttpResult<String>),
        (status = 400, description = "参数校验失败（app_stage/app_id/database/owner 非标识符）", body = HttpResult<String>),
        (status = 404, description = "prod 环境 app 不存在", body = HttpResult<String>),
        (status = 409, description = "数据库已存在（含并发创建竞态复检）", body = HttpResult<String>),
        (status = 500, description = "容器侧执行失败（PG 未就绪/SQL 失败）", body = HttpResult<String>)
    ),
    tag = "Userapp · 双态 · 数据库",
    operation_id = "userapp_db_create_database",
    summary = "新建 PG 数据库",
    description = r#"
在目标容器的 PG 里建库（API 化建库，Java/CI 自动化场景免手工 psql）：

- 先查 `pg_database` 再 CREATE（check-then-act；409 已存在含并发竞态复检，
  不靠 stderr 文本判定）；
- `owner` 可选：库属主账号（须已存在）；缺省 = 执行者 superuser；
- 标识符白名单校验 `[A-Za-z0-9_]`（app_id/database/owner 全过，防注入）；
- prod 环境 stopped 自动唤醒并等待 PG 就绪。

普通数据操作建议走 dbx 控制台 / 业务迁移脚本，本接口面向"建库"这一步编排。
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

    let runner = resolve_exec_target(&state, app_stage, &body.app_id, &body.user_id).await?;
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
