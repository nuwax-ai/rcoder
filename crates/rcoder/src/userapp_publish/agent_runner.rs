//! 与 agent-runner 的 build 交互:解析地址 + 消费 build 进度 SSE + 断流恢复。
//!
//! 底层 HTTP(trigger/subscribe/cancel/get_snapshot/package_url)在 `super::client`;本模块在其上
//! 做"消费 SSE → 透传进度到 task → 终态收敛成 `BuildOutcome`;断流/错误时查快照恢复终态"。
//! 编排(何时触发、终态/取消/回滚收敛)在 `super::orchestrator`。

use anyhow::{Context, Result, anyhow};

// 存储契约 trait：state.projects（ProjectStoreBackend 枚举）上的方法经此解析
use shared_types::ProjectStore as _;
use shared_types::{AGENT_FILE_SERVER_PORT, BuildProgressEvent, build_backend_addr};

use crate::router::AppState;

use super::client;
use super::task::PublishTask;
use super::types::PublishEvent;

/// build 等待结果(消费 agent-runner build SSE 终态事件得出)。
pub(super) enum BuildOutcome {
    Completed { release_id: String },
    Failed(String),
    Cancelled,
}

// agent-runner 的 build 进度事件类型 = `shared_types::BuildProgressEvent`(file-server 发送 ↔
// rcoder 接收共享)。终态判定直接 match 类型化变体,不再字符串键解析(消除漂移)。

/// 解析 agent-runner project_id → file-server addr(`http://{host}:60000`)。
/// 复用 `build_backend_addr`(K8s 自动走 `{container_name}-svc.{ns}.svc.{domain}`,Docker 走 container_ip)。
pub(super) fn resolve_agent_addr(state: &AppState, project_id: &str) -> Result<String> {
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
    Ok(format!("http://{host}:{AGENT_FILE_SERVER_PORT}"))
}

/// 消费 agent-runner build SSE:透传进度到 task,终态返 BuildOutcome。
/// 期间检查 task.is_cancelled → cancel_build + Cancelled。
/// 流错误或断流(无终态)→ 查 task 快照收敛终态(不再吞成 "stream ended without terminal event")。
pub(super) async fn wait_build(
    addr: &str,
    build_task_id: &str,
    task: &PublishTask,
) -> Result<BuildOutcome> {
    if task.is_cancelled() {
        client::cancel_build(addr, build_task_id)
            .await
            .context("cancel agent-runner build before subscribing progress")?;
        return Ok(BuildOutcome::Cancelled);
    }
    let mut rx = client::subscribe_build_progress(addr, build_task_id);
    loop {
        let item = tokio::select! {
            biased;
            _ = task.cancellation_notified() => {
                client::cancel_build(addr, build_task_id)
                    .await
                    .context("cancel agent-runner build")?;
                return Ok(BuildOutcome::Cancelled);
            }
            item = rx.recv() => match item {
                Some(item) => item,
                None => return recover_outcome_from_snapshot(addr, build_task_id).await,
            },
        };
        match item {
            Ok(data) => {
                // 终态判定直接 match 类型化事件(类型保证 release_id 存在)。
                let terminal_outcome: Option<BuildOutcome> = match &data {
                    BuildProgressEvent::Completed { release_id, .. } => {
                        Some(BuildOutcome::Completed {
                            release_id: release_id.clone(),
                        })
                    }
                    BuildProgressEvent::Failed { error } => {
                        Some(BuildOutcome::Failed(error.clone()))
                    }
                    BuildProgressEvent::Cancelled => Some(BuildOutcome::Cancelled),
                    BuildProgressEvent::Stage { .. }
                    | BuildProgressEvent::Building { .. }
                    | BuildProgressEvent::BuildOk { .. }
                    | BuildProgressEvent::BuildFail { .. }
                    | BuildProgressEvent::Log { .. } => None,
                };
                if let Some(BuildOutcome::Failed(error)) = &terminal_outcome {
                    tracing::warn!(
                        task_id = %task.id,
                        remote_build_task_id = %build_task_id,
                        error = %error,
                        "agent-runner build reported failure"
                    );
                }
                task.emit(PublishEvent::BuildProgress { data }).await;
                if let Some(outcome) = terminal_outcome {
                    return Ok(outcome);
                }
            }
            Err(e) => {
                // 流级错误(connect/非2xx/读取/缓冲):log + snapshot 恢复(不再 log-then-close 让 orchestrator 瞎)。
                tracing::warn!(
                    task_id = %task.id,
                    remote_build_task_id = %build_task_id,
                    error = %e,
                    "agent-runner build stream error, attempting snapshot recovery"
                );
                return recover_outcome_from_snapshot(addr, build_task_id).await;
            }
        }
    }
}

/// 断流后查 agent-runner task 快照收敛终态。已终态(completed/failed/cancelled)按快照映射;
/// 仍非终态或快照不可达 → Err(真因透传,不再吞成 "stream ended without terminal event")。
async fn recover_outcome_from_snapshot(addr: &str, build_task_id: &str) -> Result<BuildOutcome> {
    let snap = client::get_build_snapshot(addr, build_task_id)
        .await
        .context("snapshot recovery after build stream ended")?;
    let status = snap.get("status").and_then(|s| s.as_str()).unwrap_or("");
    match status {
        "completed" => {
            let release_id = snap
                .get("releaseId")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("snapshot completed but missing releaseId"))?;
            Ok(BuildOutcome::Completed { release_id })
        }
        "failed" => Ok(BuildOutcome::Failed(
            snap.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("build failed")
                .to_owned(),
        )),
        "cancelled" => Ok(BuildOutcome::Cancelled),
        other => Err(anyhow!(
            "build stream ended and task still non-terminal (status={other})"
        )),
    }
}
