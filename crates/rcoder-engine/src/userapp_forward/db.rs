//! `POST /api/v1/userapp/db/{dev|prod}/reset-password|create-database`：
//! Userapp PG 账号/库管理。
//!
//! 统一前缀 `/api/v1/userapp/db/*`（路径段区分环境，可滤镜、可扩展）：
//! - `dev` → 该 app 的 UserappBuilder 开发容器（exec 直达 builder 容器，
//!   含 PG 就绪等待）
//! - `prod` → Userapp 运行容器（app_manager runtime exec 通道）；改密不自动唤醒。
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

use crate::app_state::AppState;
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
pub(super) enum ExecChannel<'a> {
    DevHttp {
        /// dev 容器 file-server 基址（`dev_file_server_addr` 产出）
        base: String,
        app_id: String,
        credentials: shared_types::FileServerRequestCredentials,
        command_timeout: std::time::Duration,
    },
    ProdRuntime {
        runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
        app_id: String,
    },
}

#[async_trait]
impl shared_types::PgCommandRunner for ExecChannel<'_> {
    async fn run(
        &self,
        command: &str,
        mode: shared_types::PgCommandMode,
    ) -> Result<shared_types::CommandOutcome, shared_types::PgCommandError> {
        use shared_types::{PgCommandError, PgCommandEvidence};
        match self {
            Self::DevHttp {
                base,
                app_id,
                credentials,
                command_timeout,
            } => {
                let request = crate::http_client::shared_client()
                    .post(format!("{base}/api/v1/userapp/execute-command"))
                    .json(&serde_json::json!({"app_id": app_id, "command": command}))
                    .timeout(*command_timeout);
                let request = credentials.apply(request);
                let resp = request.send().await.map_err(|error| {
                    let code = if error.is_builder() {
                        shared_types::ERR_RUNTIME_CONFIGURATION
                    } else if error.is_timeout() {
                        shared_types::ERR_RUNTIME_TIMEOUT
                    } else {
                        shared_types::ERR_RUNTIME_UNAVAILABLE
                    };
                    if error.is_connect() || error.is_builder() {
                        PgCommandError::new(
                            code,
                            "PostgreSQL command was not dispatched",
                            PgCommandEvidence::NotDispatched,
                        )
                    } else {
                        PgCommandError::transport(mode, code, "PostgreSQL command transport failed")
                    }
                })?;
                let status = resp.status();
                let body: serde_json::Value = resp.json().await.map_err(|error| {
                    PgCommandError::transport(
                        mode,
                        if error.is_timeout() {
                            shared_types::ERR_RUNTIME_TIMEOUT
                        } else {
                            shared_types::ERR_CONTAINER_EXEC_FAILED
                        },
                        if error.is_timeout() {
                            "PostgreSQL command response body exceeded the request deadline"
                        } else {
                            "PostgreSQL command response was incomplete"
                        },
                    )
                })?;
                if !status.is_success() || body["success"].as_bool() != Some(true) {
                    // A generic error envelope cannot prove whether execute-command
                    // was dispatched. Only the endpoint's validation denial is definitive.
                    let definitive =
                        status.is_client_error() && !matches!(status.as_u16(), 408 | 499);
                    let diagnostic = command_response_diagnostic(&body, command);
                    let error = PgCommandError::new(
                        if mode == shared_types::PgCommandMode::Write && !definitive {
                            shared_types::ERR_OPERATION_OUTCOME_UNKNOWN
                        } else {
                            shared_types::ERR_CONTAINER_EXEC_FAILED
                        },
                        format!("PostgreSQL command rejected: HTTP {status}"),
                        if definitive {
                            PgCommandEvidence::DefinitivelyRejected
                        } else {
                            PgCommandEvidence::OutcomeUnknown
                        },
                    );
                    return Err(match diagnostic {
                        Some(diagnostic) => error.with_diagnostic(diagnostic),
                        None => error,
                    });
                }
                let exit_code = body["exit_code"].as_i64().ok_or_else(|| {
                    PgCommandError::transport(
                        mode,
                        shared_types::ERR_CONTAINER_EXEC_FAILED,
                        "PostgreSQL command response has no exit status",
                    )
                })?;
                Ok(shared_types::CommandOutcome {
                    exit_code,
                    stdout: body["stdout"].as_str().unwrap_or_default().into(),
                    stderr: body["stderr"].as_str().unwrap_or_default().into(),
                })
            }
            Self::ProdRuntime { runtime, app_id } => {
                let result = runtime
                    .exec(app_id, vec!["sh".into(), "-c".into(), command.into()])
                    .await
                    .map_err(|error| {
                        container_runtime_api::runtime_pg_command_error(&error, mode)
                    })?;
                Ok(shared_types::CommandOutcome {
                    exit_code: result.exit_code,
                    stdout: result.stdout,
                    stderr: result.stderr,
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
pub(super) async fn resolve_exec_target<'a>(
    state: &'a AppState,
    app_stage: UserappStage,
    app_id: &str,
) -> Result<ExecChannel<'a>, AppError> {
    match app_stage {
        UserappStage::Dev => {
            let (info, _recreated) = ensure_userapp_builder_probed(state, app_id)
                .await
                .map_err(|error| crate::userapp_builder::control_error(&error))?;
            // dev 通道：dev 容器 file-server execute-command（契约见 ExecChannel）
            let channel = ExecChannel::DevHttp {
                base: crate::userapp_builder::dev_file_server_addr(state, &info)?,
                app_id: app_id.to_string(),
                credentials: super::file_credentials::credentials(
                    state,
                    UserappStage::Dev,
                    app_id,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(90),
                )
                .await
                .map_err(shared_types::WakeFailure::into_app_error)?,
                command_timeout: std::time::Duration::from_secs(90),
            };
            // builder 内 PG 可能刚 initdb（新容器/重建后），等就绪再执行改密命令
            let wait = channel
                .run(
                    &shared_types::pg_utils::pg_wait_ready_cmd(60),
                    shared_types::PgCommandMode::ReadOnly,
                )
                .await
                .map_err(|error| error.into_app_error("database_readiness"))?;
            if wait.exit_code != 0 {
                return Err(AppError::with_message(
                    shared_types::ERR_DATABASE_NOT_READY,
                    "Development PostgreSQL is not ready after container preparation",
                ));
            }
            Ok(channel)
        }
        UserappStage::Prod => {
            state
                .app_service
                .get_app(app_id)
                .await
                .map_err(AppError::from)?;
            use shared_types::AppWakeControl;
            match state.activity.ensure_running(app_id).await {
                shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
                shared_types::WakeOutcome::Blocked { message, blocker } => {
                    return Err(AppError::conflict(&message).with_blocker(blocker));
                }
                shared_types::WakeOutcome::Timeout(failure)
                | shared_types::WakeOutcome::Failed(failure) => {
                    return Err(failure.into_app_error());
                }
            }
            // 唤醒后容器内 PG 启动窗口：等就绪再交还 exec 通道
            let channel = ExecChannel::ProdRuntime {
                runtime: state.runtime(),
                app_id: app_id.to_string(),
            };
            let wait = channel
                .run(
                    &shared_types::pg_utils::pg_wait_ready_cmd(60),
                    shared_types::PgCommandMode::ReadOnly,
                )
                .await
                .map_err(|error| error.into_app_error("database_readiness"))?;
            if wait.exit_code != 0 {
                return Err(AppError::with_message(
                    shared_types::ERR_DATABASE_NOT_READY,
                    "Production PostgreSQL is not ready after wake",
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
        E::Command { code, .. } => code,
    }
}

fn db_admin_app_error(error: shared_types::DbAdminError) -> AppError {
    let code = db_admin_error_code(&error);
    let message = error.to_string();
    if let shared_types::DbAdminError::Command {
        stage,
        detail,
        evidence,
        cause_code,
        diagnostic,
        ..
    } = error
    {
        let retryable = evidence == shared_types::PgCommandEvidence::NotDispatched
            && matches!(
                code,
                shared_types::ERR_RUNTIME_UNAVAILABLE | shared_types::ERR_RUNTIME_TIMEOUT
            );
        if let Some(diagnostic) = diagnostic {
            return diagnostic.into_app_error(code, cause_code, stage, &detail, retryable);
        }
        return AppError::with_message(code, message).with_error_detail(
            shared_types::ErrorDetail::new(cause_code, stage, detail).with_retryable(retryable),
        );
    }
    AppError::with_message(code, message)
}

fn command_response_diagnostic(
    body: &serde_json::Value,
    command: &str,
) -> Option<shared_types::PgCommandDiagnostic> {
    let code = body
        .get("code")
        .and_then(serde_json::Value::as_str)
        .filter(|code| !code.is_empty() && *code != "UNKNOWN_ERROR")
        .unwrap_or_default()
        .to_owned();
    let operation_id = body
        .get("operation_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let blocker = body
        .get("blocker")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let mut error_detail: Option<shared_types::ErrorDetail> = body
        .get("error_detail")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    if let Some(detail) = &mut error_detail {
        // Never echo the credential-bearing command sent to execute-command.
        detail.detail = detail.detail.replace(command, "[REDACTED COMMAND]");
        detail.hint = detail.hint.replace(command, "[REDACTED COMMAND]");
        *detail = detail.localized(shared_types::current_request_locale());
    }
    if code.is_empty() && operation_id.is_none() && blocker.is_none() && error_detail.is_none() {
        return None;
    }
    Some(shared_types::PgCommandDiagnostic {
        code,
        operation_id,
        blocker,
        error_detail,
    })
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
        (status = 200, description = "HttpResult：成功表示密码已设置且 TCP 验证成功；失败检查 code/message/operation_id，非法输入 ERR_VALIDATION、身份或操作冲突 ERR_CONFLICT、后端失败 ERR_BACKEND_ERROR。未知结果保持保护", body = HttpResult<String>)
    ),
    tag = "Userapp · 双态 · 数据库",
    operation_id = "userapp_db_reset_password",
    summary = "重置/创建 PG 账号密码",
    description = r#"
设置目标容器内 PG 的账号密码，两种语义：

- **不带 username**：目标为 PGDATA 中持久化的初始化管理员；
- **带 username**：账号 upsert——角色存在则 ALTER USER 改密，不存在则 CREATE ROLE
  建号后再设密。

dbx 预置连接为容器内 local-pg socket 免密（与改密链解耦——改密不影响 dbx 访问）。
prod 环境需要目标容器已运行；未运行时明确返回错误，由用户先启动容器。

运行账号也可直接改密。此接口不重启应用或容器，不主动断开现有数据库会话。
已认证会话继续使用；后续采用密码认证的新连接必须使用新密码。应用连接配置更新及重启由用户决定。
request_id 用于原请求重放，lifecycle_id 用于拒绝已换代应用；建议调用方始终传入两者。
受理后的协调任务不会因 HTTP 断连而取消。写结果未知时保留操作与租约，不能换 request_id 重试绕过。
成功表示数据库已确认写入且 TCP 凭据验证通过；密码不进入操作记录或错误响应。
"#,
)]
pub(crate) async fn reset_password(
    State(state): State<Arc<AppState>>,
    Path(app_stage): Path<String>,
    Json(body): Json<shared_types::UserappDbResetPasswordRequest>,
) -> Result<HttpResult<String>, AppError> {
    let app_stage = UserappStage::parse(&app_stage)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_app_stage_error(&app_stage)))?;
    body.validate().map_err(|e| AppError::bad_request(&e))?;

    super::db_password::execute(state, app_stage, body)
        .await
        .map(HttpResult::success)
}

/// Reconcile or cancel the original uncertain password write on its captured target.
#[utoipa::path(
    post,
    path = "/api/v1/userapp/db/{app_stage}/reset-password/recover",
    request_body = shared_types::UserappDbPasswordRecoveryRequest,
    params(("app_stage" = String, Path, description = "Original environment: dev or prod")),
    responses(
        (status = 200, description = "HttpResult：成功时检查 data.state 和 lease_cleanup_pending；失败时检查 ERR_VALIDATION、ERR_CONFLICT 或 ERR_BACKEND_ERROR。身份、回执或 TCP 验证未确认时保持保护", body = HttpResult<shared_types::UserappDbPasswordRecoveryResponse>)
    ),
    tag = "Userapp · 双态 · 数据库",
    operation_id = "userapp_db_recover_password",
    summary = "确认或取消原改密操作",
    description = "显式恢复：携带原请求（包括原 request_id 和密码）、lifecycle_id、operation_id、expected_revision。已提交的 PG 事务经 TCP 验证后记 Succeeded；未提交的请求通过事务墓碑阻止迟到写入，记 Failed。不会重新改密、启动或替换容器。旧版本无事务回执协议的操作拒绝自动恢复。终态重放返回同一结果；lease_cleanup_pending 表示仅原租约清理待完成。"
)]
pub(crate) async fn recover_password(
    State(state): State<Arc<AppState>>,
    Path(app_stage): Path<String>,
    Json(body): Json<shared_types::UserappDbPasswordRecoveryRequest>,
) -> Result<HttpResult<shared_types::UserappDbPasswordRecoveryResponse>, AppError> {
    let stage = UserappStage::parse(&app_stage)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_app_stage_error(&app_stage)))?;
    super::db_password::recover(state, stage, body)
        .await
        .map(HttpResult::success)
}

/// Reconcile an uncertain explicit deployment password write under its original operation.
#[utoipa::path(
    post,
    path = "/api/v1/userapp/deploy-pg/recover",
    request_body = shared_types::UserappDeployPgRecoveryRequest,
    responses(
        (status = 200, description = "HttpResult：成功时检查 data.state/data.stage；失败时检查 ERR_VALIDATION、ERR_CONFLICT 或 ERR_BACKEND_ERROR。回执未确认、物理目标被替换或 TCP 验证失败时保持保护", body = HttpResult<shared_types::UserappDeployPgRecoveryResponse>)
    ),
    tag = "Userapp · 双态 · 数据库",
    operation_id = "userapp_deploy_pg_recover",
    summary = "确认或取消原显式部署改密写入",
    description = "仅当原操作已进入 RecoveryRequired，沿原部署操作身份恢复；Running 操作仍可能执行后续步骤，拒绝取消回执和释放租约。显式部署携带 `pg` 输入的改密写结果未知（断连/协调器中断/checkpoint 提交失败）时：携带 app_id/lifecycle_id/operation_id/expected_revision 与原 pg 的 username/password。已提交的事务经回执+TCP 验证确认；未提交的通过取消墓碑阻止迟到写入。两种结果都终局为 Failed——部署本身未记录完成证据，需重发部署（已确认的密码不会再次改写）。不会重新执行密码写、重新部署、启动或替换容器；旧版本无事务回执协议的操作拒绝自动恢复。终态重放返回同一结果；lease_cleanup_pending 表示仅原租约清理待完成。"
)]
pub(crate) async fn recover_deploy_pg(
    State(state): State<Arc<AppState>>,
    Json(body): Json<shared_types::UserappDeployPgRecoveryRequest>,
) -> Result<HttpResult<shared_types::UserappDeployPgRecoveryResponse>, AppError> {
    super::db_password::recover_deploy_pg(state, body)
        .await
        .map(HttpResult::success)
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
        (status = 200, description = "HttpResult：成功表示数据库已创建；非法输入 ERR_VALIDATION、已有数据库 ERR_CONFLICT、执行失败 ERR_DATABASE_COMMAND_FAILED，未知写入 ERR_OPERATION_OUTCOME_UNKNOWN。错误通过 code/message 返回", body = HttpResult<String>)
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
    body.validate().map_err(|e| AppError::bad_request(&e))?;

    let runner = resolve_exec_target(&state, app_stage, &body.app_id).await?;
    shared_types::create_pg_database(&runner, &body.database, body.owner.as_deref())
        .await
        .map_err(db_admin_app_error)?;
    info!(
        "[USERAPP_DB_ADMIN] database created: app_stage={}, app_id={}, database={}, owner={:?}",
        app_stage.as_str(),
        body.app_id,
        body.database,
        body.owner
    );
    Ok(HttpResult::success("数据库已创建".to_string()))
}

#[cfg(test)]
mod execution_key_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn database_http_execution_uses_optional_file_token_peer() {
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use axum::{Json, Router};
        for (required, configured, success) in [
            (false, None, true),
            (true, Some("fixture-file-token"), true),
            (true, None, false),
            (true, Some("wrong-token"), false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fixture");
            let base = format!("http://{}", listener.local_addr().expect("address"));
            let router = Router::new().route("/api/v1/userapp/execute-command", post(move |headers: HeaderMap| async move {
                assert!(!headers.contains_key("x-api-key"));
                if required && headers.get("x-proxy-token").and_then(|value| value.to_str().ok()) != Some("fixture-file-token") {
                    return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"success":false})));
                }
                (StatusCode::OK, Json(serde_json::json!({"success":true,"exit_code":0,"stdout":"1","stderr":""})))
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.expect("fixture server");
            });
            let channel = ExecChannel::DevHttp {
                base,
                app_id: "business".into(),
                credentials: shared_types::FileServerRequestCredentials {
                    proxy_token: configured.map(str::to_owned),
                },
                command_timeout: std::time::Duration::from_secs(5),
            };
            let result = channel
                .run("read-only fixture", shared_types::PgCommandMode::ReadOnly)
                .await;
            assert_eq!(
                result.is_ok(),
                success,
                "actual optional-token peer: {result:?}"
            );
            if success {
                assert_eq!(result.unwrap().stdout, "1");
            } else {
                assert_eq!(
                    result.unwrap_err().evidence,
                    shared_types::PgCommandEvidence::DefinitivelyRejected
                );
            }
            server.abort();
            drop(server.await);
        }
    }

    #[tokio::test]
    async fn database_http_body_timeout_preserves_cause_and_unknown_write() {
        for mode in [
            shared_types::PgCommandMode::ReadOnly,
            shared_types::PgCommandMode::Write,
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fixture");
            let base = format!("http://{}", listener.local_addr().expect("address"));
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut input = Vec::new();
                loop {
                    let mut bytes = [0; 4096];
                    let length = socket.read(&mut bytes).await.expect("request");
                    input.extend_from_slice(&bytes[..length]);
                    if length == 0 {
                        break;
                    }
                    if let Some(end) = input.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&input[..end]);
                        let body_length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if input.len() >= end + 4 + body_length {
                            break;
                        }
                    }
                }
                assert!(String::from_utf8_lossy(&input).contains("same-fixture-command"));
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"success\":true,").await.expect("partial response");
                stopped.await.expect("release fixture");
            });
            let channel = ExecChannel::DevHttp {
                base,
                app_id: "business".into(),
                credentials: shared_types::FileServerRequestCredentials::default(),
                command_timeout: std::time::Duration::from_millis(100),
            };
            let error = channel
                .run("same-fixture-command", mode)
                .await
                .expect_err("missing response body");
            stop.send(()).expect("stop fixture");
            server.await.expect("join fixture");
            assert_eq!(error.cause_code, shared_types::ERR_RUNTIME_TIMEOUT);
            assert_eq!(
                error.code,
                if mode == shared_types::PgCommandMode::Write {
                    shared_types::ERR_OPERATION_OUTCOME_UNKNOWN
                } else {
                    shared_types::ERR_RUNTIME_TIMEOUT
                }
            );
            assert_eq!(
                error.evidence,
                shared_types::PgCommandEvidence::OutcomeUnknown
            );
            let response = error
                .into_app_error("database_exec_response")
                .into_http_result::<()>("en-US");
            assert_eq!(
                response.error_detail.expect("detail").retryable,
                mode == shared_types::PgCommandMode::ReadOnly
            );
        }
    }
}
#[cfg(test)]
mod command_diagnostic_tests {
    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};
    use shared_types::{PgCommandEvidence, PgCommandMode};

    async fn server(status: axum::http::StatusCode) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let base = format!("http://{}", listener.local_addr().expect("fixture address"));
        let app = Router::new().route("/api/v1/userapp/execute-command", post(move || async move {
            (status, Json(serde_json::json!({
                "success": false, "code": shared_types::ERR_DATABASE_NOT_READY,
                "operation_id": "original-db-operation",
                "blocker": { "scope":"Prod", "operation_id":"blocking-stop", "kind":"Stop", "state":"Running", "step":"stop" },
                "error_detail": { "reason_code":shared_types::ERR_DATABASE_NOT_READY,
                    "stage":"database_readiness", "detail":"Database startup has not completed; password=private_response_marker",
                    "hint":"Wait for the original operation", "retryable":true,
                    "task_id":"original-database-task", "service_id":"postgres" }
            })))
        }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fixture server");
        });
        (base, task)
    }

    #[tokio::test]
    async fn devhttp_json_failure_preserves_original_error_and_operation() {
        let (base, server) = server(axum::http::StatusCode::OK).await;
        let channel = ExecChannel::DevHttp {
            base,
            app_id: "fixtureapp".into(),
            credentials: shared_types::FileServerRequestCredentials::default(),
            command_timeout: std::time::Duration::from_secs(5),
        };
        let error = shared_types::create_pg_database(&channel, "fixturedb", None)
            .await
            .expect_err("read rejected");
        let response = db_admin_app_error(error).into_http_result::<()>("en-US");
        assert_eq!(response.code, shared_types::ERR_DATABASE_NOT_READY);
        assert_eq!(
            response.operation_id.as_deref(),
            Some("original-db-operation")
        );
        assert_eq!(
            response.blocker.expect("original blocker").operation_id,
            "blocking-stop"
        );
        let detail = response.error_detail.expect("original diagnostic");
        assert_eq!(detail.stage, "database_readiness");
        assert_eq!(detail.task_id.as_deref(), Some("original-database-task"));
        assert_eq!(detail.service_id.as_deref(), Some("postgres"));
        assert!(detail.detail.contains("Database startup has not completed"));
        assert!(!format!("{detail:?}").contains("private_response_marker"));
        assert!(detail.retryable);
        server.abort();
        drop(server.await);
    }

    #[tokio::test]
    async fn devhttp_json_diagnostic_cannot_claim_an_unknown_write_is_retryable() {
        for (status, unknown) in [
            (axum::http::StatusCode::OK, true),
            (axum::http::StatusCode::UNAUTHORIZED, false),
        ] {
            let (base, server) = server(status).await;
            let channel = ExecChannel::DevHttp {
                base,
                app_id: "fixtureapp".into(),
                credentials: shared_types::FileServerRequestCredentials::default(),
                command_timeout: std::time::Duration::from_secs(5),
            };
            let error = channel
                .run("same-single-write", PgCommandMode::Write)
                .await
                .expect_err("write failed");
            assert_eq!(
                error.evidence,
                if unknown {
                    PgCommandEvidence::OutcomeUnknown
                } else {
                    PgCommandEvidence::DefinitivelyRejected
                }
            );
            let response = error
                .into_app_error("database_write")
                .into_http_result::<()>("en-US");
            assert_eq!(
                response.code,
                if unknown {
                    shared_types::ERR_OPERATION_OUTCOME_UNKNOWN
                } else {
                    shared_types::ERR_DATABASE_NOT_READY
                }
            );
            assert_eq!(
                response.operation_id.as_deref(),
                Some("original-db-operation")
            );
            let detail = response.error_detail.expect("original cause");
            assert_eq!(detail.reason_code, shared_types::ERR_DATABASE_NOT_READY);
            assert_eq!(detail.stage, "database_readiness");
            assert_eq!(detail.retryable, !unknown);
            server.abort();
            drop(server.await);
        }
    }

    #[tokio::test]
    async fn devhttp_password_upsert_redacts_peer_carrier_without_losing_write_evidence() {
        use std::sync::Mutex;

        let password = "peer-se'cret";
        let fragment = shared_types::pg_utils::pg_shell_quote(password);
        for locale in ["en-US", "zh-CN"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fixture listener");
            let base = format!("http://{}", listener.local_addr().expect("fixture address"));
            let commands = Arc::new(Mutex::new(Vec::<String>::new()));
            let observed = commands.clone();
            let fragment = fragment.clone();
            let app = Router::new().route(
                "/api/v1/userapp/execute-command",
                post(move |Json(body): Json<serde_json::Value>| {
                    let observed = observed.clone();
                    let fragment = fragment.clone();
                    async move {
                        let first = {
                            let mut commands = observed.lock().unwrap();
                            commands.push(body["command"].as_str().unwrap().to_owned());
                            commands.len() == 1
                        };
                        if first {
                            return Json(serde_json::json!({
                                "success":true, "exit_code":0, "stdout":"1", "stderr":""
                            }));
                        }
                        Json(serde_json::json!({
                            "success":false, "code":shared_types::ERR_RUNTIME_TIMEOUT,
                            "operation_id":"peer-password-operation",
                            "blocker":{ "scope":"Prod", "operation_id":"blocking-password-write",
                                "kind":"ResetProdDatabasePassword", "state":"RecoveryRequired", "step":"password_write_submitted" },
                            "error_detail":{ "reason_code":shared_types::ERR_RUNTIME_TIMEOUT,
                                "stage":"peer_exec_result", "detail":format!("Remote diagnostic fragment: {fragment}"),
                                "hint":format!("Inspect the captured fragment {fragment}"), "retryable":true,
                                "task_id":"peer-password-task", "service_id":"postgres" }
                        }))
                    }
                }),
            );
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.expect("fixture server");
            });
            let channel = ExecChannel::DevHttp {
                base,
                app_id: "fixtureapp".into(),
                credentials: shared_types::FileServerRequestCredentials::default(),
                command_timeout: std::time::Duration::from_secs(5),
            };
            let error = shared_types::scope_request_locale(
                locale,
                shared_types::upsert_pg_user(&channel, "business", password),
            )
            .await
            .expect_err("password write reply was not confirmed");
            let domain_debug = format!("{error:?}");
            let error = db_admin_app_error(error);
            let public_debug = format!("{error:?}");
            let response = error.into_http_result::<()>(locale);
            assert_eq!(response.code, shared_types::ERR_OPERATION_OUTCOME_UNKNOWN);
            assert_eq!(
                response.operation_id.as_deref(),
                Some("peer-password-operation")
            );
            assert_eq!(
                response.blocker.as_ref().unwrap().operation_id,
                "blocking-password-write"
            );
            let diagnostic = response.error_detail.as_ref().unwrap();
            assert_eq!(diagnostic.reason_code, shared_types::ERR_RUNTIME_TIMEOUT);
            assert_eq!(diagnostic.stage, "peer_exec_result");
            assert_eq!(diagnostic.task_id.as_deref(), Some("peer-password-task"));
            assert_eq!(diagnostic.service_id.as_deref(), Some("postgres"));
            assert!(!diagnostic.retryable);
            let json = serde_json::to_string(&response).unwrap();
            for output in [&domain_debug, &public_debug, &json] {
                assert!(
                    !output.contains("peer-se"),
                    "password peer carrier leaked: {output}"
                );
            }
            assert_eq!(
                diagnostic.hint,
                shared_types::get_error_hint(shared_types::ERR_RUNTIME_TIMEOUT, locale)
            );
            {
                let commands = commands.lock().unwrap();
                assert_eq!(
                    commands.len(),
                    2,
                    "unknown password writes must not be replayed"
                );
                assert!(commands[0].contains("pg_roles"));
                assert!(commands[1].contains("ALTER USER"));
            }
            server.abort();
            drop(server.await);
        }
    }
}
