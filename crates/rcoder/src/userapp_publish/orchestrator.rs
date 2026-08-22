//! 构建编排(对外入口):`run_build`,顶层终态/取消收敛在此。
//!
//! 与 agent-runner 的 build SSE 消费见 `super::agent_runner`;与 app_manager 的生命周期
//! (ensure_app/wait_app_ready)见 `super::app_lifecycle`;底层 HTTP 在 `super::client`。
//! - `run_build`:仅触发 agent-runner build + 透传进度(独立 build 接口)。

use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::router::AppState;

use super::agent_runner::{BuildOutcome, ensure_agent_addr, wait_build};
use super::client;
use super::task::PublishTask;
use super::types::PublishEvent;

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
    // 取消快速失败（此前 create 后立即取消仍会先跑完
    // 可能数十秒的 builder ensure 才在 wait_build 开头响应）。
    fail_if_cancelled(task)?;
    // ensure builder:未注册时自动创建(K8s 拉镜像可能数十秒,先亮阶段让前端可见)。
    task.emit(PublishEvent::Stage {
        stage: "EnsureBuilder".to_string(),
    })
    .await;
    let addr = ensure_agent_addr(state, project_id).await?;
    task.emit(PublishEvent::Stage {
        stage: "Build".to_string(),
    })
    .await;
    let build_task_id = client::trigger_build(&addr, app_id).await?;
    task.set_remote_build(addr.clone(), build_task_id.clone())
        .await;
    match wait_build(&addr, &build_task_id, task).await? {
        BuildOutcome::Completed => {
            // 产物摘要回填（file-server build 快照是唯一真源：release_id/sha256/
            // size/file_name 由构建侧 hash_file 计算）：Java 轮询任务快照即可取包，
            // 不必从 SSE 事件捞。拉取失败不阻断终态（摘要缺失仅降低可观测性）。
            match client::get_build_snapshot(&addr, &build_task_id).await {
                Ok(snap) => {
                    let release_id = snap.release_id.clone();
                    task.set_artifact(super::types::ArtifactDigest {
                        file_name: snap.file_name.clone().unwrap_or_default(),
                        sha256: snap.sha256.clone().unwrap_or_default(),
                        size_bytes: snap.size_bytes.unwrap_or(0),
                    })
                    .await;
                    task.emit(PublishEvent::Completed { release_id }).await;
                }
                Err(e) => {
                    tracing::warn!(
                        "[USERAPP_BUILD] artifact digest backfill failed (completion unaffected): {e:#}"
                    );
                    task.emit(PublishEvent::Completed { release_id: None })
                        .await;
                }
            }
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

fn fail_if_cancelled(task: &PublishTask) -> Result<()> {
    if task.is_cancelled() {
        return Err(anyhow!("publish cancelled by user"));
    }
    Ok(())
}
