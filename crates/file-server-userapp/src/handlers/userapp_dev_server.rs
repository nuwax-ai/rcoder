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
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::{
    BuildTaskStatus, DevOpBody, DevOperationRecovery, UserappDevList, UserappDevListQuery,
    UserappDevProcess, UserappDevStopped, UserappDevTaskCreated, UserappFrameworkDetection,
    UserappFrameworkInfo, UserappFrameworkInfoQuery, UserappServiceFrameworkInfo,
};
use file_server::error::AppError;
use file_server::models::DevProcess;
use file_server::service::dev_server::StoppedDev;
use file_server::workspace::resolve_userapp_dev;

/// 进程表 key（与 web projectId 空间隔离; log_dir 剥前缀）。
pub(super) fn dev_key(app_id: &str) -> String {
    format!("userapp:{app_id}")
}

/// key → 对外 app_id（非本域 key 返回 None）。
fn app_id_of_key(key: &str) -> Option<&str> {
    key.strip_prefix("userapp:")
}

/// app-cli 终局事件的有界等待上限（秒）。
///
/// P1-06：旧版硬编码 3600——"Startup completion event timed out" 在 app-cli
/// 已失败退出时仍傻等一小时（原因是发送端保活 + EOF 不上报）；P1-03/04 修了
/// 通道语义后仍需合理上限：合法慢启动（PG + java readiness + pingap 确认）
/// 可达数百秒，但正常路径 done 先于 finish 到达——窗口仅兜底。
///
/// 最终值按 P1-06 `launch_budget()` 动态计算，此常量仅为后备默认。
const START_DONE_WAIT_MAX_SECS: u64 = 1200;

use crate::service::userapp::start_events::{StartEvent as EvtOutcome, StartEventPipe};

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
/// 编译三分派（`[devbuild]` 显式配置则执行；只配 `[devrun]` 的服务跳过——devrun
/// 自足不消费产物；未配 `[devrun]` 的服务回落 `[build].command` 刷新源码目录产物、
/// 不打 zip），随后 app-cli 直接编排源码 workspace 并以 `[devrun].command`（缺省回落
/// `[run].command`）启动——热加载命令（vite/nodemon 等）改码即生效。未配 `[devrun]`
/// 的 app 走产物态（现状）：manifest 同核编译打 zip → 部署 `.run` → `[run].command`
/// （dev 编译通过=可部署）。编译失败任务终态 Failed、不启动。进度/结果：轮询
/// `GET /api/v1/userapp/tasks/{task_id}`、SSE `/api/v1/userapp/tasks/{task_id}/logs/stream`；
/// 终态后端口经 `GET /api/v1/userapp/dev/list` 查询（Userapp workspace 恒为 pingap 9080）。
/// 入参 basePath 对 Userapp workspace（manifest/app-cli 引擎）**无效**
/// ——pingap 路由前缀由各服务 project.manifest.toml `[proxy].path` 决定。
/// 入参 pg（可选，与 prod start/restart 的 `pg` 同构 wire）：给出则注入
/// 编排进程 env 的 `POSTGRES_USER`/`POSTGRES_PASSWORD`（覆盖容器默认透传）
/// ——这是本次显式启动捕获的运行配置，不表示保存配置已即时修改数据库密码。
/// prod 受管账号遵守版本化配置保存/显式生效流程；本 dev 接口不能替代该流程。
/// 不传则使用当前容器运行配置；未知结果恢复必须保留原操作及原配置。
#[utoipa::path(
    post,
    path = "/dev/start",
    request_body = DevOpBody,
    responses(
        (status = 200, body = HttpResult<UserappDevTaskCreated>, description = "启动任务已创建（task_id）"),
    ),
    tag = "Userapp · dev · 进程管理"
)]
pub(crate) async fn dev_start(
    State(state): State<UserAppState>,
    Json(body): Json<DevOpBody>,
) -> UserAppReply<UserappDevTaskCreated> {
    // 参数校验最先：非法入参不触发 workspace IO（precheck 含目录扫描/discover）
    if let Err(e) = body.validate() {
        return reply(Err(file_server::error::from_garde(e)));
    }
    // 受理前置校验：空 workspace / manifest 无可用服务同步 4xx 拒绝
    // （专用错误码），不再创建"受理成功 → 秒级 failed"的任务。
    let precheck =
        match crate::service::userapp::precheck_dev_workspace(&body.app_id, &state.fs.config).await
        {
            Ok(p) => p,
            Err(e) => return super::userapp::dev_precheck_reply(e),
        };
    let result = async {
        tracing::info!(app_id = %body.app_id, "userapp dev start");
        let task_id = spawn_dev_task(
            state,
            &body.app_id,
            body.base_path.map(|s| s.to_string()),
            body.pg.clone(),
            DevTaskAction::Start,
            precheck,
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
        let lifecycle = state.build_tasks.dev_lifecycle(&body.app_id).await;
        let mut generation = lifecycle.lock().await;
        *generation = generation
            .checked_add(1)
            .ok_or_else(|| AppError::system("dev lifecycle generation exhausted"))?;
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
        // R05：managed 域停止不 ps 扫描——external 登记经运行 API 幂等停；
        // 登记缺失但有 owner 应答时明确拒绝（不抢杀未知进程）
        let workspace = resolve_userapp_dev(&body.app_id, None, &state.fs.config)
            .map_err(|e| AppError::business(format!("resolve workspace for stop: {e}")))?;
        let stopped: StoppedDev = state
            .fs
            .dev_server
            .stop_userapp_dev(&key, &workspace)
            .await?;
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
/// 任一服务配了 `[devrun]` 时走**源码态**：编译三分派（`[devbuild]` 显式配置则
/// 执行；只配 `[devrun]` 的服务跳过——devrun 自足；其余回落 `[build].command`），
/// 源码目录 release.lock 按 manifest 新旧自动重锁后 app-cli
/// 重编源码 workspace（`[devrun]` 优先）；未配则产物态（现状：同核编译打 zip →
/// `.run` 换入）。编译失败任务终态 Failed、旧服务原样保留（可继续用旧版本测试，
/// 不因中间态断流）。启动阶段逐服务 SSE 事件（service_starting/start_ok/
/// start_fail，单服务失败不阻塞其余）同 start 的说明。进度/结果查询同 start。
/// 入参 basePath 对 Userapp
/// workspace 无效（同 start 的说明）。
/// 入参 pg（可选）语义同 start 的说明（注入编排 env、覆盖容器默认透传）。
#[utoipa::path(
    post,
    path = "/dev/restart",
    request_body = DevOpBody,
    responses(
        (status = 200, body = HttpResult<UserappDevTaskCreated>, description = "重启任务已创建（task_id）"),
    ),
    tag = "Userapp · dev · 进程管理"
)]
pub(crate) async fn dev_restart(
    State(state): State<UserAppState>,
    Json(body): Json<DevOpBody>,
) -> UserAppReply<UserappDevTaskCreated> {
    // 参数校验最先（同 dev/start），再受理前置校验：空 workspace / 无可用
    // 服务同步拒绝。
    if let Err(e) = body.validate() {
        return reply(Err(file_server::error::from_garde(e)));
    }
    let precheck =
        match crate::service::userapp::precheck_dev_workspace(&body.app_id, &state.fs.config).await
        {
            Ok(p) => p,
            Err(e) => return super::userapp::dev_precheck_reply(e),
        };
    let result = async {
        tracing::info!(app_id = %body.app_id, "userapp dev restart");
        let task_id = spawn_dev_task(
            state,
            &body.app_id,
            body.base_path.map(|s| s.to_string()),
            body.pg.clone(),
            DevTaskAction::Restart,
            precheck,
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

/// dev 任务骨架：create task + spawn 后台执行
/// （manifest 同核编译 → 成功后按 action 启动/重启 dev 服务）。
/// 终态：Completed（制品四字段占位空）/ Failed（友好错误）。
///
/// workspace 就绪性（resolve + 源码态判定）由调用方受理前置校验
/// （`precheck_dev_workspace`）完成并以 `precheck` 传入——空目录 /
/// manifest 无可用服务在受理期即同步 4xx 拒绝，不再进入本函数。
///
/// `pg`：请求携带的 PG 凭据（可选）——透传给编排器 spawn（注入
/// POSTGRES_USER/POSTGRES_PASSWORD 覆盖容器默认透传）；None 维持旧行为。
async fn spawn_dev_task(
    state: UserAppState,
    app_id: &str,
    base_path: Option<String>,
    pg: Option<shared_types::StartPgCredential>,
    action: DevTaskAction,
    precheck: crate::service::userapp::DevWorkspacePrecheck,
) -> Result<String, AppError> {
    state
        .fs
        .dev_server
        .ensure_new_build_admissible(&dev_key(app_id))?;
    let workspace_activity = state
        .build_tasks
        .workspace_activity(app_id)
        .await
        .read_owned()
        .await;
    let kind = match action {
        DevTaskAction::Start => crate::models::BuildTaskKind::DevStart,
        DevTaskAction::Restart => crate::models::BuildTaskKind::DevRestart,
    };
    let lifecycle = state.build_tasks.dev_lifecycle(app_id).await;
    let generation = *lifecycle.lock().await;
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
    // 源码态判定（[devrun] 触发，单一事实源）由 precheck 带入：决定 dev
    // 编译/启动链路形态；workspace 根同步入任务快照供日志/SSE 解析。
    let dev_source_mode = precheck.dev_source_mode;
    let ws = precheck.ws;
    task.set_workspace_root(ws.clone()).await;
    let app_id = app_id.to_string();
    let task_clone = task.clone();
    let workers = state.build_tasks.workers.clone();
    let context = task.command_context();
    workers
        .spawn_identified(
            context.identity.clone(),
            context.scope(async move {
                let _workspace_activity = workspace_activity;
                let key = dev_key(&app_id);
                // 编译（形态分派）：
                // - 产物态（现状）：manifest 同核编译（单一编译事实源）——discover →
                //   逐子项目 [build].command → 组 workspace zip（dev 编译通过 = 可部署）。
                // - 源码态（[devrun] 触发）：逐服务三分派（[devbuild] 显式配置则执行；
                //   只配 [devrun] 的服务跳过——devrun 自足；其余回落 [build].command
                //   刷新源码目录产物）——不打 zip（热加载命令跑源码，
                //   制品无消费者；可部署性检查走 /api/v1/userapp/build）。
                // P3-03：构建前捕获 owner 期望（instance/revision）——构建期间
                // owner 被 stop/restart 时，提交按 ERR_REVISION_MISMATCH 拒绝
                // （不自动刷新重发，防绕过用户 stop）。无 owner 时无副作用。
                state
                    .fs
                    .dev_server
                    .capture_owner_expectation(&key, &ws)
                    .await;
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
                let (evt_tx, event_pipe) = StartEventPipe::new(task_clone.clone());
                // P1-06：启动预算取配置兜底——不能用硬编码 3600s 掩盖通道语义缺陷，
                // 也不能截断合法慢启动。此处以 dev_command_timeout_secs 为基线加
                // pingap 确认余量（30s）与调度余量（120s），再取上限 1200s 兜底。
                let launch_budget_secs = std::cmp::min(
                    START_DONE_WAIT_MAX_SECS,
                    state.fs.config.dev_command_timeout_secs.saturating_add(150),
                );
                let hook_tx = evt_tx.clone();
                let runtime_hooks_tx = evt_tx.clone();
                let hooks = file_server::service::dev_server::DevEventHooks {
                    on_line: {
                        let tx = hook_tx;
                        std::sync::Arc::new(move |json: &str| match map_app_cli_evt(json) {
                            Some(event) => {
                                drop(tx.send(event));
                            }
                            None => {
                                tracing::warn!(
                                    json,
                                    "[DEV_START] unparsed app-cli EVT line dropped"
                                )
                            }
                        })
                            as file_server::service::dev_server::process::OnLineCallback
                    },
                    on_end: Some(std::sync::Arc::new(move |end| {
                        drop(runtime_hooks_tx.send(EvtOutcome::StreamEnded {
                            reason: end.to_string(),
                        }));
                    })),
                };
                let outcome = async {
            result?;
            if task_clone.is_cancelled() {
                return Ok::<(), AppError>(());
            }
            // A registry entry only establishes ownership, not successful startup.
            // External owners always follow the identity-bound control operation.
            if matches!(action, DevTaskAction::Start)
                && pg.is_none()
                && state
                    .fs
                    .dev_server
                    .confirmed_local_manifest_ready(&key, &ws)
                    .await?
            {
                return Ok::<(), AppError>(());
            }
            // 编译成功但任务已被取消（cancel 落在编译完成后的打包/探活窗口
            // ——pid 已清零只软取消）：不再执行启停，保持"取消=无副作用"
            // 与终态 Cancelled 一致
            if task_clone.is_cancelled() {
                tracing::info!(%app_id, "dev task cancelled after build; skipping start/restart");
                return Ok(());
            }
            // 启动 workspace 根（形态分派）：
            // - 产物态（R03）：staging 只解压校验；**激活权归属 owner**——
            //   匹配 owner 在 → Restart(ArtifactId)（owner 侧身份/revision
            //   核验后自行解压/激活/编排，拒绝不改变 active 目录）；
            //   无 owner → 本地 activate + spawn（legacy 路径不变）。
            // - 源码态：ensure 源码目录 release.lock（mtime 检测自动重锁），
            //   app-cli 直接编排源码 workspace（devrun 优先、run 兜底）。
            // 两种形态失败语义一致：旧运行态原样保留，任务 Failed。
            if !dev_source_mode {
                match state
                    .fs
                    .dev_server
                    .route_artifact_restart(
                        &key,
                        &ws,
                        &release_id,
                        Some(hooks.clone()),
                        pg.as_ref(),
                        Some(&task_clone.id),
                    )
                    .await
                {
                    Ok(Some(started)) => {
                        // owner 已受理并确认 Succeeded（含事件转发/终态排空/
                        // external 登记，见 route_artifact_restart）——本地
                        // .run 未被触碰，无需 commit_start 的本地激活段
                        let _ = started;
                        tracing::info!(%app_id, "artifact restart routed through runtime owner");
                        return Ok(());
                    }
                    Ok(None) => { /* 无 owner：走下方本地激活 + spawn */ }
                    Err(error) => return Err(error),
                }
            }
            let prepared = if dev_source_mode {
                None
            } else {
                Some(crate::service::userapp::run_dir::prepare_run_dir(&ws, &release_id).await?)
            };
            if !task_clone
                .commit_start(&lifecycle, generation, async {
                    let run_root = match prepared {
                        Some(prepared) => prepared.activate()?,
                        None => crate::service::userapp::dev_mode::ensure_dev_lock(&ws).await?,
                    };
                    match action {
                        DevTaskAction::Start => {
                            state
                                .fs
                                .dev_server
                                .start_dev(
                                    &key,
                                    &run_root,
                                    base_path.as_deref(),
                                    Some(hooks.clone()),
                                    pg.as_ref(),
                                    Some(&task_clone.id),
                                )
                                .await?;
                        }
                        DevTaskAction::Restart => {
                            state
                                .fs
                                .dev_server
                                .restart_dev(
                                    &key,
                                    &run_root,
                                    base_path.as_deref(),
                                    Some(hooks.clone()),
                                    pg.as_ref(),
                                    Some(&task_clone.id),
                                )
                                .await?;
                        }
                    }
                    Ok::<(), AppError>(())
                })
                .await?
            {
                return Ok(());
            }
            if let Some(supervised) = state.fs.dev_server.supervised_child(&key) {
                let exit_tx = evt_tx.clone();
                let span = tracing::Span::current();
                tokio::spawn({
                    let supervised = supervised.clone();
                    async move {
                        if let Some(exit) = supervised
                            .wait_exit(std::time::Duration::from_secs(launch_budget_secs))
                            .await
                        {
                            let _guard = span.enter();
                            drop(exit_tx.send(EvtOutcome::ProducerExited {
                                exit: exit.describe(),
                            }));
                        }
                    }
                });
            }
            drop(evt_tx);
            event_pipe
                .finish(std::time::Duration::from_secs(launch_budget_secs))
                .await
        }
        .await;
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
            }),
        )
        .map_err(AppError::system)?;

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
        tracing::debug!(app_id = %q.app_id, "dev list");
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
        let ws = resolve_userapp_dev(&q.app_id, None, &state.fs.config)?;
        // discover 严格模式：manifest 损坏即 400（识别接口必须给出可信清单，
        // 静默跳过坏服务会让调用方误判 workspace 结构）
        let discovered = crate::service::userapp::discover_projects_async(&ws)
            .await
            .map_err(|e| AppError::business(format!("discover workspace services: {e}")))?;
        // frontend-detector 是纯同步 crate（逐服务读 package.json + 探测锁文件），
        // 整批移进阻塞池，避免每个服务各阻塞一次 worker
        let dirs: Vec<_> = discovered.iter().map(|p| ws.join(&p.dir)).collect();
        let detections = tokio::task::spawn_blocking(move || {
            dirs.iter()
                .map(|dir| frontend_detector::detect_project(dir))
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| AppError::system(format!("framework detect task: {e}")))?;
        let services = discovered
            .iter()
            .zip(detections)
            .map(|(project, detected)| UserappServiceFrameworkInfo {
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

#[cfg(test)]
mod precheck_reply_tests {
    use super::*;
    use crate::models::DevOpBody;
    use axum::response::IntoResponse;

    use super::super::userapp_files::tests_support::make_state;

    /// 受理全链（handler 层）：空 workspace → HTTP 400 + 信封 code=ERR_WORKSPACE_EMPTY，
    /// 且不创建任务（build_tasks 空）——不再走"200 受理 → 秒级 failed 任务"。
    #[tokio::test]
    async fn dev_start_rejects_empty_workspace_with_dedicated_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().to_path_buf());

        let reply = dev_start(
            State(state.clone()),
            Json(DevOpBody {
                app_id: "app-7".into(),
                base_path: None,
                pg: None,
            }),
        )
        .await;
        let resp = reply.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let (parts, body) = resp.into_parts();
        drop(parts);
        let bytes = axum::body::to_bytes(body, 16 * 1024)
            .await
            .expect("read body");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("ERR_WORKSPACE_EMPTY"),
            "code expected in envelope: {text}"
        );
        // 未创建任务：被拒受理不在任务表产生残留
        assert!(
            state
                .build_tasks
                .active_tasks_for_app("app-7")
                .await
                .is_empty(),
            "no task should be created"
        );
    }
}

/// 恢复原开发服务操作
///
/// 查询或重放已持久化的原操作，不重新编译源码、不创建新操作身份。
#[utoipa::path(
    post,
    path = "/dev/operations/{operation_id}/recover",
    params(("operation_id" = String, Path, description = "Original runtime operation ID")),
    request_body = DevOpBody,
    responses(
        (status = 200, body = HttpResult<DevOperationRecovery>, description = "Original operation observed or replayed; inspect state, this is not a promise of success"),
        (status = 400, description = "Invalid app, original credential mismatch, owner changed or recovery protected")
    ),
    description = "Explicitly resume the original operation after lost response or file-server restart. No source rebuild, no new operation ID. app_id identifies the application; optional pg must match the originally captured credentials. base_path is unused. Return state may still be running or RecoveryRequired.",
    tag = "Userapp · dev · 进程管理"
)]
pub(crate) async fn recover_dev_operation(
    State(state): State<UserAppState>,
    crate::extract::AppPath(operation_id): crate::extract::AppPath<String>,
    Json(body): Json<DevOpBody>,
) -> UserAppReply<DevOperationRecovery> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        shared_types::validate_identifier(&body.app_id, "app_id").map_err(AppError::validation)?;
        shared_types::validate_identifier(&operation_id, "operation_id")
            .map_err(AppError::validation)?;
        let workspace = resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
        let (task_id, operation) = state
            .fs
            .dev_server
            .recover_external_operation(
                &dev_key(&body.app_id),
                &workspace,
                &operation_id,
                body.pg.as_ref(),
            )
            .await?;
        Ok(DevOperationRecovery { task_id, operation })
    };
    reply(result.await)
}

#[cfg(test)]
mod operation_recovery_tests {
    use super::*;
    use axum::{Router, body::Body, http::Request};
    use sha2::{Digest, Sha256};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    fn make_state(config: file_server::Config) -> UserAppState {
        UserAppState::new(
            file_server::FileServer::builder(config)
                .build()
                .unwrap()
                .state(),
        )
    }
    async fn http_recover(state: UserAppState, operation: &str, app_id: &str) -> serde_json::Value {
        let router = Router::new()
            .route(
                "/recover/{operation_id}",
                axum::routing::post(recover_dev_operation),
            )
            .with_state(state);
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/recover/{operation}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"app_id": app_id}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn recover_handler_preserves_operation_after_lost_reply_and_process_restart() {
        for accepted_before_loss in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspaces/app123");
            std::fs::create_dir_all(&workspace).unwrap();
            let mut config = file_server::Config::from_env().unwrap();
            config.userapp_single_app_id = None;
            config.userapp_workspace_dir = root.path().join("workspaces");
            config.log_base_dir = root.path().join("logs");
            std::fs::create_dir_all(&config.log_base_dir).unwrap();
            let application_id = std::env::var("PROJECT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "unknown-app".into());
            let token_root = workspace
                .parent()
                .unwrap()
                .join(".app-cli-state")
                .join(&application_id);
            std::fs::create_dir_all(&token_root).unwrap();
            std::fs::write(token_root.join("token"), "fixture-token").unwrap();
            let identity = shared_types::RuntimeIdentityView {
                application_id,
                service_family: "userapp-dev".into(),
                workspace_id: "workspace".into(),
                source_root: workspace.to_string_lossy().into(),
                runtime_instance_id: "original-instance".into(),
                deployment_generation_id: "generation".into(),
                protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
                capabilities: vec![],
            };
            let posts = Arc::new(Mutex::new(
                Vec::<shared_types::RuntimeOperationRequest>::new(),
            ));
            let committed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let post_sink = posts.clone();
            let post_committed = committed.clone();
            let get_committed = committed.clone();
            let owner = Router::new()
                .route("/v1/runtime/identity", axum::routing::get(move || { let identity = identity.clone(); async move { axum::Json(serde_json::json!({"data": identity})) } }))
                .route("/v1/runtime/operations", axum::routing::post(move |axum::Json(request): axum::Json<shared_types::RuntimeOperationRequest>| {
                    let posts = post_sink.clone(); let committed = post_committed.clone();
                    async move {
                        let mut posts = posts.lock().unwrap(); posts.push(request.clone());
                        if posts.len() == 1 {
                            committed.store(accepted_before_loss, std::sync::atomic::Ordering::SeqCst);
                            return (axum::http::StatusCode::OK, "lost-response".to_string());
                        }
                        committed.store(true, std::sync::atomic::Ordering::SeqCst);
                        (axum::http::StatusCode::ACCEPTED, serde_json::json!({"data": shared_types::RuntimeOperationAccepted {
                            operation_id: request.operation_id.clone(), state: shared_types::RuntimeOperationState::Accepted,
                            poll: format!("/v1/runtime/operations/{}", request.operation_id),
                        }}).to_string())
                    }
                }))
                .route("/v1/runtime/operations/{id}", axum::routing::get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                    let committed = get_committed.clone(); async move {
                        if !committed.load(std::sync::atomic::Ordering::SeqCst) { return (axum::http::StatusCode::NOT_FOUND, axum::Json(serde_json::json!({}))); }
                        (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"data": shared_types::RuntimeOperationView {
                            operation_id: id, kind: shared_types::RuntimeOperationKind::Restart, state: shared_types::RuntimeOperationState::Succeeded,
                            request_digest: "digest".into(), revision: 8, runtime_instance_id: "original-instance".into(), error_code: None, error_message: None, failure_detail: None,
                        }})))
                    }
                }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(async move {
                axum::serve(listener, owner).await.unwrap();
            });
            let request = shared_types::RuntimeOperationRequest {
                operation_id: "original-op".into(),
                expected_runtime_instance_id: "original-instance".into(),
                expected_revision: 7,
                workspace_id: "workspace".into(),
                kind: shared_types::RuntimeOperationKind::Restart,
                profile: shared_types::RunProfileInput::Source {
                    workspace_id: "workspace".into(),
                },
                run_config: None,
                request_context: Some("original-task".into()),
            };
            let digest: String = Sha256::digest(serde_json::to_vec(&request).unwrap())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let disk = serde_json::json!({
                "owners": {"userapp:app123": {"pid":0,"port":9080,"project_id":"userapp:app123","owner":{"address":address,"runtime_instance_id":"original-instance"}}},
                "stops": {}, "intents": {"userapp:app123|Restart": {"request":request,"digest":digest,"has_private_config":false,"address":address,"project_root":workspace.canonicalize().unwrap()}}
            });
            std::fs::write(
                config.log_base_dir.join("dev-server-external.json"),
                disk.to_string(),
            )
            .unwrap();
            let state = make_state(config.clone());
            assert_eq!(state.fs.dev_server.list_dev().unwrap().len(), 1);
            // Real outer coordinator must not accept another task or claim this registration is started.
            let error = spawn_dev_task(
                state.clone(),
                "app123",
                None,
                None,
                DevTaskAction::Start,
                crate::service::userapp::DevWorkspacePrecheck {
                    ws: workspace.clone(),
                    dev_source_mode: true,
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(error, AppError::Conflict(_)));
            assert!(error.to_string().contains("original-op"));
            assert!(posts.lock().unwrap().is_empty());
            let first = http_recover(state, "original-op", "app123").await;
            assert_eq!(first["success"], false);
            let restarted = make_state(config);
            let success = http_recover(restarted.clone(), "original-op", "app123").await;
            assert_eq!(success["success"], true, "{success}");
            assert_eq!(success["data"]["task_id"], "original-task");
            assert_eq!(success["data"]["operation"]["operation_id"], "original-op");
            assert_eq!(success["data"]["operation"]["state"], "succeeded");
            let replay = http_recover(restarted.clone(), "original-op", "app123").await;
            assert_eq!(replay["success"], true);
            assert_eq!(
                posts.lock().unwrap().len(),
                if accepted_before_loss { 1 } else { 2 }
            );
            for posted in posts.lock().unwrap().iter() {
                assert_eq!(
                    serde_json::to_value(posted).unwrap(),
                    serde_json::to_value(&request).unwrap()
                );
            }
            // Receipt replay is query-only after completion, even if owner history was pruned.
            committed.store(false, std::sync::atomic::Ordering::SeqCst);
            let missing_receipt = http_recover(restarted.clone(), "original-op", "app123").await;
            assert_eq!(missing_receipt["success"], false);
            assert_eq!(
                posts.lock().unwrap().len(),
                if accepted_before_loss { 1 } else { 2 }
            );
            let wrong_app = http_recover(restarted, "original-op", "anotherapp").await;
            assert_eq!(wrong_app["success"], false);
            server.abort();
        }
    }
}
