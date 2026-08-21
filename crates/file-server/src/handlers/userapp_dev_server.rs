//! `/api/userapp/dev/*`: UserApp 开发阶段的服务生命周期 + 开发编译（新契约, appId 唯一 key）。
//!
//! 复用 file-server 的 DevServerManager（进程/端口池/探活/日志）与 build 流水线,
//! 进程 key 用 `userapp:{appId}` 前缀与 web 项目的 projectId 空间隔离
//! （appId≡project_id 同值时, web 项目 workspace 在 project 树 / UserApp 在开发卷,
//! 同 key 会互踩路径）; 对外响应一律剥前缀回 appId。

use axum::extract::State;
use garde::Validate;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value, json};

use super::build::build_exec::build_project_impl;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::dev_server::{DevProcess, KilledPid, StartedDev, StoppedDev};
use crate::workspace::resolve_userapp_dev;

/// 进程表 key（与 web projectId 空间隔离; log_dir 剥前缀）。
fn dev_key(app_id: &str) -> String {
    format!("userapp:{app_id}")
}

/// key → 对外 appId（非本域 key 返回 None）。
fn app_id_of_key(key: &str) -> Option<&str> {
    key.strip_prefix("userapp:")
}

// ── DTO ────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevOpBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub app_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    /// dev server 的 base path（vite --base 等; 缺省 "/"）。
    #[serde(default)]
    #[garde(skip)]
    pub base_path: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevLogsQuery {
    pub app_id: String,
    /// 日志起始行（分页, 默认 1）。
    #[serde(default = "default_start_index")]
    pub start_index: usize,
    /// "main"（当日汇总）或 "temp"（最新一次, 默认）。
    #[serde(default)]
    pub log_type: Option<String>,
}
fn default_start_index() -> usize {
    1
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappDevStarted {
    pub success: bool,
    pub message: String,
    pub app_id: String,
    pub pid: u32,
    pub port: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappDevStopped {
    pub success: bool,
    pub message: String,
    pub app_id: String,
    pub pid: Option<u32>,
    pub killed_pids: Vec<KilledPid>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappDevProcess {
    pub app_id: String,
    pub pid: u32,
    pub port: u16,
    pub started_at: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappDevList {
    pub success: bool,
    pub list: Vec<UserappDevProcess>,
}

// ── handlers ───────────────────────────────────────────────────────────────────

/// `POST /api/userapp/dev/start`: 启动开发服务（端口池分配 + pnpm install + 探活等待）。
#[utoipa::path(
    post,
    path = "/dev/start",
    request_body = DevOpBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn dev_start(
    State(state): State<AppState>,
    Json(body): Json<DevOpBody>,
) -> Result<Json<UserappDevStarted>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    tracing::info!(app_id = %body.app_id, user_id = %body.user_id, "userapp dev start");
    let ws = resolve_userapp_dev(&body.app_id, None, &state.config)?;
    let started: StartedDev = state
        .dev_server
        .start_dev(&dev_key(&body.app_id), &ws, body.base_path.as_deref())
        .await?;
    Ok(Json(UserappDevStarted {
        success: true,
        message: "Development server started".to_string(),
        app_id: body.app_id,
        pid: started.pid,
        port: started.port,
    }))
}

/// `POST /api/userapp/dev/stop`: 停止开发服务（按 appId 定位进程组, 无需 pid）。
#[utoipa::path(
    post,
    path = "/dev/stop",
    request_body = DevOpBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn dev_stop(
    State(state): State<AppState>,
    Json(body): Json<DevOpBody>,
) -> Result<Json<UserappDevStopped>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let key = dev_key(&body.app_id);
    let stopped: StoppedDev = state.dev_server.stop_dev(&key).await?;
    state.log_cache.delete(&key)?;
    let all_killed = stopped.killed_pids.iter().all(|k| k.killed);
    let message = if stopped.killed_pids.is_empty() {
        "No running process found"
    } else if all_killed {
        "Stopped"
    } else {
        "Partially stopped but continue execution"
    };
    Ok(Json(UserappDevStopped {
        success: true,
        message: message.to_string(),
        app_id: body.app_id,
        pid: None,
        killed_pids: stopped.killed_pids,
    }))
}

/// `POST /api/userapp/dev/restart`: 重启开发服务（stop + start）。
#[utoipa::path(
    post,
    path = "/dev/restart",
    request_body = DevOpBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn dev_restart(
    State(state): State<AppState>,
    Json(body): Json<DevOpBody>,
) -> Result<Json<UserappDevStarted>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    tracing::info!(app_id = %body.app_id, user_id = %body.user_id, "userapp dev restart");
    let ws = resolve_userapp_dev(&body.app_id, None, &state.config)?;
    let started: StartedDev = state
        .dev_server
        .restart_dev(&dev_key(&body.app_id), &ws, body.base_path.as_deref())
        .await?;
    Ok(Json(UserappDevStarted {
        success: true,
        message: "Development server restarted".to_string(),
        app_id: body.app_id,
        pid: started.pid,
        port: started.port,
    }))
}

/// `POST /api/userapp/dev/build`: 开发编译（package.json scripts.build;
/// install + build + dist 拷贝, 失败返回解析后的友好错误）。与顶层
/// `/api/userapp/build`（workspace 打包出发布制品）语义不同。
#[utoipa::path(
    post,
    path = "/dev/build",
    request_body = DevOpBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn dev_build(
    State(state): State<AppState>,
    Json(body): Json<DevOpBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    tracing::info!(app_id = %body.app_id, user_id = %body.user_id, "userapp dev build");
    let ws = resolve_userapp_dev(&body.app_id, None, &state.config)?;
    build_project_impl(
        &state,
        &ws,
        &dev_key(&body.app_id),
        &body.app_id,
        body.base_path.as_deref(),
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Build completed",
        "appId": body.app_id,
    })))
}

/// `GET /api/userapp/dev/list`: 在跑的 UserApp 开发服务列表（不含 web/computer 项目）。
#[utoipa::path(
    get,
    path = "/dev/list",
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn dev_list(
    State(state): State<AppState>,
) -> Result<Json<UserappDevList>, AppError> {
    let processes: Vec<DevProcess> = state.dev_server.list_dev()?;
    let list = processes
        .into_iter()
        .filter_map(|p| {
            app_id_of_key(&p.project_id).map(|app_id| UserappDevProcess {
                app_id: app_id.to_string(),
                pid: p.pid,
                port: p.port,
                started_at: p.started_at,
            })
        })
        .collect();
    Ok(Json(UserappDevList {
        success: true,
        list,
    }))
}

/// `GET /api/userapp/dev/logs`: 开发服务日志（main=当日汇总 / temp=最新一次）。
#[utoipa::path(
    get,
    path = "/dev/logs",
    params(DevLogsQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn dev_logs(
    State(state): State<AppState>,
    Query(q): Query<DevLogsQuery>,
) -> Result<Json<Value>, AppError> {
    let result = state
        .dev_server
        .read_dev_log(
            &dev_key(&q.app_id),
            q.start_index,
            q.log_type.as_deref().unwrap_or("temp"),
        )
        .await?;
    let mut resp = json!({ "success": true, "appId": q.app_id });
    if let (Some(map), Some(Value::Object(extra))) =
        (resp.as_object_mut(), serde_json::to_value(&result).ok())
    {
        map.extend(extra);
    }
    Ok(Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// key 前缀隔离: appId → userapp:{appId}; 仅识别本域 key。
    #[test]
    fn dev_key_roundtrip_and_filtering() {
        assert_eq!(dev_key("my-app"), "userapp:my-app");
        assert_eq!(app_id_of_key("userapp:my-app"), Some("my-app"));
        // web/computer 域的 key 不属于本域
        assert_eq!(app_id_of_key("some-project-id"), None);
        assert_eq!(app_id_of_key("computer:u:c1"), None);
    }

    /// log_dir 剥 userapp: 前缀（目录名不带冒号）。
    #[test]
    fn log_dir_strips_userapp_prefix() {
        let cfg = crate::Config::default();
        let dir = crate::service::dev_server::log::log_dir(&cfg, "userapp:my-app");
        assert!(dir.ends_with("my-app"), "dir={}", dir.display());
    }

    /// dev_list 只返回 UserApp 域进程并剥前缀（构造带混合 key 的 manager 快照不易,
    /// 此处锁定 app_id_of_key 过滤语义——与 dev_list 的 filter_map 同谓词）。
    #[test]
    fn list_filter_semantics() {
        let keys = ["userapp:app-1", "web-proj", "computer:u:c"];
        let userapp: Vec<&str> = keys.iter().filter_map(|k| app_id_of_key(k)).collect();
        assert_eq!(userapp, vec!["app-1"]);
    }
}
