//! 发布/构建编排(对外入口):`run_build` / `run_publish`,顶层终态/取消收敛在此。
//!
//! 与 agent-runner 的 build SSE 消费见 `super::agent_runner`;与 app_manager 的生命周期
//! (ensure_app/wait_app_ready)见 `super::app_lifecycle`;底层 HTTP 在 `super::client`。
//! - `run_build`:仅触发 agent-runner build + 透传进度(独立 build 接口)。
//! - `run_publish`:全流程 build → ensure_app → prepare → activate → 轮询就绪 → confirm。

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

use app_manager::models::PrepareReleaseRequest;

use crate::router::AppState;

use super::agent_runner::{BuildOutcome, resolve_agent_addr, wait_build};
use super::app_lifecycle::{ensure_app, rcoder_app_id, wait_app_ready};
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
                // confirm(false) 自身失败时的兜底清 pending:abort_release 是 index-only CAS,
                // 不做文件/运行时操作,即便 confirm 失败也能成功,防止 pending_release_id
                // 永久残留导致 activate 守卫卡死后续所有发布。best-effort:失败仅记日志。
                if let Err(abort_error) = state
                    .app_service
                    .abort_release(&rcoder_app_id, &release_id, Some(combined.clone()))
                    .await
                {
                    tracing::error!(
                        task_id = %task.id,
                        app_id = %app_id,
                        error = %abort_error,
                        "best-effort abort_release failed; pending_release_id may remain stuck"
                    );
                }
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

fn fail_if_cancelled(task: &PublishTask) -> Result<()> {
    if task.is_cancelled() {
        return Err(anyhow!("publish cancelled by user"));
    }
    Ok(())
}
