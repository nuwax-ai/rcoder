//! 发布/构建编排(对外入口):`run_build` / `run_publish`,顶层终态/取消收敛在此。
//!
//! 与 agent-runner 的 build SSE 消费见 `super::agent_runner`;与 app_manager 的生命周期
//! (ensure_app/wait_app_ready)见 `super::app_lifecycle`;底层 HTTP 在 `super::client`。
//! - `run_build`:仅触发 agent-runner build + 透传进度(独立 build 接口)。
//! - `run_publish`:全流程 build → ensure_app → prepare → activate → 轮询就绪 → confirm。

use std::sync::Arc;

use anyhow::{Result, anyhow};

use app_manager::models::PrepareReleaseRequest;

use crate::router::AppState;

use super::agent_runner::{BuildOutcome, ensure_agent_addr, wait_build};
use super::app_lifecycle::rcoder_app_id;
use super::client;
use super::task::PublishTask;
use super::types::PublishEvent;
use app_manager::models::ReleaseStatus;

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
    // 0. ensure builder:未注册时自动创建(K8s 拉镜像可能数十秒,先亮阶段让前端可见)。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "EnsureBuilder".to_string(),
    })
    .await;
    let addr = ensure_agent_addr(state, project_id).await?;

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

    // 3. activate(单接口:切流 + ensure 容器 + 等就绪 → Active/Failed)。
    //    就绪失败保留现场(不自动回滚),此处如实转任务 Failed——恢复由调用方显式
    //    rollback 或重新发布,现场留给用户排查。
    fail_if_cancelled(task)?;
    task.emit(PublishEvent::Stage {
        stage: "Activate".to_string(),
    })
    .await;
    match state
        .app_service
        .activate_release(&rcoder_app_id, &release_id, None)
        .await
    {
        Ok(release) if release.status == ReleaseStatus::Active => {}
        Ok(release) => {
            // Ok(Failed)=就绪失败(现场保留);failure_message 透传给任务
            task.emit(PublishEvent::Failed {
                error: release
                    .failure_message
                    .unwrap_or_else(|| "activation failed".to_string()),
            })
            .await;
            return Ok(()); // 已自 emit 终态,顶层见 Ok 跳过
        }
        Err(e) => return Err(anyhow!("activate_release: {e}")),
    }

    task.emit(PublishEvent::Completed {
        release_id: release_id.clone(),
    })
    .await;
    Ok(())
}

fn fail_if_cancelled(task: &PublishTask) -> Result<()> {
    if task.is_cancelled() {
        return Err(anyhow!("publish cancelled by user"));
    }
    Ok(())
}
