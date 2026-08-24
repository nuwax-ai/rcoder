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
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    /// dev server 的 base path（vite --base 等; 缺省 "/"）。
    #[serde(default)]
    #[garde(skip)]
    /// dev server 的 base path（vite --base 等）；缺省 "/"
    pub base_path: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevLogsQuery {
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    /// 日志起始行（分页, 默认 1）。
    #[serde(default = "default_start_index")]
    /// 日志起始行（分页）；默认 1
    pub start_index: usize,
    /// "main"（当日汇总）或 "temp"（最新一次, 默认）。
    #[serde(default)]
    /// 日志类型：main=当日汇总 / temp=最新一次（默认）
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

/// `POST /api/userapp/dev/start`: 启动开发服务（UserApp workspace = spawn
/// app-cli 按 manifest run.command 编排全栈，pingap 9080 统一入口；web 域
/// 项目走原 vite 路径端口池分配）。
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

/// `POST /api/userapp/dev/build`: 异步开发编译（立即返回 taskId）。
/// 编译在后台执行（manifest 同核：逐子项目执行 project.manifest.toml 的
/// build.command，可能与发布打包完全同核——dev 编译通过 = 可部署；可能耗时
/// 数分钟，同步等待会断上游超时）。进度/结果查询复用任务族接口：轮询
/// `GET /api/userapp/tasks/{taskId}`（快照含 Failed 的错误信息）、SSE
/// `GET /api/userapp/tasks/{taskId}/logs/stream`、日志分页
/// `GET /api/userapp/tasks/{taskId}/logs`。与 `/api/userapp/build`（生产
/// 构建语义）共用编译内核，差异仅消费场景：dev 不进发布制品链。
#[utoipa::path(
    post,
    path = "/dev/build",
    request_body = DevOpBody,
    responses((status = 200, body = UserappDevTaskCreated, description = "构建任务已创建（taskId）")),
    tag = "UserApp"
)]
pub(crate) async fn dev_build(
    State(state): State<AppState>,
    Json(body): Json<DevOpBody>,
) -> Result<Json<UserappDevTaskCreated>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    tracing::info!(app_id = %body.app_id, user_id = %body.user_id, "userapp dev build");
    let task_id = spawn_dev_task(
        state,
        &body.app_id,
        body.base_path.map(|s| s.to_string()),
        crate::service::userapp::tasks::BuildTaskKind::DevBuild,
    )
    .await?;
    Ok(Json(UserappDevTaskCreated {
        app_id: body.app_id,
        task_id,
        status: "pending".to_string(),
    }))
}

/// `POST /api/userapp/dev/rebuild`: 异步一键编译 + 启动（agent 改完代码后
/// 单次调用，开发阶段闭环）。manifest 同核编译成功后自动重启 dev 服务
/// （app-cli 重拉全栈，新代码生效）；编译失败任务终态 Failed、旧服务原样
/// 保留（可继续用旧版本测试，不因中间态断流）。返回 taskId，终态后端口经
/// `GET /api/userapp/dev/list` 查询（UserApp workspace 恒为 pingap 9080）。
#[utoipa::path(
    post,
    path = "/dev/rebuild",
    request_body = DevOpBody,
    responses((status = 200, body = UserappDevTaskCreated, description = "重建任务已创建（taskId）")),
    tag = "UserApp"
)]
pub(crate) async fn dev_rebuild(
    State(state): State<AppState>,
    Json(body): Json<DevOpBody>,
) -> Result<Json<UserappDevTaskCreated>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    tracing::info!(app_id = %body.app_id, user_id = %body.user_id, "userapp dev rebuild");
    let task_id = spawn_dev_task(
        state,
        &body.app_id,
        body.base_path.map(|s| s.to_string()),
        crate::service::userapp::tasks::BuildTaskKind::DevRebuild,
    )
    .await?;
    Ok(Json(UserappDevTaskCreated {
        app_id: body.app_id,
        task_id,
        status: "pending".to_string(),
    }))
}

/// dev 异步编译任务受理响应（POST /dev/build、/dev/rebuild）。
/// camelCase 对齐 BuildCreatedData（Java 同一消费面）。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappDevTaskCreated {
    /// 应用 ID
    pub app_id: String,
    /// 异步任务 ID（轮询 /api/userapp/tasks/{taskId}、SSE /api/userapp/tasks/{taskId}/logs/stream）
    pub task_id: String,
    /// 受理时状态（pending——后台任务已创建）
    pub status: String,
}

/// dev 编译任务的公共骨架：create task + resolve workspace + spawn 后台执行。
/// DevBuild = 仅编译；DevRebuild = 编译成功后自动重启 dev server。
/// 终态：Completed（制品四字段占位空）/ Failed（友好错误）。
async fn spawn_dev_task(
    state: AppState,
    app_id: &str,
    base_path: Option<String>,
    kind: crate::service::userapp::tasks::BuildTaskKind,
) -> Result<String, AppError> {
    let task = state
        .build_tasks
        .create(app_id.to_string(), kind)
        .await
        .map_err(|e| AppError::business(e.to_string()))?;
    match resolve_userapp_dev(app_id, None, &state.config) {
        Ok(ws) => task.set_workspace_root(ws.clone()).await,
        Err(e) => {
            task.emit(shared_types::BuildProgressEvent::Failed {
                error: format!("resolve workspace: {e}"),
            })
            .await;
            return Ok(task.id.clone());
        }
    }
    let app_id = app_id.to_string();
    let task_clone = task.clone();
    tokio::spawn(async move {
        let key = dev_key(&app_id);
        let ws = task_clone.workspace_root().await.unwrap_or_else(|| {
            // set_workspace_root 已成功才走到 spawn；防御分支
            std::path::PathBuf::from(".")
        });
        // manifest 同核编译（单一编译事实源）：discover_projects → 逐子项目
        // 执行 project.manifest.toml 的 [build].command——与发布打包
        // build_workspace_package 完全同核（顺带产出制品 zip，dev 编译通过
        // = 可部署）。此前误用 web 域的 package.json/pnpm 引擎（vite 项目
        // 专用），对 UserApp 模板项目（Java/Go 多服务）不适用。
        let progress = task_clone.clone();
        let result = crate::service::userapp::build_workspace_package(
            &state.config,
            &state.build_manager,
            &app_id,
            state.config.dev_command_timeout_secs,
            Some(&progress),
        )
        .await;
        let outcome = async {
            result?;
            if matches!(
                kind,
                crate::service::userapp::tasks::BuildTaskKind::DevRebuild
            ) {
                // 编译成功：重启 dev 服务（失败则整体 Failed——旧服务已被
                // stop，语义上本轮重建失败，下次重试）
                state
                    .dev_server
                    .restart_dev(&key, &ws, base_path.as_deref())
                    .await?;
            }
            Ok::<(), AppError>(())
        }
        .await;
        match outcome {
            Ok(()) => {
                task_clone
                    .emit(shared_types::BuildProgressEvent::Completed {
                        // dev 任务无发布制品——占位（调用方按 status 消费，
                        // 新端口经 dev/list 查询）
                        release_id: String::new(),
                        sha256: String::new(),
                        size_bytes: 0,
                        file_name: String::new(),
                    })
                    .await;
            }
            Err(e) => {
                task_clone
                    .emit(shared_types::BuildProgressEvent::Failed {
                        error: e.to_string(),
                    })
                    .await;
            }
        }
    });
    Ok(task.id.clone())
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
