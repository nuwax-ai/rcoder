//! UserApp workspace HTTP handlers（独立 `/api/v1/userapp`，workspace 定位统一走 UserApp 开发卷）。
//!
//! 响应格式：JSON 接口统一 `shared_types::HttpResult`（`{code, message, data, tid, success}`）；
//! SSE（logs/stream）与静态文件（static）为特殊通道，不包 HttpResult。
//!
//! 异步编译/发布（task 10-12）：
//! - `POST /build`：workspace 多项目打包（异步：返 task_id，进度经 task 流出）。
//! - `GET  /tasks/{task_id}`：查任务状态快照（轮询通道）。
//! - `GET  /tasks/{task_id}/logs/stream`：任务进度 SSE（实时通道：进度事件 + 构建日志行 `log` 事件）。
//! - `POST /tasks/{task_id}/cancel`：取消进行中的编译任务（软取消 + kill 进程组）。
//! - `GET  /static/{app_id}`：按 app 直下构建整体包（缺省最新；`?release_id=` 指定版本）。
//!
//! 详见 `docs/application-management-service-v2-design.md` §5。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;

use crate::service::userapp::UserappBuildTask;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use garde::Validate;
use serde::Serialize;
use shared_types::HttpResult;

use crate::UserAppState;
use crate::models::{
    BuildCreatedData, BuildTaskSnapshot, BuildTaskStatus, BuildUserAppBody, CancelData,
    ConfirmData, DetectData, ProjectChainBody, StreamQuery, UserappTaskScopeQuery,
};
use crate::service::userapp;
use crate::service::userapp::tasks::BuildProgressEvent;
use file_server::error::{AppError, AppResult};
use file_server::extract::{AppJson, AppPath, AppQuery};

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

/// 解析 SSE 续传游标：`Last-Event-ID` 头优先，`?from_seq=` query 兜底。
///
/// SSE 规范（WHATWG）语义：断线重连时 `Last-Event-ID` 头的值是客户端最后收到
/// 事件的 `id:`，服务端应回放该 id **之后**的事件；而 `from_seq` 是"从哪个 seq
/// 开始（含）"，故头值需 +1 换算。头存在但非数字时忽略头回退 query
///（EventSource 不会发非数字 id，此分支只有手写客户端会触发，info 留痕）。
/// tasks 族作用域校验：app_id/user_id 必填白名单（app_id 原是 rcoder 转发层
/// 单方消费的隐式必填，本批下沉容器侧；user_id 为挂载分区组成段）。
fn validate_task_scope(scope: &UserappTaskScopeQuery, task_id: &str) -> Result<(), AppError> {
    shared_types::validate_identifier(&scope.app_id, "app_id")
        .map_err(|e| AppError::validation(e.to_string()))?;
    shared_types::validate_identifier(&scope.user_id, "user_id")
        .map_err(|e| AppError::validation(e.to_string()))?;
    tracing::debug!(app_id = %scope.app_id, user_id = %scope.user_id, %task_id, "task scope access");
    Ok(())
}

fn resolve_from_seq(last_event_id: Option<&str>, query_from_seq: u64) -> u64 {
    match last_event_id {
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(id) => id.saturating_add(1),
            Err(_) => {
                tracing::info!(
                    raw,
                    "Last-Event-ID header not numeric, fall back to from_seq query"
                );
                query_from_seq
            }
        },
        None => query_from_seq,
    }
}

/// 发起 workspace 打包
///
/// 异步任务：编译在后台 spawn 执行（`start_build_task`），受理即返 task_id；进度经 task 流出（轮询 `/tasks/{id}` +
/// SSE `/tasks/{id}/logs/stream`）。同 app_id 排队由 `BuildManager` per-project 互斥保证。
#[utoipa::path(
    post,
    path = "/build",
    request_body = BuildUserAppBody,
    responses((status = 200, body = HttpResult<BuildCreatedData>, description = "构建任务已受理（异步执行）。data 立即返回 task_id（轮询/SSE 用）与 artifact_path（受理时即确定：builds/workspace-package-{release_id}.zip，release_id 预生成）+ status=pending。同 app_id 已有活跃任务时在队列排队（per-app 互斥）；全局任务容量满时 4xx 拒绝。后续状态：轮询 GET /tasks/{task_id} 或订阅 GET /tasks/{task_id}/logs/stream（SSE，构建日志行以 log 事件实时推送）。")),
    tag = "UserApp · dev · 构建任务"
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

/// 获取构建任务状态快照
///
/// 构建进度的**轮询通道**（与 SSE 订阅二选一或互补）：受理 build 后拿 taskId
/// 周期拉取（建议 2-3s 间隔），快照含当前编译到哪个服务、进度日志摘要与产物
/// 信息——200 响应 schema 内有逐字段说明。
#[utoipa::path(
    get,
    path = "/tasks/{task_id}",
    params(
        UserappTaskScopeQuery,
        ("task_id" = String, Path, description = "任务ID"),
    ),
    description = r#"
返回构建任务的完整状态机快照。典型用法：

1. `POST /build` 受理 → 得 `task_id`；
2. 轮询本接口（2-3s）观察 `status`：`pending → running → completed|failed|cancelled`
   （后三个为终态，到达即停）；`current_service` 指示正在编译的子服务；
3. `completed` 后从 `release_id / artifact_path / size_bytes / file_name` 取产物摘要，
   直接接 `GET /static/{app_id}`（按 releaseId 回源取包）；
4. 需要实时滚动日志时改订阅 `GET /tasks/{task_id}/logs/stream`（SSE），
   `seq` 字段用于轮询→SSE 无缝续传（详见响应内说明）。

终态快照保留 24h 供回查；不存在/已清理 → 404。
"#,
    responses((status = 200, body = HttpResult<BuildTaskSnapshot>, description = "任务状态快照（轮询通道，建议 2-3s 间隔）。关键字段：status（pending/running/completed/failed/cancelled——后三者为终态，到终态即可停止轮询）、current_service（正在编译的服务）、release_id/sha256/size_bytes/file_name/artifact_path（completed 时有值：产物摘要）、error（failed 时有值）、seq（事件游标 = 已推送事件数，恰为下一条事件的 seq；从轮询切 SSE 续传时可直接作 from_seq 传，但勿直接作 Last-Event-ID 头——头语义是最后收到事件的 id，比本值小 1，直接用会漏一条事件）。终态快照保留 24h 供回查。")),
    tag = "UserApp · dev · 构建任务"
)]
pub(crate) async fn get_task(
    State(state): State<UserAppState>,
    AppPath(task_id): AppPath<String>,
    AppQuery(scope): AppQuery<UserappTaskScopeQuery>,
) -> UserAppReply<BuildTaskSnapshot> {
    let result = async {
        validate_task_scope(&scope, &task_id)?;
        let task = state
            .build_tasks
            .get(&task_id)
            .await
            .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
        Ok(task.snapshot().await)
    };
    reply(result.await)
}

/// 任务进度 SSE（实时通道）
///
/// 推送 `BuildProgressEvent`（event 名 = 事件类型，data = JSON 全量）；
/// 先回放 ring 里 `seq >= from_seq` 的历史，再实时跟随 broadcast，终态事件后关闭流。
#[utoipa::path(
    get,
    path = "/tasks/{task_id}/logs/stream",
    params(
        ("task_id" = String, Path, description = "任务ID"),
        ("Last-Event-ID" = Option<String>, Header, description = "SSE 规范续传头（优先）：断线重连时填最后收到事件的 id（即上一条消息的 seq），服务端从该 id 之后回放。浏览器 EventSource 自动重连会自动携带，无需手动处理。与 from_seq 同时存在时以本头为准。"),
        StreamQuery,
    ),
    responses(
        (
            status = 200,
            description = "SSE 任务进度流。每条消息 `id:<seq>` + `event:<事件名>` + `data:<JSON>`；seq 从 0 递增（首条事件 id:0）。断线续传两种方式（二选一）：① 请求带 `Last-Event-ID: <最后收到的seq>` 头（SSE 规范标准方式，浏览器 EventSource 自动重连自动携带，服务端从该 seq 之后回放）；② query `?from_seq=<最后seq+1>`（从该 seq 开始含本身回放；头存在时被忽略）。\n\n事件清单（event 名 → data 载荷）：\n- `building` → `{'event':'building','service':'<服务ID>'}`（开始构建某服务）\n- `log` → `{'event':'log','service':'<服务ID>','line':'一行构建输出'}`（构建日志行，实时逐行推送；出现在该服务的 building 与 build_ok/build_fail 之间，行序即进程输出顺序）\n- `build_ok` → `{'event':'build_ok','service':'...'}`（服务构建成功）\n- `build_fail` → `{'event':'build_fail','service':'...','error':'...'}`\n- `completed`（终态）→ `{'event':'completed','release_id':'...','sha256':'...','size_bytes':N,'file_name':'...','artifact_path':'builds/workspace-package-{release_id}.zip'}`\n- `failed`（终态）→ `{'event':'failed','error':'...'}`\n- `cancelled`（终态）→ `{'event':'cancelled'}`\n- `stream_lagged`（协议事件）→ `{'event':'stream_lagged','skipped':N}`——消费端落后超 broadcast 容量，服务端关流，客户端按上述任一方式带游标重连续传\n\n说明：构建日志以本流 `log` 事件实时推送（前端单流订阅即可）；断线按上述续传协议补齐——回放环有界（超环容量的早期行不可回补），长任务/超大输出建议任务创建后尽早订阅。构建串行执行（按 service_id 字母序逐服务构建），日志行按服务分段有序、不交错。`stage` 事件类型为协议预留，当前任务流不发送。终态事件（completed/failed/cancelled）后服务端关闭流；每 15s 发 `: keep-alive` 注释行保活。task 不存在时非 SSE：HttpResult JSON + 404。",
            content_type = "text/event-stream",
        ),
        (status = 404, description = "Task not found（HttpResult JSON，非 SSE）"),
    ),
    tag = "UserApp · dev · 构建任务"
)]
pub(crate) async fn stream_task_logs(
    State(state): State<UserAppState>,
    AppPath(task_id): AppPath<String>,
    AppQuery(q): AppQuery<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    // 错误路径 (校验失败/task 不存在) 也走 HttpResult shape, 与同组 JSON 接口一致
    // (成功路径是 SSE 流, 豁免 HttpResult)
    if let Err(e) = validate_task_scope(
        &UserappTaskScopeQuery {
            app_id: q.app_id.clone(),
            user_id: q.user_id.clone(),
        },
        &task_id,
    ) {
        return UserAppReply::<()>::Err(e).into_response();
    }
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

/// 取消进行中的编译任务
///
/// 双重取消：① 软取消（置 flag，build 循环服务间检查主动退出）；
/// ② 硬取消（kill 当前 build 子进程组，即时中断）。build 循环/错误分支随后 emit `Cancelled`。
#[utoipa::path(
    post,
    path = "/tasks/{task_id}/cancel",
    params(
        UserappTaskScopeQuery,
        ("task_id" = String, Path, description = "任务ID"),
    ),
    responses((status = 200, body = HttpResult<CancelData>, description = "取消结果。双重取消：软取消（置 flag）+ kill 编译进程组；已到终态的任务返回 already_terminal=true 幂等成功。取消成功后任务流发 cancelled 终态事件（SSE）")),
    tag = "UserApp · dev · 构建任务"
)]
pub(crate) async fn cancel_task(
    State(state): State<UserAppState>,
    AppPath(task_id): AppPath<String>,
    AppQuery(scope): AppQuery<UserappTaskScopeQuery>,
) -> UserAppReply<CancelData> {
    let result = async {
        validate_task_scope(&scope, &task_id)?;
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

/// 检测项目类型
///
/// 分析 workspace 内 `project_dir` 的文件结构（manifest/配置文件特征），
/// 推断 language/framework/build tool，作为 confirm 的输入。`app_id` 由
/// path 承载定位 workspace，body 仅需 `user_id` 与 `project_dir`。
#[utoipa::path(
    post,
    path = "/{app_id}/{app_stage}/projects/detect",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）")
    ),
    request_body = ProjectChainBody,
    responses((status = 200, body = HttpResult<DetectData>, description = "项目探测结果")),
    tag = "UserApp · dev · 工作区与工具链"
)]
pub(crate) async fn detect_project(
    State(state): State<UserAppState>,
    Path((app_id, _app_stage)): Path<(String, String)>,
    AppJson(body): AppJson<ProjectChainBody>,
) -> UserAppReply<DetectData> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        let workspace =
            file_server::workspace::resolve_userapp_dev(&app_id, None, &state.fs.config)?;
        let detection = userapp::import::detect_project(&workspace, &body.project_dir).await?;
        Ok(DetectData { detection })
    };
    reply(result.await)
}

/// 确认项目检测结果
///
/// 用户选择/修正 detect 推断的项目类型后提交（幂等）。附带 workspace 级
/// git init 双开关：`git_enabled` 开启时初始化本地仓库（失败仅告警不阻断
/// 确认），为 publish 快照 commit 的前提。
#[utoipa::path(
    post,
    path = "/{app_id}/{app_stage}/projects/confirm",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）")
    ),
    request_body = ProjectChainBody,
    responses((status = 200, body = HttpResult<ConfirmData>, description = "项目确认结果")),
    tag = "UserApp · dev · 工作区与工具链"
)]
pub(crate) async fn confirm_project(
    State(state): State<UserAppState>,
    Path((app_id, _app_stage)): Path<(String, String)>,
    AppJson(body): AppJson<ProjectChainBody>,
) -> UserAppReply<ConfirmData> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        let workspace =
            file_server::workspace::resolve_userapp_dev(&app_id, None, &state.fs.config)?;
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
