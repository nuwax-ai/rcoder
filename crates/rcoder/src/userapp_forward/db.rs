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

use crate::router::AppState;
use crate::userapp_publish::agent_runner::{dev_file_server_addr, ensure_userapp_builder};
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
    tag = "UserApp",
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
