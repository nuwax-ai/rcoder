//! `/api/v1/userapp/dev/*`: UserApp 开发阶段的服务生命周期 + 开发编译（新契约, app_id 唯一 key）。
//!
//! 复用 file-server 的 DevServerManager（进程/端口池/探活/日志）与 build 流水线,
//! 进程 key 用 `userapp:{app_id}` 前缀与 web 项目的 projectId 空间隔离
//! （app_id≡project_id 同值时, web 项目 workspace 在 project 树 / UserApp 在开发卷,
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
    BuildTaskStatus, DevLogsQuery, DevOpBody, UserappDevList, UserappDevListQuery,
    UserappDevProcess, UserappDevStopped, UserappDevTaskCreated,
};
use file_server::error::AppError;
use file_server::extract::{AppJson as Json, AppQuery as Query};
use file_server::models::DevProcess;
use file_server::models::ReadDevLogResult;
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

// ── handlers ───────────────────────────────────────────────────────────────────

/// 编译并启动 dev 服务
///
/// 异步任务：编译可能数分钟，受理即返 task_id。manifest 同核编译（与生产构建同核，dev 编译通过=可部署）
/// 成功后启动 dev 服务（UserApp workspace = spawn app-cli 按 manifest
/// run.command 编排全栈，pingap 9080 统一入口）；编译失败任务终态 Failed、
/// 不启动。进度/结果：轮询 `GET /api/v1/userapp/tasks/{task_id}`、SSE
/// `/api/v1/userapp/tasks/{task_id}/logs/stream`；终态后端口经
/// `GET /api/v1/userapp/dev/list` 查询（UserApp workspace 恒为 pingap 9080）。
/// 入参 basePath 对 UserApp workspace（manifest/app-cli 引擎）**无效**
/// ——pingap 路由前缀由各服务 project.manifest.toml `[proxy].path` 决定。
#[utoipa::path(
    post,
    path = "/dev/start",
    request_body = DevOpBody,
    responses((status = 200, body = HttpResult<UserappDevTaskCreated>, description = "启动任务已创建（task_id）")),
    tag = "UserApp · dev · 进程管理"
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
    tag = "UserApp · dev · 进程管理"
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
/// agent 改完代码后的开发闭环——**重启前必须先编译**，新代码才生效；异步任务立即返 task_id，编译
/// 可能数分钟。manifest 同核编译成功后 stop + start（app-cli 重拉全栈）；
/// 编译失败任务终态 Failed、旧服务原样保留（可继续用旧版本测试，不因
/// 中间态断流）。进度/结果查询同 start。入参 basePath 对 UserApp
/// workspace 无效（同 start 的说明）。
#[utoipa::path(
    post,
    path = "/dev/restart",
    request_body = DevOpBody,
    responses((status = 200, body = HttpResult<UserappDevTaskCreated>, description = "重启任务已创建（task_id）")),
    tag = "UserApp · dev · 进程管理"
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
    match resolve_userapp_dev(app_id, None, &state.fs.config) {
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
            &state.fs.config,
            &state.fs.build_manager,
            &app_id,
            &release_id,
            state.fs.config.dev_command_timeout_secs,
            Some(&progress),
        )
        .await;
        let outcome = async {
            // Start 快速路径：服务已在跑 → 跳过编译直接完成（恢复旧同步
            // start 的廉价幂等——否则白编译数分钟且已运行进程不加载新代码；
            // 要上新代码用 restart）
            if matches!(action, DevTaskAction::Start)
                && state
                    .fs.dev_server
                    .list_dev()?
                    .iter()
                    .any(|p| p.project_id == key)
            {
                tracing::info!(%app_id, "dev already running; start task completes without rebuild");
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
            // 编译成功：按 action 启动/重启（app-cli 全栈新代码生效；失败整体
            // Failed——restart 场景旧服务已被 stop，语义上本轮失败，下次重试）
            match action {
                DevTaskAction::Start => {
                    state
                        .fs.dev_server
                        .start_dev(&key, &ws, base_path.as_deref())
                        .await?;
                }
                DevTaskAction::Restart => {
                    state
                        .fs.dev_server
                        .restart_dev(&key, &ws, base_path.as_deref())
                        .await?;
                }
            }
            Ok::<(), AppError>(())
        }
        .await;
        match outcome {
            Ok(()) => {
                task_clone
                    .emit(shared_types::BuildProgressEvent::Completed {
                        // dev 任务消费方按 status/端口（dev/list）取结果；制品字段
                        // 仍带真实值（同核编译产出制品 zip，artifact_path 可用于
                        // 手动取包校验）。快速路径（跳过编译）时制品不存在，
                        // 该路径仅是预期值。
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

/// 在跑的 UserApp 开发服务列表
///
/// 列出该 `app_id`（query 必填）在跑的 dev server 进程（pid/port/started_at）。
/// 不含 web/computer 项目进程——进程键按 app 维度前缀隔离，仅 UserApp 开发
/// 服务视角；`user_id` 为挂载压平契约字段（必填校验，不参与过滤）。
#[utoipa::path(
    get,
    path = "/dev/list",
    params(UserappDevListQuery),
    responses((status = 200, body = HttpResult<UserappDevList>, description = "该 app 在跑的 UserApp 开发服务列表（不含 web/computer 项目进程）")),
    tag = "UserApp · dev · 进程管理"
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

/// 开发服务日志
///
/// 读取该 `app_id` 的 dev server 进程日志并分页返回：`log_type=main` 当日
/// 汇总 / `temp` 最新一次运行（默认）；`start_index` 起始行分页（默认 1）。
#[utoipa::path(
    get,
    path = "/dev/logs",
    params(DevLogsQuery),
    responses((status = 200, body = HttpResult<ReadDevLogResult>, description = "开发服务日志分页")),
    tag = "UserApp · dev · 进程管理"
)]
pub(crate) async fn dev_logs(
    State(state): State<UserAppState>,
    Query(q): Query<DevLogsQuery>,
) -> UserAppReply<ReadDevLogResult> {
    tracing::debug!(app_id = %q.app_id, user_id = %q.user_id, "userapp dev logs");
    let result = state
        .fs
        .dev_server
        .read_dev_log(
            &dev_key(&q.app_id),
            q.start_index,
            q.log_type.as_deref().unwrap_or("temp"),
        )
        .await;
    reply(result)
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

    /// dev_list 只返回 UserApp 域进程并剥前缀（构造带混合 key 的 manager 快照不易,
    /// 此处锁定 app_id_of_key 过滤语义——与 dev_list 的 filter_map 同谓词）。
    #[test]
    fn list_filter_semantics() {
        let keys = ["userapp:app-1", "web-proj", "computer:u:c"];
        let userapp: Vec<&str> = keys.iter().filter_map(|k| app_id_of_key(k)).collect();
        assert_eq!(userapp, vec!["app-1"]);
    }
}
