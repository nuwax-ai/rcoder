//! UserApp workspace HTTP handlers（独立 `/api/userapp`，workspace 定位统一走 UserApp 开发卷）。
//!
//! 响应格式：JSON 接口统一 `shared_types::HttpResult`（`{code, message, data, tid, success}`）；
//! SSE（logs/stream）与静态文件（static）为特殊通道，不包 HttpResult。
//!
//! 异步编译/发布（task 10-12）：
//! - `POST /build`：workspace 多项目打包（异步：返 taskId，进度经 task 流出）。
//! - `GET  /tasks/{taskId}`：查任务状态快照（轮询通道）。
//! - `GET  /tasks/{taskId}/logs`：查构建日志（分页，复用 `read_dev_log`）。
//! - `GET  /tasks/{taskId}/logs/stream`：任务进度 SSE（实时通道，进度事件推送）。
//! - `POST /tasks/{taskId}/cancel`：取消进行中的编译任务（软取消 + kill 进程组）。
//! - `GET  /static/{app_id}`：按 app 直下最新构建整体包（复用 `serve_from_root` + COMPUTER_CORS）。
//!
//! 详见 `docs/application-management-service-v2-design.md` §5。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;

use crate::service::userapp::UserappBuildTask;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use garde::Validate;
use serde::{Deserialize, Serialize};
use shared_types::HttpResult;

use crate::UserAppState;
use crate::service::userapp;
use crate::service::userapp::tasks::{BuildProgressEvent, BuildTaskSnapshot, BuildTaskStatus};
use file_server::error::{AppError, AppResult};
use file_server::extract::deserialize_id_string;
use file_server::extract::{AppJson, AppPath, AppQuery};
use file_server::service::dev_server::log::{ReadDevLogResult, read_dev_log};

// ── HttpResult 转换层 ──────────────────────────────────────────────────────────

/// UserApp JSON 接口的统一响应：成功/失败都是 HttpResult shape + 语义 HTTP 状态码。
///
/// file-server 全局 AppError shape（`{success, code:"UNKNOWN_ERROR", error:{...}}`）服务于
/// TS 对齐路由不能全局改；UserApp 是 Rust 独有新业务（TS 无此路由），此处将 AppError
/// 映射为 HttpResult 错误（code/message + 4xx/5xx 状态码）。
pub(crate) enum UserAppReply<T> {
    Ok(T),
    Err(AppError),
}

impl<T: Serialize> IntoResponse for UserAppReply<T> {
    fn into_response(self) -> Response {
        use shared_types::error_codes as ec;
        match self {
            UserAppReply::Ok(data) => Json(HttpResult::success(data)).into_response(),
            UserAppReply::Err(e) => {
                let (code, status) = match &e {
                    AppError::Validation(..)
                    | AppError::ValidationI18n(..)
                    | AppError::Business(_) => (ec::ERR_VALIDATION, StatusCode::BAD_REQUEST),
                    AppError::Resource(_) => (ec::ERR_NOT_FOUND, StatusCode::NOT_FOUND),
                    AppError::Network(_) => (ec::ERR_SERVICE_UNAVAILABLE, StatusCode::BAD_GATEWAY),
                    AppError::Permission(_)
                    | AppError::System(_)
                    | AppError::File(_)
                    | AppError::Process(_) => (
                        ec::ERR_INTERNAL_SERVER_ERROR,
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ),
                };
                let result = HttpResult::<T>::error(code, &e.to_string());
                (status, Json(result)).into_response()
            }
        }
    }
}

/// 便捷转换：`AppResult<T>` → `UserAppReply<T>`（userapp_dev* 新契约接口复用）。
pub(crate) fn reply<T>(r: AppResult<T>) -> UserAppReply<T> {
    match r {
        Ok(data) => UserAppReply::Ok(data),
        Err(e) => UserAppReply::Err(e),
    }
}

// ── data DTO（HttpResult.data 载荷）───────────────────────────────────────────

/// build 响应 data（POST /build）。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildCreatedData {
    /// 构建任务 ID（轮询 /tasks/{taskId} 与 SSE 订阅用）
    pub task_id: String,
    /// 受理时状态（恒为 pending——异步任务已创建；与 /tasks/{taskId} 轮询共用
    /// BuildTaskStatus 状态机，序列化值 "pending"）
    pub status: BuildTaskStatus,
    /// 预生成的产物相对路径（`builds/workspace-package-{releaseId}.zip`，release_id
    /// 创建时即生成）——信息字段：标识本次构建的产物位置；实际取包按 app 直下
    /// `GET /api/userapp/static/{appId}`（服务端选最新产物，无需传路径）。
    pub artifact_path: String,
}

/// cancel 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelData {
    /// 被取消的任务 ID
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<BuildTaskStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_terminal: Option<bool>,
}

/// detect 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DetectData {
    pub detection: userapp::import::DetectionResult,
}

/// confirm 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfirmData {
    pub path: String,
}

/// `POST /api/userapp/build` 请求体。
#[derive(Debug, Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildUserAppBody {
    /// UserApp 标识（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）。
    #[serde(deserialize_with = "deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub app_id: String,
    /// 用户 ID（挂载压平契约字段：rcoder ensure builder 时组装宿主树
    /// `dev/{user_id}/{app_id}` 用；file-server 侧仅日志审计，不参与容器内定位）。
    #[serde(deserialize_with = "deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub user_id: String,
}

#[derive(Debug, Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportProjectBody {
    #[serde(deserialize_with = "deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub app_id: String,
    /// 用户 ID（挂载压平契约字段：rcoder ensure builder 组装宿主树用；file-server
    /// 侧仅日志审计，不参与容器内定位）。
    #[serde(deserialize_with = "deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub user_id: String,
    /// workspace 内的子项目目录名（模板 zip 的顶层目录；detect/confirm 的定位粒度）
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub project_dir: String,
}

/// 任务构建日志查询参数（`GET /tasks/{taskId}/logs`）。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskLogsQuery {
    /// 子项目目录名（= service_id）；留空读 workspace 根日志目录。
    #[serde(default)]
    pub service: Option<String>,
    /// 起始行号（1-based，对齐 get-dev-log）。
    #[serde(default = "default_start_index")]
    pub start_index: usize,
}

fn default_start_index() -> usize {
    1
}

/// SSE 订阅参数（`GET /tasks/{taskId}/logs/stream`）。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamQuery {
    /// 从哪个 seq 开始回放（含该 seq；0 = 从头）。仅作兜底——
    /// 请求带 `Last-Event-ID` 头时以头为准（头值 + 1 = 本值语义），query 被忽略。
    #[serde(default)]
    pub from_seq: u64,
}

/// 解析 SSE 续传游标：`Last-Event-ID` 头优先，`?fromSeq=` query 兜底。
///
/// SSE 规范（WHATWG）语义：断线重连时 `Last-Event-ID` 头的值是客户端最后收到
/// 事件的 `id:`，服务端应回放该 id **之后**的事件；而 `fromSeq` 是"从哪个 seq
/// 开始（含）"，故头值需 +1 换算。头存在但非数字时忽略头回退 query
///（EventSource 不会发非数字 id，此分支只有手写客户端会触发，info 留痕）。
fn resolve_from_seq(last_event_id: Option<&str>, query_from_seq: u64) -> u64 {
    match last_event_id {
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(id) => id.saturating_add(1),
            Err(_) => {
                tracing::info!(
                    raw,
                    "Last-Event-ID header not numeric, fall back to fromSeq query"
                );
                query_from_seq
            }
        },
        None => query_from_seq,
    }
}

/// `POST /api/userapp/build` —— 异步发起 workspace 打包，立即返 taskId + 产物路径。
///
/// 编译在后台 spawn 执行（`start_build_task`）；进度经 task 流出（轮询 `/tasks/{id}` +
/// SSE `/tasks/{id}/logs/stream`）。同 app_id 排队由 `BuildManager` per-project 互斥保证。
#[utoipa::path(
    post,
    path = "/build",
    request_body = BuildUserAppBody,
    responses((status = 200, body = HttpResult<BuildCreatedData>, description = "构建任务已受理（异步执行）。data 立即返回 taskId（轮询/SSE 用）与 artifactPath（受理时即确定：builds/workspace-package-{releaseId}.zip，releaseId 预生成）+ status=pending。同 app_id 已有活跃任务时在队列排队（per-app 互斥）；全局任务容量满时 4xx 拒绝。后续状态：轮询 GET /tasks/{taskId} 或订阅 GET /tasks/{taskId}/logs/stream（SSE）；构建日志分页 GET /tasks/{taskId}/logs。")),
    tag = "UserApp"
)]
pub(crate) async fn build_workspace(
    State(state): State<UserAppState>,
    AppJson(body): AppJson<BuildUserAppBody>,
) -> UserAppReply<BuildCreatedData> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        let (task_id, artifact_path) = userapp::start_build_task(
            &state.build_tasks,
            &state.fs.config,
            state.fs.build_manager.clone(),
            body.app_id.clone(),
            state.fs.config.dev_command_timeout_secs,
        )
        .await?;

        tracing::info!(app_id = %body.app_id, user_id = %body.user_id, %task_id, %artifact_path, "userapp build task started");
        Ok(BuildCreatedData {
            task_id,
            status: BuildTaskStatus::Pending,
            artifact_path,
        })
    };
    reply(result.await)
}

/// 获取 UserApp 构建任务状态快照（含进度日志摘要）
#[utoipa::path(
    get,
    path = "/tasks/{task_id}",
    params(("task_id" = String, Path, description = "任务ID")),
    responses((status = 200, body = HttpResult<BuildTaskSnapshot>, description = "任务状态快照（轮询通道，建议 2-3s 间隔）。关键字段：status（pending/running/completed/failed/cancelled——后三者为终态，到终态即可停止轮询）、currentService（正在编译的服务）、releaseId/sha256/sizeBytes/fileName/artifactPath（completed 时有值：产物摘要）、error（failed 时有值）、seq（事件游标，对齐 SSE 的 id）。终态快照保留 24h 供回查。")),
    tag = "UserApp"
)]
pub(crate) async fn get_task(
    State(state): State<UserAppState>,
    AppPath(task_id): AppPath<String>,
) -> UserAppReply<BuildTaskSnapshot> {
    let result = async {
        let task = state
            .build_tasks
            .get(&task_id)
            .await
            .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
        Ok(task.snapshot().await)
    };
    reply(result.await)
}

/// `GET /api/userapp/tasks/{taskId}/logs` —— 构建日志分页（复用 `read_dev_log`）。
#[utoipa::path(
    get,
    path = "/tasks/{task_id}/logs",
    params(("task_id" = String, Path, description = "任务ID"), TaskLogsQuery),
    responses((status = 200, body = HttpResult<ReadDevLogResult>, description = "构建日志分页（历史日志文件读取，非 SSE）。query：service=子项目目录名（留空=workspace 根日志）；startIndex=起始行号（1-based，用上批响应的 totalLines 翻页）。响应 data：logs[{line,content}]（行号+内容）、totalLines（总行数）、startIndex、logFileName。日志按天滚动（dev-YYYY-MM-DD.log），只读当前文件。")),
    tag = "UserApp"
)]
pub(crate) async fn get_task_logs(
    State(state): State<UserAppState>,
    AppPath(task_id): AppPath<String>,
    AppQuery(q): AppQuery<TaskLogsQuery>,
) -> UserAppReply<ReadDevLogResult> {
    let result = async {
        let task = state
            .build_tasks
            .get(&task_id)
            .await
            .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
        let ws_root = task
            .workspace_root()
            .await
            .ok_or_else(|| AppError::resource("build task workspace not resolved yet"))?;
        let dir = match q.service.as_deref() {
            Some(service) if !service.is_empty() => {
                shared_types::validate_service_id(service).map_err(|error| {
                    AppError::validation(format!("invalid log service selector: {error}"))
                })?;
                file_server::path_safety::ensure_within(&ws_root.join("logs"), service).map_err(
                    |_| AppError::validation("log service selector escapes workspace logs"),
                )?
            }
            _ => ws_root.join("logs"),
        };
        read_dev_log(
            &dir,
            q.start_index,
            "main",
            state.fs.config.log_read_max_bytes,
        )
        .await
    };
    reply(result.await)
}

/// `GET /api/userapp/tasks/{taskId}/logs/stream` —— 任务进度 SSE（实时通道）。
///
/// 推送 `BuildProgressEvent`（event 名 = 事件类型，data = JSON 全量）；
/// 先回放 ring 里 `seq >= from_seq` 的历史，再实时跟随 broadcast，终态事件后关闭流。
#[utoipa::path(
    get,
    path = "/tasks/{task_id}/logs/stream",
    params(
        ("task_id" = String, Path, description = "任务ID"),
        ("Last-Event-ID" = Option<String>, Header, description = "SSE 规范续传头（优先）：断线重连时填最后收到事件的 id（即上一条消息的 seq），服务端从该 id 之后回放。浏览器 EventSource 自动重连会自动携带，无需手动处理。与 fromSeq 同时存在时以本头为准。"),
        StreamQuery,
    ),
    responses(
        (
            status = 200,
            description = "SSE 任务进度流。每条消息 `id:<seq>` + `event:<事件名>` + `data:<JSON>`；seq 从 1 递增。断线续传两种方式（二选一）：① 请求带 `Last-Event-ID: <最后收到的seq>` 头（SSE 规范标准方式，浏览器 EventSource 自动重连自动携带，服务端从该 seq 之后回放）；② query `?fromSeq=<最后seq+1>`（从该 seq 开始含本身回放；头存在时被忽略）。\n\n事件清单（event 名 → data 载荷）：\n- `building` → `{'event':'building','service':'<服务ID>'}`（开始编译某服务）\n- `build_ok` → `{'event':'buildOk','service':'...'}`（服务编译成功；注意 data 内 tag 为 camelCase）\n- `build_fail` → `{'event':'buildFail','service':'...','error':'...'}`\n- `completed`（终态）→ `{'event':'completed','releaseId':'...','sha256':'...','sizeBytes':N,'fileName':'...','artifactPath':'builds/workspace-package-{releaseId}.zip'}`\n- `failed`（终态）→ `{'event':'failed','error':'...'}`\n- `cancelled`（终态）→ `{'event':'cancelled'}`\n- `stream_lagged`（协议事件）→ `{'event':'stream_lagged','skipped':N}`——消费端落后超 broadcast 容量，服务端关流，客户端按上述任一方式带游标重连续传\n\n说明：构建日志经独立接口 `GET /tasks/{taskId}/logs` 分页查询，不走本流；`stage`/`log` 两种事件类型为协议预留，当前任务流不发送。终态事件（completed/failed/cancelled）后服务端关闭流；每 15s 发 `: keep-alive` 注释行保活。task 不存在时非 SSE：HttpResult JSON + 404。",
            content_type = "text/event-stream",
        ),
        (status = 404, description = "Task not found（HttpResult JSON，非 SSE）"),
    ),
    tag = "UserApp"
)]
pub(crate) async fn stream_task_logs(
    State(state): State<UserAppState>,
    AppPath(task_id): AppPath<String>,
    AppQuery(q): AppQuery<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    // 错误路径 (task 不存在) 也走 HttpResult shape, 与同组 JSON 接口一致
    // (成功路径是 SSE 流, 豁免 HttpResult)
    let Some(task) = state.build_tasks.get(&task_id).await else {
        return UserAppReply::<()>::Err(AppError::resource(format!(
            "build task not found: {task_id}"
        )))
        .into_response();
    };
    let from_seq = resolve_from_seq(
        headers.get("last-event-id").and_then(|v| v.to_str().ok()),
        q.from_seq,
    );
    let (replay, mut rx) = task.subscribe(from_seq).await;

    let progress = stream! {
        // 回放历史事件（seq >= from_seq）
        for (seq, ev) in replay {
            let terminal = is_terminal_event(&ev);
            yield Ok::<_, Infallible>(event_from_progress(seq, &ev));
            if terminal {
                return;
            }
        }
        // 实时跟随 broadcast
        loop {
            match rx.recv().await {
                Ok((seq, ev)) => {
                    let terminal = is_terminal_event(&ev);
                    yield Ok::<_, Infallible>(event_from_progress(seq, &ev));
                    if terminal {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    let data = serde_json::json!({"event":"stream_lagged","skipped":skipped});
                    yield Ok::<_, Infallible>(Event::default().event("stream_lagged").data(data.to_string()));
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(Box::pin(progress))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// `POST /api/userapp/tasks/{taskId}/cancel` —— 取消进行中的编译任务。
///
/// 双重取消：① 软取消（置 flag，build 循环服务间检查主动退出）；
/// ② 硬取消（kill 当前 build 子进程组，即时中断）。build 循环/错误分支随后 emit `Cancelled`。
#[utoipa::path(
    post,
    path = "/tasks/{task_id}/cancel",
    params(("task_id" = String, Path, description = "任务ID")),
    responses((status = 200, body = HttpResult<CancelData>, description = "取消结果。双重取消：软取消（置 flag）+ kill 编译进程组；已到终态的任务返回 alreadyTerminal=true 幂等成功。取消成功后任务流发 cancelled 终态事件（SSE）")),
    tag = "UserApp"
)]
pub(crate) async fn cancel_task(
    State(state): State<UserAppState>,
    AppPath(task_id): AppPath<String>,
) -> UserAppReply<CancelData> {
    let result = async {
        let task = state
            .build_tasks
            .get(&task_id)
            .await
            .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
        if task.is_terminal().await {
            return Ok(CancelData {
                task_id,
                status: None,
                already_terminal: Some(true),
            });
        }
        cancel_build_task(&task).await;
        Ok(CancelData {
            task_id,
            status: Some(BuildTaskStatus::Cancelled),
            already_terminal: None,
        })
    };
    reply(result.await)
}

/// 任务的取消内核（soft cancel + kill 编译进程组 + emit Cancelled 终态）。
/// cancel_task handler 与 dev_stop 的在途任务联动取消共用。
pub(crate) async fn cancel_build_task(task: &Arc<UserappBuildTask>) {
    task.cancel();
    // 硬 cancel：kill 当前 build 子进程组（run_command_to_log 用 process_group(0)，pid==pgid）。
    if let Some(pid) = task.pid() {
        let killed = file_server::service::dev_server::process::kill_process_group(pid);
        tracing::info!(task_id = %task.id, pid, killed, "build task cancelled, process group signalled");
    } else {
        tracing::info!(task_id = %task.id, "build task cancelled (no active pid; soft cancel via loop check)");
    }
    // 主动 emit Cancelled：若 build 在循环间隙（非 build_generic 内），靠此置终态；
    // 若在 build_generic 内被 kill，错误分支的 is_cancelled 分支会 emit Cancelled
    //（终态保护丢弃这里的重复）。
    task.emit(BuildProgressEvent::Cancelled).await;
}

/// 检测项目类型（分析文件结构推断 language/framework/build tool）
#[utoipa::path(
    post,
    path = "/projects/detect",
    request_body = ImportProjectBody,
    responses((status = 200, body = HttpResult<DetectData>, description = "项目探测结果")),
    tag = "UserApp"
)]
pub(crate) async fn detect_project(
    State(state): State<UserAppState>,
    AppJson(body): AppJson<ImportProjectBody>,
) -> UserAppReply<DetectData> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        let workspace =
            file_server::workspace::resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
        let detection = userapp::import::detect_project(&workspace, &body.project_dir).await?;
        Ok(DetectData { detection })
    };
    reply(result.await)
}

/// 确认项目检测结果（用户选择/修正 detect 推断的项目类型后提交）
#[utoipa::path(
    post,
    path = "/projects/confirm",
    request_body = ImportProjectBody,
    responses((status = 200, body = HttpResult<ConfirmData>, description = "项目确认结果")),
    tag = "UserApp"
)]
pub(crate) async fn confirm_project(
    State(state): State<UserAppState>,
    AppJson(body): AppJson<ImportProjectBody>,
) -> UserAppReply<ConfirmData> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        let app_id = body.app_id.clone();
        let workspace =
            file_server::workspace::resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
        let path = userapp::import::confirm_project(&workspace, &body.project_dir).await?;
        // workspace 级 git init（幂等）：本地版本管理 + publish snapshot commit 的前提。
        // 放 handler 层（持有 config.git_enabled / author）；失败仅告警，不阻断 manifest 确认。
        if state.fs.config.git_enabled
            && let Err(e) = file_server::service::git::write::init_repo(
                &workspace,
                &state.fs.config.git_default_author_name,
                &state.fs.config.git_default_author_email,
            )
        {
            tracing::warn!(%app_id, error = %e, "workspace git init failed (non-blocking)");
        }
        Ok(ConfirmData { path })
    };
    reply(result.await)
}

// ── SSE helper ──────────────────────────────────────────────────────────────────

/// `BuildProgressEvent` → SSE `Event`（event 名 = 事件类型，data = JSON 全量）。
fn event_from_progress(seq: u64, ev: &BuildProgressEvent) -> Event {
    let name = match ev {
        BuildProgressEvent::Stage { .. } => "stage",
        BuildProgressEvent::Building { .. } => "building",
        BuildProgressEvent::BuildOk { .. } => "build_ok",
        BuildProgressEvent::BuildFail { .. } => "build_fail",
        BuildProgressEvent::Log { .. } => "log",
        BuildProgressEvent::Completed { .. } => "completed",
        BuildProgressEvent::Failed { .. } => "failed",
        BuildProgressEvent::Cancelled => "cancelled",
    };
    let data = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    Event::default().id(seq.to_string()).event(name).data(data)
}

/// 是否终态事件（SSE 收到后关闭流）。
fn is_terminal_event(ev: &BuildProgressEvent) -> bool {
    matches!(
        ev,
        BuildProgressEvent::Completed { .. }
            | BuildProgressEvent::Failed { .. }
            | BuildProgressEvent::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::resolve_from_seq;

    #[test]
    fn resolve_from_seq_prefers_last_event_id_header_with_plus_one() {
        // SSE 规范：头值是最后收到的 id，回放其后 → +1
        assert_eq!(resolve_from_seq(Some("7"), 0), 8);
        assert_eq!(resolve_from_seq(Some(" 7 "), 3), 8); // 容忍空白
        assert_eq!(resolve_from_seq(Some("0"), 0), 1);
        assert_eq!(resolve_from_seq(Some(&u64::MAX.to_string()), 0), u64::MAX);
    }

    #[test]
    fn resolve_from_seq_falls_back_to_query_when_header_absent_or_invalid() {
        assert_eq!(resolve_from_seq(None, 5), 5);
        assert_eq!(resolve_from_seq(None, 0), 0);
        // 非数字头（EventSource 不会发，仅手写客户端触发）→ 忽略头用 query
        assert_eq!(resolve_from_seq(Some("abc"), 5), 5);
        assert_eq!(resolve_from_seq(Some(""), 9), 9);
    }
}
