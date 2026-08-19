//! 与 agent-runner 的 build 交互:确保 builder 存在(缺则自动创建)+ 消费 build 进度 SSE + 断流恢复。
//!
//! 底层 HTTP(trigger/subscribe/cancel/get_snapshot/package_url)在 `super::client`;本模块在其上
//! 做"消费 SSE → 透传进度到 task → 终态收敛成 `BuildOutcome`;断流/错误时查快照恢复终态"。
//! 编排(何时触发、终态/取消/回滚收敛)在 `super::orchestrator`。

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use container_runtime_api::ContainerCreateParams;
// 存储契约 trait：state.projects（ProjectStoreBackend 枚举）上的方法经此解析
use shared_types::ProjectStore as _;
use shared_types::{
    AGENT_FILE_SERVER_PORT, BuildProgressEvent, ContainerBasicInfo, ProjectAndContainerInfo,
    ServiceType, build_backend_addr,
};
use tracing::info;

use crate::router::AppState;

use super::client;
use super::task::PublishTask;
use super::types::PublishEvent;

/// UserAppBuilder per-app PVC 默认大小(后续可提到 config.yml 的 user-app-builder.service 段)。
const DEFAULT_BUILDER_STORAGE_SIZE: &str = "10Gi";

/// build 等待结果(消费 agent-runner build SSE 终态事件得出)。
pub(super) enum BuildOutcome {
    Completed { release_id: String },
    Failed(String),
    Cancelled,
}

// agent-runner 的 build 进度事件类型 = `shared_types::BuildProgressEvent`(file-server 发送 ↔
// rcoder 接收共享)。终态判定直接 match 类型化变体,不再字符串键解析(消除漂移)。

/// 确保 UserAppBuilder 存在并解析为 file-server addr(`http://{host}:60000`)。
///
/// 注册表(state.projects)命中直接复用;未命中自动创建并注册——调用方(build/publish
/// orchestrator)一次调用即可,无需先 ensure 再 build。`create_container` 幂等
/// (PVC/headless svc/STS 均 ensure 语义,已存在的 STS 复用并等 Ready),因此
/// rcoder 重启后注册表(内存态)丢失时,首个 build/publish 会自愈重建注册。
pub(super) async fn ensure_agent_addr(state: &AppState, project_id: &str) -> Result<String> {
    let info = match registered_builder(state, project_id) {
        Some(info) => info,
        None => create_builder_and_register(state, project_id).await?,
    };
    Ok(file_server_addr(state, &info))
}

/// 纯解析:只查 state.projects,无副作用。
fn registered_builder(state: &AppState, project_id: &str) -> Option<ContainerBasicInfo> {
    state
        .projects
        .get(project_id)
        .and_then(|p| p.container_info())
}

/// 复用 `build_backend_addr`(K8s 自动走 `{container_name}-svc.{ns}.svc.{domain}`,Docker 走 container_ip)。
fn file_server_addr(state: &AppState, info: &ContainerBasicInfo) -> String {
    let host = build_backend_addr(
        &info.container_name,
        &info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    );
    format!("http://{host}:{AGENT_FILE_SERVER_PORT}")
}

/// 创建 UserAppBuilder(幂等)并注册进 state.projects,返回容器信息。
///
/// 直接调 `runtime.create_container`(UserAppBuilder → `create_agent_container`),
/// **不走 ComputerContainerManager**(避免 ComputerAgentRunner 专属的 lazy_migrate)。
async fn create_builder_and_register(
    state: &AppState,
    project_id: &str,
) -> Result<ContainerBasicInfo> {
    // UserAppBuilder identifier = project_id(app_id 兼任);host_workspace_path K8s 模式不用。
    let params = ContainerCreateParams::builder()
        .project_id(project_id.to_string())
        .user_id(project_id.to_string())
        .host_workspace_path("")
        .service_type(ServiceType::UserAppBuilder)
        .storage_size(DEFAULT_BUILDER_STORAGE_SIZE)
        .build();

    let container_info = state
        .runtime()
        .create_container(params)
        .await
        .context("ensure UserAppBuilder failed")?;

    // 注册到 state.projects(后续 build/publish 据 project_id 查 container_name/ip)。
    let project_info = if let Some(existing) = state.get_project(project_id) {
        let mut info = (*existing).clone();
        info.set_container(Some(container_info.clone()));
        info
    } else {
        let mut info = ProjectAndContainerInfo::new(project_id.to_string());
        info.set_service_type(Some(ServiceType::UserAppBuilder));
        info.set_container(Some(container_info.clone()));
        info
    };
    state
        .insert_project(project_id.to_string(), Arc::new(project_info))
        .context("register UserAppBuilder to projects failed")?;

    info!(
        "[USERAPP_PUBLISH] UserAppBuilder ensured: app_id={}, container={}, ip={}",
        project_id, container_info.container_name, container_info.container_ip
    );
    Ok(container_info)
}

/// 消费 agent-runner build SSE:透传进度到 task,终态返 BuildOutcome。
/// 期间检查 task.is_cancelled → cancel_build + Cancelled。
///
/// 断流容错：SSE 是进度流而非可靠性契约——网络瞬断/代理 idle 断连（build 可达
/// 1800s，中间链路断连不罕见）时查任务快照，已终态按快照收敛；**仍非终态则退避
/// 重订阅续上**（构建还在远端正常执行，一次断流就判死会把发布误杀为失败——远端
/// 白烧资源无人收割，用户看到失败立即重试还得在远端排队）。5 次退避（2s..32s，
/// 累计约 62s）耗尽仍非终态才 Err；file-server 侧 1800s 超时保证任务终态必达，
/// 由 orchestrator 的终态兜底路径（stale 对账）收敛。
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
    let mut resubscribes = 0usize;
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
                // 断流（通道关闭）与流级错误归一，统一走快照恢复/重订阅
                None => Err(client::BuildStreamError::Read("build stream ended".to_string())),
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
                // 流级错误(connect/非2xx/读取/缓冲/断流):log + 快照恢复(不再 log-then-close
                // 让 orchestrator 瞎,也不再单次断流就判死)。
                tracing::warn!(
                    task_id = %task.id,
                    remote_build_task_id = %build_task_id,
                    error = %e,
                    "agent-runner build stream lost, attempting snapshot recovery"
                );
                match recover_or_resubscribe(addr, build_task_id, task, &mut resubscribes).await? {
                    StreamRecovery::Terminal(outcome) => return Ok(outcome),
                    StreamRecovery::Resubscribed(new_rx) => rx = new_rx,
                }
            }
        }
    }
}

/// 断流后的恢复分支结果。
enum StreamRecovery {
    /// 快照已终态：按快照映射 outcome。
    Terminal(BuildOutcome),
    /// 仍非终态且重试有余额：退避后重订阅成功，续上进度流。
    Resubscribed(client::BuildProgressReceiver),
}

/// 断流恢复：查 agent-runner 任务快照。已终态(completed/failed/cancelled)按快照映射；
/// 仍非终态 → 退避重订阅（5 次：2/4/8/16/32s；退避期间仍响应取消）。快照不可达或
/// 重试耗尽 → Err（真因透传）。
async fn recover_or_resubscribe(
    addr: &str,
    build_task_id: &str,
    task: &PublishTask,
    resubscribes: &mut usize,
) -> Result<StreamRecovery> {
    const RESUBSCRIBE_DELAYS_SECS: [u64; 5] = [2, 4, 8, 16, 32];
    let snap = client::get_build_snapshot(addr, build_task_id)
        .await
        .context("snapshot recovery after build stream lost")?;
    let terminal = match snap.status.as_str() {
        "completed" => Some(BuildOutcome::Completed {
            release_id: snap
                .release_id
                .clone()
                .ok_or_else(|| anyhow!("snapshot completed but missing releaseId"))?,
        }),
        "failed" => Some(BuildOutcome::Failed(
            snap.error
                .clone()
                .unwrap_or_else(|| "build failed".to_string()),
        )),
        "cancelled" => Some(BuildOutcome::Cancelled),
        _non_terminal => None,
    };
    if let Some(outcome) = terminal {
        return Ok(StreamRecovery::Terminal(outcome));
    }
    if *resubscribes >= RESUBSCRIBE_DELAYS_SECS.len() {
        return Err(anyhow!(
            "build stream lost and task still non-terminal after {} resubscribe attempts \
             (last status={})",
            RESUBSCRIBE_DELAYS_SECS.len(),
            snap.status
        ));
    }
    let delay = RESUBSCRIBE_DELAYS_SECS[*resubscribes];
    *resubscribes += 1;
    tracing::warn!(
        task_id = %task.id,
        remote_build_task_id = %build_task_id,
        attempt = *resubscribes,
        total = RESUBSCRIBE_DELAYS_SECS.len(),
        delay_secs = delay,
        "build task still running, resubscribing after backoff"
    );
    // 退避期间仍响应取消（最长 32s 的窗口不该挡住用户取消）
    tokio::select! {
        biased;
        _ = task.cancellation_notified() => {
            if let Err(e) = client::cancel_build(addr, build_task_id).await {
                tracing::warn!(%e, "cancel_build during backoff failed");
            }
            return Ok(StreamRecovery::Terminal(BuildOutcome::Cancelled));
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(delay)) => {}
    }
    Ok(StreamRecovery::Resubscribed(
        client::subscribe_build_progress(addr, build_task_id),
    ))
}
