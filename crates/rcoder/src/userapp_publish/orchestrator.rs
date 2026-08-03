//! 发布/构建编排:rcoder 正向调 agent-runner build(HTTP :60000 + 订阅进度 SSE)
//! → 同进程调 app_manager(prepare/activate/create_app/confirm)。
//!
//! - `run_build`:仅触发 agent-runner build + 透传进度(独立 build 接口)。
//! - `run_publish`:全流程 build → ensure_app → prepare → activate → 轮询就绪 → confirm。

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use app_manager::models::commons::{AppStatus, ExposeType, HealthCheckType};
use app_manager::models::{CreateAppRequest, HealthCheckConfig, PortConfig, PrepareReleaseRequest};
use shared_types::build_backend_addr;

use crate::router::AppState;

use super::client;
use super::task::{PublishEvent, PublishTask};

/// agent-runner 内嵌 file-server 端口(与 k8s_service.rs AGENT_FILE_SERVER_PORT 一致)。
const FILE_SERVER_PORT: u16 = 60_000;
/// app-runtime 容器公网端口(pingap 监听,对外 Service + PortConfig 用)。
const APP_HTTP_PORT: u16 = 9080;
/// app-cli 管理 API 端口(K8s 探针打这里:app-cli 自身提供 /health+/ready,不强依赖后端 app)。
const APP_CLI_ADMIN_PORT: u16 = 3010;
/// app-cli 提供的探针路径(liveness=进程活,readiness=初始化完成/可选桥接后端)。
const APP_LIVENESS_PATH: &str = "/health";
const APP_READINESS_PATH: &str = "/ready";
/// 就绪轮询间隔。
const READY_POLL_INTERVAL_SECS: u64 = 3;
/// 就绪轮询总超时(activate 后 app 启动 + 健康检查窗口)。
const APP_READY_TIMEOUT_SECS: u64 = 600;

/// build 等待结果(消费 agent-runner build SSE 终态事件得出)。
enum BuildOutcome {
    Completed { release_id: String },
    Failed(String),
    Cancelled,
}

/// agent-runner `BuildProgressEvent` 的 wire-level `event` 值。
///
/// `Unknown` 保留未来新增的非终态事件，rcoder 仍可原样透传给前端；缺失或空事件名则属于协议错误。
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentBuildEventKind {
    Stage,
    Building,
    BuildOk,
    BuildFail,
    Log,
    Completed,
    Failed,
    Cancelled,
    Unknown(String),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("agent-runner build event name must not be empty")]
struct ParseAgentBuildEventError;

impl FromStr for AgentBuildEventKind {
    type Err = ParseAgentBuildEventError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.trim().is_empty() {
            return Err(ParseAgentBuildEventError);
        }
        Ok(match value {
            "stage" => Self::Stage,
            "building" => Self::Building,
            "buildOk" => Self::BuildOk,
            "buildFail" => Self::BuildFail,
            "log" => Self::Log,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            unknown => Self::Unknown(unknown.to_owned()),
        })
    }
}

impl fmt::Display for AgentBuildEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Stage => "stage",
            Self::Building => "building",
            Self::BuildOk => "buildOk",
            Self::BuildFail => "buildFail",
            Self::Log => "log",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown(value) => value,
        };
        formatter.write_str(value)
    }
}

fn completed_release_id(data: &serde_json::Value) -> Result<String> {
    data.get("release_id")
        .and_then(|release_id| release_id.as_str())
        .filter(|release_id| !release_id.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("agent-runner completed build event missing non-empty release_id"))
}

fn failed_build_error(data: &serde_json::Value) -> Result<String> {
    data.get("error")
        .and_then(|error| error.as_str())
        .filter(|error| !error.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("agent-runner failed build event missing non-empty string 'error'"))
}

/// 独立 build 入口(spawn 调):触发 agent-runner build + 透传进度,终态 emit。
pub async fn run_build(
    task: Arc<PublishTask>,
    state: Arc<AppState>,
    project_id: String,
    app_id: String,
) {
    let result = run_build_inner(&task, &state, &project_id, &app_id).await;
    finalize_terminal(&task, &app_id, &project_id, result, "build").await;
}

/// 全流程发布入口(spawn 调):build → ensure_app → prepare → activate → 轮询 → confirm。
pub async fn run_publish(
    task: Arc<PublishTask>,
    state: Arc<AppState>,
    project_id: String,
    app_id: String,
) {
    let result = run_publish_inner(&task, &state, &project_id, &app_id).await;
    finalize_terminal(&task, &app_id, &project_id, result, "publish").await;
}

/// 顶层终态收敛:inner 返回 Err 时(尚未自 emit 终态),按是否取消区分 emit Cancelled/Failed。
/// 取消路径不再被旧的 `!is_terminal` 守卫吞掉(#6):取消+回滚成功 → Cancelled,失败/超时 → Failed。
async fn finalize_terminal(
    task: &PublishTask,
    app_id: &str,
    project_id: &str,
    result: Result<()>,
    label: &str,
) {
    let Err(error) = result else {
        return; // inner 已自 emit 终态(Cancelled/Failed/Completed)或正常完成
    };
    if task.is_cancelled() && !task.is_terminal().await {
        tracing::info!(
            task_id = %task.id,
            app_id = %app_id,
            project_id = %project_id,
            "UserApp {label} cancelled by user"
        );
        task.emit(PublishEvent::Cancelled).await;
    } else if !task.is_terminal().await {
        tracing::error!(
            task_id = %task.id,
            app_id = %app_id,
            project_id = %project_id,
            error = %error,
            "UserApp {label} orchestration failed"
        );
        task.emit(PublishEvent::Failed {
            error: error.to_string(),
        })
        .await;
    }
}

async fn run_build_inner(
    task: &PublishTask,
    state: &AppState,
    project_id: &str,
    app_id: &str,
) -> Result<()> {
    let addr = resolve_agent_addr(state, project_id)?;
    task.emit(PublishEvent::Stage {
        stage: "Build".to_string(),
    })
    .await;
    let build_task_id = client::trigger_build(&addr, app_id).await?;
    task.set_remote_build(addr.clone(), build_task_id.clone())
        .await;
    match wait_build(&addr, &build_task_id, task).await? {
        BuildOutcome::Completed { release_id } => {
            task.emit(PublishEvent::Completed { release_id }).await;
        }
        BuildOutcome::Failed(err) => {
            task.emit(PublishEvent::Failed { error: err }).await;
        }
        BuildOutcome::Cancelled => {
            task.emit(PublishEvent::Cancelled).await;
        }
    }
    Ok(())
}

async fn run_publish_inner(
    task: &PublishTask,
    state: &AppState,
    project_id: &str,
    app_id: &str,
) -> Result<()> {
    let addr = resolve_agent_addr(state, project_id)?;

    // 1. build(透传 agent-runner 进度;拿 release_id)。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "Build".to_string(),
    })
    .await;
    let build_task_id = client::trigger_build(&addr, app_id).await?;
    task.set_remote_build(addr.clone(), build_task_id.clone())
        .await;
    let release_id = match wait_build(&addr, &build_task_id, task).await? {
        BuildOutcome::Completed { release_id } => release_id,
        BuildOutcome::Failed(err) => {
            task.emit(PublishEvent::Failed { error: err }).await;
            return Ok(());
        }
        BuildOutcome::Cancelled => {
            task.emit(PublishEvent::Cancelled).await;
            return Ok(());
        }
    };
    // build 产物摘要(sha/size/file_name)从 agent-runner task 快照取(file-server build 完成写入)。
    let snap = client::get_build_snapshot(&addr, &build_task_id).await?;
    let sha256 = snap
        .get("sha256")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("build snapshot missing sha256"))?
        .to_string();
    let size_bytes = snap
        .get("sizeBytes")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("build snapshot missing sizeBytes"))?;
    let file_name = snap
        .get("fileName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("build snapshot missing fileName"))?
        .to_string();

    let rcoder_app_id = rcoder_app_id(app_id);
    let image = std::env::var("RCODER_RUNTIME_IMAGE_DIGEST")
        .context("RCODER_RUNTIME_IMAGE_DIGEST env not set (app-runtime image for create_app)")?;

    // 2. prepare(包 url 指向 agent-runner file-server,app_manager 据此下载校验)。
    //    prepare 自带 ensure_app_workspace_ready + ensure_release_dirs,无需先 create_app。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "Prepare".to_string(),
    })
    .await;
    let url = client::package_url(&addr, app_id, &file_name);
    state
        .app_service
        .prepare_release(
            &rcoder_app_id,
            PrepareReleaseRequest {
                release_id: release_id.clone(),
                url,
                sha256,
                size_bytes,
                retention: None,
            },
        )
        .await
        .map_err(|e| anyhow!("prepare_release: {e}"))?;

    // 3. activate(切 code 目录 + 重启 app-runtime 容器;新 app 无容器则只解压 code)。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "Activate".to_string(),
    })
    .await;
    state
        .app_service
        .activate_release(&rcoder_app_id, &release_id)
        .await
        .map_err(|e| anyhow!("activate_release: {e}"))?;

    // activate 之后的所有退出路径都必须收敛到 confirm。否则 readiness/取消失败
    // 会永久留下 pending_release_id，阻塞后续发布。
    let post_activate = async {
        // 4. ensure_app:create app 运行时容器(image=app-runtime,端口 9080,health ready 端点)。
        fail_if_cancelled(task)?;
        task.emit(PublishEvent::Stage {
            stage: "EnsureApp".to_string(),
        })
        .await;
        ensure_app(state, &rcoder_app_id, app_id, &image).await?;

        // 5. 轮询就绪:status=Running 且 health 非 Unhealthy。
        fail_if_cancelled(task)?;
        task.emit(PublishEvent::Stage {
            stage: "WaitReady".to_string(),
        })
        .await;
        wait_app_ready(state, &rcoder_app_id, task).await?;

        // 6. confirm healthy → Active。
        fail_if_cancelled(task)?;
        task.emit(PublishEvent::Stage {
            stage: "Confirm".to_string(),
        })
        .await;
        state
            .app_service
            .confirm_release(
                &rcoder_app_id,
                &release_id,
                true,
                Some("publish auto-confirm".to_string()),
            )
            .await
            .map_err(|e| anyhow!("confirm_release: {e}"))
    }
    .await;

    if let Err(error) = post_activate {
        let message = error.to_string();
        let cancelled = task.is_cancelled();
        match state
            .app_service
            .confirm_release(&rcoder_app_id, &release_id, false, Some(message.clone()))
            .await
        {
            Ok(_) => {
                // 回滚成功:取消 → 终态 Cancelled(在此 emit,顶层见 Ok 跳过);
                // 非取消失败 → 交顶层 finalize_terminal emit Failed("已回滚")。
                if cancelled {
                    task.emit(PublishEvent::Cancelled).await;
                    return Ok(());
                }
                return Err(anyhow!(
                    "publish failed after activation and was rolled back: {message}"
                ));
            }
            Err(rollback_error) => {
                // 回滚失败是真实故障:必须以 Failed 暴露,绝不能被顶层 !is_terminal 吞掉(#6)。
                let combined = format!(
                    "publish failed after activation: {message}; rollback also failed: {rollback_error}"
                );
                tracing::error!(
                    task_id = %task.id,
                    app_id = %app_id,
                    project_id = %project_id,
                    error = %combined,
                    "UserApp publish rollback failed"
                );
                task.emit(PublishEvent::Failed { error: combined }).await;
                return Ok(()); // 已自 emit 终态,顶层见 Ok 跳过
            }
        }
    }

    task.emit(PublishEvent::Completed {
        release_id: release_id.clone(),
    })
    .await;
    Ok(())
}

/// 解析 agent-runner project_id → file-server addr(`http://{host}:60000`)。
/// 复用 `build_backend_addr`(K8s 自动走 `{container_name}-svc.{ns}.svc.{domain}`,Docker 走 container_ip)。
fn resolve_agent_addr(state: &AppState, project_id: &str) -> Result<String> {
    let info = state
        .projects
        .get(project_id)
        .and_then(|p| p.container_info())
        .ok_or_else(|| anyhow!("agent-runner not found for project_id={project_id}"))?;
    let host = build_backend_addr(
        &info.container_name,
        &info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    );
    Ok(format!("http://{host}:{FILE_SERVER_PORT}"))
}

/// 消费 agent-runner build SSE:透传进度到 task,终态返 BuildOutcome。
/// 期间检查 task.is_cancelled → cancel_build + Cancelled。
async fn wait_build(addr: &str, build_task_id: &str, task: &PublishTask) -> Result<BuildOutcome> {
    if task.is_cancelled() {
        client::cancel_build(addr, build_task_id)
            .await
            .context("cancel agent-runner build before subscribing progress")?;
        return Ok(BuildOutcome::Cancelled);
    }
    let mut rx = client::subscribe_build_progress(addr, build_task_id);
    // 未知事件按 event 名去重警告一次,避免未来高频未知事件刷日志(P3)。
    let mut warned_unknown = std::collections::HashSet::<String>::new();
    loop {
        let data = tokio::select! {
            biased;
            _ = task.cancellation_notified() => {
                client::cancel_build(addr, build_task_id)
                    .await
                    .context("cancel agent-runner build")?;
                return Ok(BuildOutcome::Cancelled);
            }
            data = rx.recv() => match data {
                Some(data) => data,
                None => break,
            },
        };
        let event = data
            .get("event")
            .and_then(|e| e.as_str())
            .ok_or_else(|| anyhow!("agent-runner build event missing string field 'event'"))?
            .parse::<AgentBuildEventKind>()
            .context("parse agent-runner build event kind")?;

        // 在 data move 给前端事件前构造终态结果。Completed 的 struct-variant 字段仍为 snake_case。
        let terminal_outcome: Option<Result<BuildOutcome>> = match &event {
            AgentBuildEventKind::Completed => Some(
                completed_release_id(&data)
                    .map(|release_id| BuildOutcome::Completed { release_id }),
            ),
            AgentBuildEventKind::Failed => {
                Some(failed_build_error(&data).map(BuildOutcome::Failed))
            }
            AgentBuildEventKind::Cancelled => Some(Ok(BuildOutcome::Cancelled)),
            AgentBuildEventKind::Stage
            | AgentBuildEventKind::Building
            | AgentBuildEventKind::BuildOk
            | AgentBuildEventKind::BuildFail
            | AgentBuildEventKind::Log => None,
            AgentBuildEventKind::Unknown(event_name) => {
                if warned_unknown.insert(event_name.clone()) {
                    tracing::warn!(
                        task_id = %task.id,
                        remote_build_task_id = %build_task_id,
                        event = %event_name,
                        "received unknown agent-runner build event (warned once per event name)"
                    );
                }
                None
            }
        };

        // 透传(含终态事件,前端可见 build 完整进度)。
        task.emit(PublishEvent::BuildProgress { data }).await;
        if let Some(outcome) = terminal_outcome {
            if let Ok(BuildOutcome::Failed(error)) = &outcome {
                tracing::warn!(
                    task_id = %task.id,
                    remote_build_task_id = %build_task_id,
                    error = %error,
                    "agent-runner build reported failure"
                );
            }
            return outcome;
        }
    }
    Err(anyhow!(
        "agent-runner build stream ended without terminal event"
    ))
}

/// 确保 app 计算单元存在:不存在则 create_app(幂等;image/ports 首次设定后恒定)。
async fn ensure_app(state: &AppState, rcoder_app_id: &str, name: &str, image: &str) -> Result<()> {
    match state.app_service.get_app(rcoder_app_id).await {
        Ok(_) => {
            // app 已存在:image/ports/probes 首次设定后恒定,不自动 reconcile(#14)。
            // 注:app_service trait 只暴露运行时信息(AppRuntimeInfo,无 image 字段),无法在此
            // 直接比对存储镜像;改为记录期望 image,平台升级 app-runtime 后运维可据日志发现滞后。
            tracing::info!(
                app_id = %rcoder_app_id,
                desired_image = %image,
                "[USERAPP_PUBLISH] app already exists; image/ports/probes are constant after first \
                 create and will NOT be reconciled to the desired image"
            );
            return Ok(()); // 已存在
        }
        Err(e) if is_not_found(&e) => {} // 不存在 → create
        Err(e) => return Err(anyhow!("get_app: {e}")),
    }
    let req = CreateAppRequest {
        app_id: Some(rcoder_app_id.to_string()),
        name: name.to_string(),
        image: image.to_string(),
        command: None,
        env: None,
        secrets: None,
        resources: None,
        ports: Some(vec![PortConfig {
            name: "http".to_string(),
            port: APP_HTTP_PORT,
            expose_type: ExposeType::Http,
            strip_prefix: None,
        }]),
        // 探针打 app-cli 的 3010 管理 API(非 pingap 9080):app-cli 自身提供 /health(liveness,
        // 进程活,后端有 bug 也不杀容器)+ /ready(readiness,默认 app-cli 就绪/可选桥接后端)。
        // 不再硬编码 /api/rust/ready(旧 bug:与实际后端语言无关,且强依赖后端实现该路径)。
        health_check: Some(HealthCheckConfig {
            check_type: HealthCheckType::Http,
            path: Some(APP_READINESS_PATH.to_string()),
            liveness_path: Some(APP_LIVENESS_PATH.to_string()),
            port: Some(APP_CLI_ADMIN_PORT),
        }),
        tenant_id: None,
        space_id: None,
        // 发布编排创建的 UserApp 默认参与闲置回收（= 免费用户语义）；如需付费常驻由调用方另行 update。
        recycle_enabled: None,
        idle_timeout_seconds: None,
    };
    state
        .app_service
        .create_app(req)
        .await
        .map_err(|e| anyhow!("create_app: {e}"))?;
    Ok(())
}

/// 轮询 app 到 status=Running 且 health 非 Unhealthy;超时或进入 Error 则失败。
async fn wait_app_ready(state: &AppState, rcoder_app_id: &str, task: &PublishTask) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(APP_READY_TIMEOUT_SECS);
    loop {
        if task.is_cancelled() {
            return Err(anyhow!("publish cancelled by user"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "app readiness poll timed out after {APP_READY_TIMEOUT_SECS}s"
            ));
        }
        let info = state
            .app_service
            .get_app(rcoder_app_id)
            .await
            .map_err(|e| anyhow!("get_app poll: {e}"))?;
        if info.status == AppStatus::Error {
            return Err(anyhow!(
                "app entered Error state (health={})",
                info.health.status
            ));
        }
        if info.status == AppStatus::Running && info.health.status != "Unhealthy" {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(READY_POLL_INTERVAL_SECS)).await;
    }
}

fn fail_if_cancelled(task: &PublishTask) -> Result<()> {
    if task.is_cancelled() {
        return Err(anyhow!("publish cancelled by user"));
    }
    Ok(())
}

/// app_manager 错误是否 "app 不存在"(get_app 判存性用)。
fn is_not_found(e: &app_manager::error::AppOperationError) -> bool {
    matches!(e, app_manager::error::AppOperationError::NotFound(_))
}

/// file-server project_id → rcoder app_id(强制 `app-` 前缀,已带则原样)。
fn rcoder_app_id(app_id: &str) -> String {
    if app_id.starts_with("app-") {
        app_id.to_string()
    } else {
        format!("app-{app_id}")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AgentBuildEventKind, completed_release_id, failed_build_error, rcoder_app_id};

    #[test]
    fn agent_build_event_kind_round_trips_known_and_unknown_values() {
        for value in [
            "stage",
            "building",
            "buildOk",
            "buildFail",
            "log",
            "completed",
            "failed",
            "cancelled",
            "futureEvent",
        ] {
            let event = value
                .parse::<AgentBuildEventKind>()
                .expect("non-empty event name");
            assert_eq!(event.to_string(), value);
        }
        assert!("".parse::<AgentBuildEventKind>().is_err());
        assert!("  ".parse::<AgentBuildEventKind>().is_err());
    }

    #[test]
    fn completed_event_requires_non_empty_release_id() {
        assert_eq!(
            completed_release_id(&json!({"release_id": "release-a"})).expect("valid release id"),
            "release-a"
        );
        for invalid in [
            json!({}),
            json!({"release_id": null}),
            json!({"release_id": "  "}),
        ] {
            let error = completed_release_id(&invalid).expect_err("release id is required");
            assert!(error.to_string().contains("missing non-empty release_id"));
        }
    }

    #[test]
    fn failed_event_requires_non_empty_error_message() {
        assert_eq!(
            failed_build_error(&json!({"error": "compile failed"})).expect("valid build error"),
            "compile failed"
        );
        for invalid in [
            json!({}),
            json!({"error": null}),
            json!({"error": 42}),
            json!({"error": "  "}),
        ] {
            let error = failed_build_error(&invalid).expect_err("error string is required");
            assert!(
                error
                    .to_string()
                    .contains("missing non-empty string 'error'")
            );
        }
    }

    #[test]
    fn rcoder_app_id_prepends_prefix_when_missing() {
        assert_eq!(rcoder_app_id("userapp-e2e"), "app-userapp-e2e");
    }

    #[test]
    fn rcoder_app_id_is_idempotent_when_prefixed() {
        assert_eq!(rcoder_app_id("app-userapp-e2e"), "app-userapp-e2e");
    }
}
