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
use super::types::PublishStage;

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
        stage: PublishStage::EnsureBuilder,
    })
    .await;
    let addr = ensure_agent_addr(state, project_id).await?;
    task.emit(PublishEvent::Stage {
        stage: PublishStage::Build,
    })
    .await;
    // build 请求体 userId 必填（file-server 挂载压平契约字段）：优先 app 元数据
    // owner（handler 在 build 受理时已 record_dev_registration），查不到兜底 app_id。
    let build_user_id = state
        .app_service
        .get_app_owner(app_id)
        .await
        .filter(|uid| !uid.trim().is_empty())
        .unwrap_or_else(|| app_id.to_string());
    let build_task_id = client::trigger_build(&addr, app_id, &build_user_id).await?;
    task.set_remote_build(addr.clone(), build_task_id.clone())
        .await;
    match wait_build(&addr, &build_task_id, task).await? {
        BuildOutcome::Completed => {
            // 产物摘要回填（file-server build 快照是唯一真源：release_id/sha256/
            // size/file_name/artifact_path 由构建侧 hash_file 计算）：Java 轮询任务
            // 快照即可取包，不必从 SSE 事件捞。快照拉取失败不阻断终态（终态是构建
            // 成败的事实，摘要是附加数据）；但**字段不全时不得拼半截摘要**——
            // artifact_path 是 Java 取包 URL（/static/{app_id}/{artifactPath}）的
            // 关键依据，空串会拼出坏 URL，比无摘要更糟。Completed 任务缺字段属
            // 协议异常（远端版本不匹配/数据损坏），按摘要不可用降级 + error 留痕。
            match client::get_build_snapshot(&addr, &build_task_id).await {
                Ok(snap) => {
                    let release_id = snap.release_id.clone();
                    match (&snap.file_name, &snap.sha256, snap.size_bytes) {
                        (Some(file_name), Some(sha256), Some(size_bytes)) => {
                            // artifact_path 缺失 = 旧版 builder（产物落 workspace 根,
                            // STS 模板不自动更新——升级窗口内存量 builder 无此字段）：
                            // 回退 file_name 作相对路径，恰好是旧版的正确取包路径
                            // （/static/{app_id}/{file_name}）。非空串兜底——语义等价
                            // 旧协议，升级窗口不断链。
                            let artifact_path = snap
                                .artifact_path
                                .clone()
                                .filter(|p| !p.trim().is_empty())
                                .unwrap_or_else(|| file_name.clone());
                            task.set_artifact(super::types::ArtifactDigest {
                                file_name: file_name.clone(),
                                artifact_path,
                                sha256: sha256.clone(),
                                size_bytes,
                            })
                            .await;
                            task.emit(PublishEvent::Completed { release_id }).await;
                        }
                        _ => {
                            tracing::error!(
                                app_id,
                                task_id = %build_task_id,
                                file_name = ?snap.file_name,
                                sha256_present = snap.sha256.is_some(),
                                size_present = snap.size_bytes.is_some(),
                                "[USERAPP_BUILD] artifact digest incomplete on completed \
                                 build (protocol mismatch?); completing without digest"
                            );
                            task.emit(PublishEvent::Completed { release_id: None })
                                .await;
                        }
                    }
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
