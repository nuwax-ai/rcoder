//! `/api/v1/userapp/dev/*`: Userapp 开发阶段的服务生命周期 + 开发编译（新契约, app_id 唯一 key）。
//!
//! 复用 file-server 的 DevServerManager（进程/端口池/探活/日志）与 build 流水线,
//! 进程 key 用 `userapp:{app_id}` 前缀与 web 项目的 projectId 空间隔离
//! （app_id≡project_id 同值时, web 项目 workspace 在 project 树 / Userapp 在开发卷,
//! 同 key 会互踩路径）; 对外响应一律剥前缀回 app_id。
//!
//! 响应格式：JSON 接口统一 `shared_types::HttpResult` 信封
//! （`{code, message, data, tid, success}`，经 `UserAppReply` 包装，与 build 域一致）；
//! data 载荷内不再重复 `success` 字段。

use axum::extract::State;
use garde::Validate;
use shared_types::HttpResult;

use super::userapp::{UserAppReply, reply};
use crate::UserAppState;
use crate::models::{
    BuildTaskStatus, DevOpBody, UserappDevList, UserappDevListQuery, UserappDevProcess,
    UserappDevStopped, UserappDevTaskCreated, UserappFrameworkDetection, UserappFrameworkInfo,
    UserappFrameworkInfoQuery, UserappServiceFrameworkInfo,
};
use file_server::error::AppError;
use file_server::extract::{AppJson as Json, AppQuery as Query};
use file_server::models::DevProcess;
use file_server::service::dev_server::StoppedDev;
use file_server::workspace::resolve_userapp_dev;

/// 进程表 key（与 web projectId 空间隔离; log_dir 剥前缀）。
fn dev_key(app_id: &str) -> String {
    format!("userapp:{app_id}")
}

/// key → 对外 app_id（非本域 key 返回 None）。
fn app_id_of_key(key: &str) -> Option<&str> {
    key.strip_prefix("userapp:")
}

/// app-cli 终局事件的有界等待上限（秒）。**必须覆盖最慢启动路径**：done 在
/// 逐服务 readiness 探测（各自 `[health].startup_timeout_seconds`，模板 java
/// 60s）+ pingap 就绪确认后才输出，而 start_dev 在 9080 listen 即返回——
/// 探测窗口内两者时间差可达数十秒。done 到达即 break（快路径零等待）；
/// 超时兜底按当前累积清单判终态（事件流仍是真相源）。曾为 2s：java 60s
/// 探测场景下 done 必然迟到、终态后事件被丢，部分失败误判 Completed。
const START_DONE_WAIT_MAX_SECS: u64 = 120;

/// 任务级日志行（快速路径说明等）的事件 service 标识——对齐编排日志源
/// `service_id=app-cli` 的既有命名。
const ORCHESTRATOR_LOG_SERVICE: &str = "app-cli";

/// 启动判定状态（EVT 回调同步写 / spawn 主流程 await 完后读）。
#[derive(Default)]
struct StartEventsState {
    /// app-cli 终局事件（orchestration_done）已到。
    done: bool,
    /// 启动失败清单（service, error）——done 的清单为权威值（覆盖累积）。
    failed: Vec<(String, String)>,
}

/// app-cli EVT 行（JSON 字符串，前缀已被 file-server 管道剥离）的映射结果。
#[derive(Debug)]
enum EvtOutcome {
    /// 可直接转发的进度事件。
    Event(shared_types::BuildProgressEvent),
    /// 终局事件：权威失败清单。
    Done { failed: Vec<(String, String)> },
}

/// app-cli EVT JSON → 进度事件/终局（跨进程 wire 契约；与 app-cli
/// `orchestration_events` 的 serde 形态一致——两端测试锁同一组字符串）。
/// 解析失败/未知事件返回 None（调用方 warn 丢弃，不影响编排）。
fn map_app_cli_evt(json: &str) -> Option<EvtOutcome> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let event = value.get("event")?.as_str()?;
    match event {
        "service_starting" => Some(EvtOutcome::Event(
            shared_types::BuildProgressEvent::ServiceStarting {
                service: value.get("service")?.as_str()?.to_string(),
            },
        )),
        "service_start_ok" => Some(EvtOutcome::Event(
            shared_types::BuildProgressEvent::ServiceStartOk {
                service: value.get("service")?.as_str()?.to_string(),
            },
        )),
        "service_start_fail" => Some(EvtOutcome::Event(
            shared_types::BuildProgressEvent::ServiceStartFail {
                service: value.get("service")?.as_str()?.to_string(),
                error: value.get("error")?.as_str()?.to_string(),
            },
        )),
        "orchestration_done" => {
            let failed = value
                .get("failed")?
                .as_array()?
                .iter()
                .map(|item| {
                    let service = item.get("service")?.as_str()?.to_string();
                    let error = item.get("error")?.as_str()?.to_string();
                    Some((service, error))
                })
                .collect::<Option<Vec<_>>>()?;
            Some(EvtOutcome::Done { failed })
        }
        _ => None,
    }
}

// ── handlers ───────────────────────────────────────────────────────────────────

/// 编译并启动 dev 服务
///
/// 异步任务：编译可能耗时，受理即返 task_id。任一服务配了 `[devrun]` 时走**源码态**：
/// 编译执行 `[devbuild].command`（缺省回落 `[build].command`，刷新源码目录产物、不打
/// zip），随后 app-cli 直接编排源码 workspace 并以 `[devrun].command`（缺省回落
/// `[run].command`）启动——热加载命令（vite/nodemon 等）改码即生效。未配 `[devrun]`
/// 的 app 走产物态（现状）：manifest 同核编译打 zip → 部署 `.run` → `[run].command`
/// （dev 编译通过=可部署）。编译失败任务终态 Failed、不启动。进度/结果：轮询
/// `GET /api/v1/userapp/tasks/{task_id}`、SSE `/api/v1/userapp/tasks/{task_id}/logs/stream`；
/// 终态后端口经 `GET /api/v1/userapp/dev/list` 查询（Userapp workspace 恒为 pingap 9080）。
/// 入参 basePath 对 Userapp workspace（manifest/app-cli 引擎）**无效**
/// ——pingap 路由前缀由各服务 project.manifest.toml `[proxy].path` 决定。
#[utoipa::path(
    post,
    path = "/dev/start",
    request_body = DevOpBody,
    responses((status = 200, body = HttpResult<UserappDevTaskCreated>, description = "启动任务已创建（task_id）")),
    tag = "Userapp · dev · 进程管理"
)]
pub(crate) async fn dev_start(
    State(state): State<UserAppState>,
    Json(body): Json<DevOpBody>,
) -> UserAppReply<UserappDevTaskCreated> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        tracing::info!(app_id = %body.app_id, user_id = %body.user_id, "userapp dev start");
        let task_id = spawn_dev_task(
            state,
            &body.app_id,
            body.base_path.map(|s| s.to_string()),
            DevTaskAction::Start,
        )
        .await?;
        Ok(UserappDevTaskCreated {
            app_id: body.app_id,
            task_id,
            status: BuildTaskStatus::Pending,
        })
    };
    reply(result.await)
}

/// 停止开发服务
///
/// 按 app_id 定位进程组，无需 pid。
/// **联动取消该 app 在途的 start/restart 任务**——否则编译中的任务会在
/// 编译完成后把刚停的服务重新拉起（停止意图被异步任务推翻）。
#[utoipa::path(
    post,
    path = "/dev/stop",
    request_body = DevOpBody,
    responses((status = 200, body = HttpResult<UserappDevStopped>, description = "停止结果（含进程组杀灭明细）")),
    tag = "Userapp · dev · 进程管理"
)]
pub(crate) async fn dev_stop(
    State(state): State<UserAppState>,
    Json(body): Json<DevOpBody>,
) -> UserAppReply<UserappDevStopped> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        let key = dev_key(&body.app_id);
        // 先取消在途任务（kill 编译进程组 + 终态 Cancelled），再停服务——
        // 顺序保证任务侧不会再有 start 动作追上来
        for task in state.build_tasks.active_tasks_for_app(&body.app_id).await {
            if !task.is_terminal().await {
                tracing::info!(
                    app_id = %body.app_id, task_id = %task.id,
                    "[DEV_STOP] cancelling in-flight dev task (stop intent)"
                );
                super::userapp::cancel_build_task(&task).await;
            }
        }
        let stopped: StoppedDev = state.fs.dev_server.stop_dev(&key).await?;
        state.fs.log_cache.delete(&key)?;
        let all_killed = stopped.killed_pids.iter().all(|k| k.killed);
        let message = if stopped.killed_pids.is_empty() {
            "No running process found"
        } else if all_killed {
            "Stopped"
        } else {
            "Partially stopped but continue execution"
        };
        Ok(UserappDevStopped {
            message: message.to_string(),
            app_id: body.app_id,
            pid: None,
            killed_pids: stopped.killed_pids,
        })
    };
    reply(result.await)
}

/// 编译并重启 dev 服务
///
/// agent 改完代码后的开发闭环——**重启前必须先编译**；异步任务立即返 task_id。
/// 任一服务配了 `[devrun]` 时走**源码态**：编译执行 `[devbuild].command`（缺省回落
/// `[build].command`），源码目录 release.lock 按 manifest 新旧自动重锁后 app-cli
/// 重编源码 workspace（`[devrun]` 优先）；未配则产物态（现状：同核编译打 zip →
/// `.run` 换入）。编译失败任务终态 Failed、旧服务原样保留（可继续用旧版本测试，
/// 不因中间态断流）。启动阶段逐服务 SSE 事件（service_starting/start_ok/
/// start_fail，单服务失败不阻塞其余）同 start 的说明。进度/结果查询同 start。
/// 入参 basePath 对 Userapp
/// workspace 无效（同 start 的说明）。
#[utoipa::path(
    post,
    path = "/dev/restart",
    request_body = DevOpBody,
    responses((status = 200, body = HttpResult<UserappDevTaskCreated>, description = "重启任务已创建（task_id）")),
    tag = "Userapp · dev · 进程管理"
)]
pub(crate) async fn dev_restart(
    State(state): State<UserAppState>,
    Json(body): Json<DevOpBody>,
) -> UserAppReply<UserappDevTaskCreated> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        tracing::info!(app_id = %body.app_id, user_id = %body.user_id, "userapp dev restart");
        let task_id = spawn_dev_task(
            state,
            &body.app_id,
            body.base_path.map(|s| s.to_string()),
            DevTaskAction::Restart,
        )
        .await?;
        Ok(UserappDevTaskCreated {
            app_id: body.app_id,
            task_id,
            status: BuildTaskStatus::Pending,
        })
    };
    reply(result.await)
}

/// dev 任务的后置动作（编译成功后执行哪个生命周期操作）。
pub(crate) enum DevTaskAction {
    Start,
    Restart,
}

/// dev 任务骨架：create task + resolve workspace + spawn 后台执行
/// （manifest 同核编译 → 成功后按 action 启动/重启 dev 服务）。
/// 终态：Completed（制品四字段占位空）/ Failed（友好错误）。
async fn spawn_dev_task(
    state: UserAppState,
    app_id: &str,
    base_path: Option<String>,
    action: DevTaskAction,
) -> Result<String, AppError> {
    let kind = match action {
        DevTaskAction::Start => crate::models::BuildTaskKind::DevStart,
        DevTaskAction::Restart => crate::models::BuildTaskKind::DevRestart,
    };
    let task = state
        .build_tasks
        .create(app_id.to_string(), kind)
        .await
        .map_err(|e| AppError::business(e.to_string()))?;
    // release_id 预生成（与 start_build_task 对称）：快照 pending 期即有确定性
    // artifact_path；build_workspace_package 同核产出制品 zip 落同一路径。
    let release_id = uuid::Uuid::now_v7().simple().to_string();
    let artifact_rel_path = crate::service::userapp::workspace_artifact_rel_path(&release_id);
    task.set_artifact_path(release_id.clone(), artifact_rel_path.clone())
        .await;
    // 源码态判定（[devrun] 触发，单一事实源）：决定 dev 编译/启动链路形态。
    // 判定失败（manifest 坏等）与 resolve 失败同款 fail-fast——受理即终态 Failed。
    let dev_source_mode = match resolve_userapp_dev(app_id, None, &state.fs.config) {
        Ok(ws) => {
            task.set_workspace_root(ws.clone()).await;
            match crate::service::userapp::dev_mode::dev_mode_enabled(&ws) {
                Ok(mode) => mode,
                Err(e) => {
                    task.emit(shared_types::BuildProgressEvent::Failed {
                        error: format!("dev mode detection: {e}"),
                    })
                    .await;
                    return Ok(task.id.clone());
                }
            }
        }
        Err(e) => {
            task.emit(shared_types::BuildProgressEvent::Failed {
                error: format!("resolve workspace: {e}"),
            })
            .await;
            return Ok(task.id.clone());
        }
    };
    let app_id = app_id.to_string();
    let task_clone = task.clone();
    tokio::spawn(async move {
        let key = dev_key(&app_id);
        let ws = task_clone.workspace_root().await.unwrap_or_else(|| {
            // set_workspace_root 已成功才走到 spawn；防御分支
            std::path::PathBuf::from(".")
        });
        // 编译（形态分派）：
        // - 产物态（现状）：manifest 同核编译（单一编译事实源）——discover →
        //   逐子项目 [build].command → 组 workspace zip（dev 编译通过 = 可部署）。
        // - 源码态（[devrun] 触发）：逐服务 [devbuild].command（缺省回落
        //   [build].command）刷新源码目录产物——不打 zip（热加载命令跑源码，
        //   制品无消费者；可部署性检查走 /api/v1/userapp/build）。
        let progress = task_clone.clone();
        let result = if dev_source_mode {
            crate::service::userapp::dev_mode::run_dev_builds(
                &state.fs.build_manager,
                &app_id,
                &ws,
                state.fs.config.dev_command_timeout_secs,
                Some(progress),
            )
            .await
            .map(|()| None)
        } else {
            crate::service::userapp::build_workspace_package(
                &state.fs.config,
                &state.fs.build_manager,
                &app_id,
                &release_id,
                state.fs.config.dev_command_timeout_secs,
                Some(progress),
            )
            .await
            .map(Some)
        };
        // 启动事件通道：app-cli stdout EVT 行（同步管道回调）→ unbounded 通道 →
        // 独立消费 task 异步 emit（emit 为 async；对齐构建日志管道先例）。
        // 消费 task 常驻到管道 EOF（app-cli 退出），fire-and-forget。
        let (evt_tx, mut evt_rx) =
            tokio::sync::mpsc::unbounded_channel::<shared_types::BuildProgressEvent>();
        let emit_task = {
            let task = task_clone.clone();
            tokio::spawn(async move {
                while let Some(event) = evt_rx.recv().await {
                    task.emit(event).await;
                }
            })
        };
        // 启动判定状态（回调同步写 / 主流程 await 完后读）：
        // - failed：启动失败清单（service, error）——orchestration_done 的清单为
        //   权威值（覆盖逐事件累积）
        // - done：app-cli 终局事件已到（9080 listen 即全部判定完成，通常先于
        //   start_dev 返回；bounded 等待为防御）
        let start_state = std::sync::Arc::new(std::sync::Mutex::new(StartEventsState::default()));
        let on_event = {
            let tx = evt_tx.clone();
            let start_state = start_state.clone();
            std::sync::Arc::new(move |json: &str| match map_app_cli_evt(json) {
                Some(EvtOutcome::Event(event)) => {
                    drop(tx.send(event));
                }
                Some(EvtOutcome::Done { failed }) => {
                    let mut state = start_state.lock().expect("start state lock");
                    state.done = true;
                    state.failed = failed;
                }
                None => {
                    tracing::warn!(json, "[DEV_START] unparsed app-cli EVT line dropped");
                }
            }) as file_server::service::dev_server::process::OnLineCallback
        };
        let outcome = async {
            // Start 快速路径：服务已在跑 → 跳过启停直接完成（廉价幂等）。
            // 注意：此处编译已在上方 await 完（产物态产出新 zip 但不部署不重启
            // ——已运行进程不加载新代码，要上新代码用 restart；源码态同理——
            // dev 编译刷新源码目录产物，热加载服务自身决定是否热更）。
            // 快速路径仅省去解压换入/锁 ensure 与启停。
            if matches!(action, DevTaskAction::Start)
                && state
                    .fs
                    .dev_server
                    .list_dev()?
                    .iter()
                    .any(|p| p.project_id == key)
            {
                tracing::info!(%app_id, "dev already running; start task completes without deploy/restart");
                task_clone
                    .emit(shared_types::BuildProgressEvent::Log {
                        service: ORCHESTRATOR_LOG_SERVICE.into(),
                        line: "dev already running; start task completes without deploy/restart".into(),
                    })
                    .await;
                return Ok::<(), AppError>(());
            }
            result?;
            // 编译成功但任务已被取消（cancel 落在编译完成后的打包/探活窗口
            // ——pid 已清零只软取消）：不再执行启停，保持"取消=无副作用"
            // 与终态 Cancelled 一致
            if task_clone.is_cancelled() {
                tracing::info!(%app_id, "dev task cancelled after build; skipping start/restart");
                return Ok(());
            }
            // 启动 workspace 根（形态分派）：
            // - 产物态：zip 部署到 {ws}/.run 原子换入（dev 运行编译产物）。
            // - 源码态：ensure 源码目录 release.lock（mtime 检测自动重锁），
            //   app-cli 直接编排源码 workspace（devrun 优先、run 兜底）。
            // 两种形态失败语义一致：旧运行态原样保留，任务 Failed。
            let run_root = if dev_source_mode {
                crate::service::userapp::dev_mode::ensure_dev_lock(&ws).await?
            } else {
                crate::service::userapp::run_dir::prepare_run_dir(&ws, &release_id).await?
            };
            // 启动/重启（start_dev 内 poll_alive 宽松就绪——app-cli 进程存活
            // 即成功；单服务启动成败经 EVT 事件流逐服务呈现，见下方终态判定）
            match action {
                DevTaskAction::Start => {
                    state
                        .fs.dev_server
                        .start_dev(&key, &run_root, base_path.as_deref(), Some(on_event.clone()))
                        .await?;
                }
                DevTaskAction::Restart => {
                    state
                        .fs.dev_server
                        .restart_dev(&key, &run_root, base_path.as_deref(), Some(on_event.clone()))
                        .await?;
                }
            }
            // bounded 等 app-cli 终局事件：start_dev 在 9080（pingap）listen 即
            // 返回，但逐服务 readiness 探测可能仍在进行（java 60s 窗口）——done
            // 最晚在探测+pingap 确认后输出。等待期 poll_alive 的宽松语义不变，
            // 调用方此时段经 SSE 已可看到 service_starting（受理即订阅）。
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(START_DONE_WAIT_MAX_SECS);
            loop {
                {
                    let snapshot = start_state.lock().expect("start state lock");
                    if snapshot.done || tokio::time::Instant::now() >= deadline {
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let failed = start_state
                .lock()
                .expect("start state lock")
                .failed
                .clone();
            // 排空窗口：done 行到达时通道内 service 事件已全部 send（stdout 行序
            // 保证），但消费 task 的 emit 是异步的——短暂等待让 service_* 事件
            // 先于终态入环（SSE 消费方按序看到逐服务结果再收终态）。
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if !failed.is_empty() {
                // 部分服务启动失败：任务终态 Failed（逐服务汇总；调用方经 SSE
                // 事件流自明各服务成败）——**已启动服务保留运行**（不 stop；
                // dev/list 可查部分存活），与"失败不阻塞"语义一致。
                let summary = failed
                    .iter()
                    .map(|(service, error)| format!("{service}: {error}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(AppError::business(format!(
                    "服务启动失败（其余服务已启动）: {summary}"
                )));
            }
            Ok::<(), AppError>(())
        }
        .await;
        // 事件转发 task 显式 detach（drop JoinHandle）：由 stdout 管道 EOF
        // （app-cli 退出）自然收尾；终态 emit 在主流程（上方排空窗口后）。
        drop(emit_task);
        match outcome {
            Ok(()) => {
                task_clone
                    .emit(shared_types::BuildProgressEvent::Completed {
                        // dev 任务消费方按 status/端口（dev/list）取结果；制品字段
                        // 仍带真实值（同核编译产出制品 zip，artifact_path 可用于
                        // 手动取包校验）。快速路径（跳过部署）时制品为本次
                        // 编译产出，存在但未部署。
                        release_id: release_id.clone(),
                        sha256: String::new(),
                        size_bytes: 0,
                        file_name: String::new(),
                        artifact_path: artifact_rel_path.clone(),
                    })
                    .await;
            }
            Err(e) => {
                // 守卫对齐兄弟实现（start_build_task）：cancel 已置终态时不再
                // emit Failed（防"cancel 接口返回 cancelled 但终态是 Failed"
                // 的竞态不一致）
                if !task_clone.is_cancelled() && !task_clone.is_terminal().await {
                    task_clone
                        .emit(shared_types::BuildProgressEvent::Failed {
                            error: e.to_string(),
                        })
                        .await;
                }
            }
        }
    });
    Ok(task.id.clone())
}

/// 在跑的 Userapp 开发服务列表
///
/// 列出该 `app_id`（query 必填）在跑的 dev server 进程（pid/port/started_at）。
/// 不含 web/computer 项目进程——进程键按 app 维度前缀隔离，仅 Userapp 开发
/// 服务视角；`user_id` 为挂载压平契约字段（必填校验，不参与过滤）。
#[utoipa::path(
    get,
    path = "/dev/list",
    params(UserappDevListQuery),
    responses((status = 200, body = HttpResult<UserappDevList>, description = "该 app 在跑的 Userapp 开发服务列表（不含 web/computer 项目进程）")),
    tag = "Userapp · dev · 进程管理"
)]
pub(crate) async fn dev_list(
    State(state): State<UserAppState>,
    Query(q): Query<UserappDevListQuery>,
) -> UserAppReply<UserappDevList> {
    let result = async {
        shared_types::validate_identifier(&q.app_id, "app_id")
            .map_err(|e| AppError::validation(e.to_string()))?;
        shared_types::validate_identifier(&q.user_id, "user_id")
            .map_err(|e| AppError::validation(e.to_string()))?;
        tracing::debug!(app_id = %q.app_id, user_id = %q.user_id, "dev list");
        let wanted = dev_key(&q.app_id);
        let processes: Vec<DevProcess> = state.fs.dev_server.list_dev()?;
        let list = processes
            .into_iter()
            .filter(|p| p.project_id == wanted)
            .filter_map(|p| {
                app_id_of_key(&p.project_id).map(|app_id| UserappDevProcess {
                    app_id: app_id.to_string(),
                    pid: p.pid,
                    port: p.port,
                    started_at: p.started_at,
                })
            })
            .collect();
        Ok(UserappDevList { list })
    };
    reply(result.await)
}

/// workspace 框架识别
///
/// 识别该 app workspace 下**全部服务**（含 disabled，`enabled` 字段自明）的技术栈：
/// - **manifest 声明面**（权威）：service_id/type（node/java/python/go/rust/static）/
///   kind/dir/enabled
/// - **探测面**（package.json + 文件系统，纯只读毫秒级）：`build_framework`
///（构建/meta 框架：vite/nextjs/nuxt/astro/sveltekit 等 14 种）、`ui_framework`
///（react/vue3/vue2/svelte 等，与 build 维度**正交可同真**——next 项目
/// build=nextjs 且 ui=react）、每维度框架版本（三级口径：node_modules 实测 >
/// 精确声明 > range 提取，`version_source` 自明）、`package_manager`、`typescript`
///
/// 无法识别为 `name: "other"`（恒有值免判空）。design 设计模式支持判定建议：
/// `build_framework.name == "vite"`（设计模式注入插件为 vite 插件）。
#[utoipa::path(
    get,
    path = "/dev/framework-info",
    params(UserappFrameworkInfoQuery),
    responses((status = 200, body = HttpResult<UserappFrameworkInfo>, description = "全部服务的技术栈识别结果（manifest 声明面 + 框架探测面）")),
    tag = "Userapp · dev · 进程管理"
)]
pub(crate) async fn framework_info(
    State(state): State<UserAppState>,
    Query(q): Query<UserappFrameworkInfoQuery>,
) -> UserAppReply<UserappFrameworkInfo> {
    let result = async {
        shared_types::validate_identifier(&q.app_id, "app_id")
            .map_err(|e| AppError::validation(e.to_string()))?;
        shared_types::validate_identifier(&q.user_id, "user_id")
            .map_err(|e| AppError::validation(e.to_string()))?;
        let ws = resolve_userapp_dev(&q.app_id, None, &state.fs.config)?;
        // discover 严格模式：manifest 损坏即 400（识别接口必须给出可信清单，
        // 静默跳过坏服务会让调用方误判 workspace 结构）
        let discovered = shared_types::discover_projects(&ws)
            .map_err(|e| AppError::business(format!("discover workspace services: {e}")))?;
        let services = discovered
            .iter()
            .map(|project| {
                let dir = ws.join(&project.dir);
                let detected = frontend_detector::detect_project(&dir);
                UserappServiceFrameworkInfo {
                    service_id: project.service_id().to_string(),
                    name: project.manifest.project.name.clone(),
                    r#type: format!("{:?}", project.manifest.project.r#type).to_lowercase(),
                    kind: format!("{:?}", project.manifest.project.kind).to_lowercase(),
                    dir: project.dir.clone(),
                    enabled: project.manifest.project.enabled,
                    package_manager: detected.package_manager,
                    typescript: detected.typescript,
                    build_framework: to_detection(detected.build),
                    ui_framework: to_detection(detected.ui),
                }
            })
            .collect();
        Ok(UserappFrameworkInfo { services })
    };
    reply(result.await)
}

/// detector 领域结构 → wire DTO（分层：探测归 frontend-detector，展示归壳层）。
fn to_detection(hit: frontend_detector::FrameworkHit) -> UserappFrameworkDetection {
    UserappFrameworkDetection {
        name: hit.name.to_string(),
        display_name: hit.display_name.to_string(),
        declared_range: hit.declared_range,
        version: hit.version,
        version_source: hit.source.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// key 前缀隔离: app_id → userapp:{app_id}; 仅识别本域 key。
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
        let cfg = file_server::Config::default();
        let dir = file_server::service::dev_server::log::log_dir(&cfg, "userapp:my-app");
        assert!(dir.ends_with("my-app"), "dir={}", dir.display());
    }

    /// dev_list 只返回 Userapp 域进程并剥前缀（构造带混合 key 的 manager 快照不易,
    /// 此处锁定 app_id_of_key 过滤语义——与 dev_list 的 filter_map 同谓词）。
    #[test]
    fn list_filter_semantics() {
        let keys = ["userapp:app-1", "web-proj", "computer:u:c"];
        let userapp: Vec<&str> = keys.iter().filter_map(|k| app_id_of_key(k)).collect();
        assert_eq!(userapp, vec!["app-1"]);
    }
    /// EVT JSON 映射契约：与 app-cli orchestration_events 的 wire 字符串一致
    /// （跨进程行协议，两端测试锁同一组字面量；此处锁 file-server-userapp 侧）。
    #[test]
    fn maps_app_cli_evt_wire_to_progress_events() {
        match map_app_cli_evt(r#"{"event":"service_starting","service":"frontend"}"#) {
            Some(EvtOutcome::Event(shared_types::BuildProgressEvent::ServiceStarting {
                service,
            })) => assert_eq!(service, "frontend"),
            other => panic!("unexpected: {other:?}"),
        }
        match map_app_cli_evt(r#"{"event":"service_start_ok","service":"backend-go"}"#) {
            Some(EvtOutcome::Event(shared_types::BuildProgressEvent::ServiceStartOk {
                service,
            })) => assert_eq!(service, "backend-go"),
            other => panic!("unexpected: {other:?}"),
        }
        match map_app_cli_evt(
            r#"{"event":"service_start_fail","service":"backend-java","error":"probe timeout"}"#,
        ) {
            Some(EvtOutcome::Event(shared_types::BuildProgressEvent::ServiceStartFail {
                service,
                error,
            })) => {
                assert_eq!(service, "backend-java");
                assert_eq!(error, "probe timeout");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match map_app_cli_evt(
            r#"{"event":"orchestration_done","failed":[{"service":"s1","error":"e1"}]}"#,
        ) {
            Some(EvtOutcome::Done { failed }) => {
                assert_eq!(failed, vec![("s1".to_string(), "e1".to_string())]);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // 空 failed = 全部成功
        match map_app_cli_evt(r#"{"event":"orchestration_done","failed":[]}"#) {
            Some(EvtOutcome::Done { failed }) => assert!(failed.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }
        // 未知事件/坏 JSON → None（丢弃不 panic）
        assert!(map_app_cli_evt(r#"{"event":"something_else"}"#).is_none());
        assert!(map_app_cli_evt("not json").is_none());
    }
    /// DTO 转换：detector 领域结构字段全量透传（name/display/range/version/source）。
    #[test]
    fn to_detection_maps_all_fields() {
        let hit = frontend_detector::FrameworkHit {
            name: "vite",
            display_name: "Vite",
            declared_range: "^5.4.21".into(),
            version: Some("5.4.21".into()),
            source: frontend_detector::VersionSource::Installed,
        };
        let dto = to_detection(hit);
        assert_eq!(dto.name, "vite");
        assert_eq!(dto.display_name, "Vite");
        assert_eq!(dto.declared_range, "^5.4.21");
        assert_eq!(dto.version.as_deref(), Some("5.4.21"));
        assert_eq!(dto.version_source, "installed");
    }
}
