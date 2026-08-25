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
use tracing::{info, warn};

use crate::router::AppState;

use super::client;
use super::task::PublishTask;
use super::types::PublishEvent;

/// UserAppBuilder per-app PVC 默认大小(后续可提到 config.yml 的 user-app-builder.service 段)。
const DEFAULT_BUILDER_STORAGE_SIZE: &str = "10Gi";

/// build 等待结果(消费 agent-runner build SSE 终态事件得出)。
pub(super) enum BuildOutcome {
    Completed,
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
    let info = ensure_userapp_builder(state, project_id).await?;
    Ok(dev_file_server_addr(state, &info))
}

/// 确保 UserAppBuilder 开发容器存在（幂等）并返回容器信息。
///
/// 跨域公共入口：发布编排（build/publish）、文件转发层（`userapp_forward`）、
/// chat 开发对话、create-workspace 共用——注册表命中复用，miss 创建注册。
pub(crate) async fn ensure_userapp_builder(
    state: &AppState,
    app_id: &str,
) -> Result<ContainerBasicInfo> {
    match registered_builder(state, app_id) {
        Some(info) => Ok(info),
        None => create_builder_and_register(state, app_id).await,
    }
}

/// 探活自愈版 [`ensure_userapp_builder`]：注册命中后连容器 file-server 探活
/// （3s 超时），失败视为注册脏值（容器被外部删除）→ 清注册重建。
///
/// 供低频管理面调用（pod ensure/keepalive）：**先探活再返回**，防"注册表命中
/// 死容器"幻报就绪；热路径（转发/chat/publish）不适用——它们有自己的节流
/// 探活（forward 30s 正缓存）或按需自愈语义。
///
/// 返回 `(info, created)`——created 由本函数判定（探活失败重建/miss 创建=true，
/// 复用=false），调用方无需再读注册表推断。
pub(crate) async fn ensure_userapp_builder_probed(
    state: &AppState,
    app_id: &str,
) -> Result<(ContainerBasicInfo, bool)> {
    if let Some(info) = registered_builder(state, app_id) {
        let addr = dev_file_server_addr(state, &info);
        if probe_file_server(&addr).await {
            return Ok((info, false));
        }
        warn!(
            "[USERAPP_ENSURE] dev container probe failed (stale registry?), recreating: app_id={app_id}, addr={addr}"
        );
        // 就地清 container 字段而非 remove_project（保 PG project 行与会话映射）
        state.shutdown_sse_streams_for_project(app_id);
        if let Some(mut stale) = state.get_project(app_id).map(|p| (*p).clone()) {
            stale.set_container(None);
            if let Err(e) = state.insert_project(app_id.to_string(), Arc::new(stale)) {
                warn!("[USERAPP_ENSURE] clear stale container field failed: app_id={app_id}: {e}");
            }
        }
        let info = create_builder_and_register(state, app_id).await?;
        return Ok((info, true));
    }
    let info = create_builder_and_register(state, app_id).await?;
    Ok((info, true))
}

/// 开发容器 file-server 轻量探活（连接失败/非 2xx 均不可用）。
async fn probe_file_server(addr: &str) -> bool {
    crate::http_client::shared_client()
        .get(format!("{addr}/api/version"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// 开发容器 file-server 地址（`http://{host}:60000`）。
pub(crate) fn dev_file_server_addr(state: &AppState, info: &ContainerBasicInfo) -> String {
    file_server_addr(state, info)
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
/// 1800s，中间链路断连不罕见）时查任务快照（快照请求本身失败也纳入退避——同一次
/// 瞬断常同时打掉流与快照），已终态按快照收敛；**仍非终态则退避重订阅续上**（构建
/// 还在远端正常执行，一次断流就判死会把发布误杀为失败——远端白烧资源无人收割，
/// 用户看到失败立即重试还得在远端排队）。重订阅带 `from_seq`（最后收到的远端
/// seq+1）续传，只补断流窗口内错过的事件（缺省 0 的全量回放会把历史重复推给
/// 任务：前端日志整段重复、进度回跳）。5 次退避（2s..32s）耗尽仍非终态才 Err；
/// file-server 侧 1800s 超时保证任务终态必达，漏网由 stale 对账兜底收敛。
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
    // 远端事件游标（file-server ring 分配的 SSE id）：重订阅时 from_seq=last+1 续传。
    let mut last_seq: Option<u64> = None;
    let mut rx = client::subscribe_build_progress(addr, build_task_id, 0);
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
            Ok(item) => {
                let data = item.event;
                if let Some(seq) = item.seq {
                    last_seq = Some(last_seq.map_or(seq, |prev| prev.max(seq)));
                }
                // 终态判定直接 match 类型化事件(类型保证 release_id 存在)。
                let terminal_outcome: Option<BuildOutcome> = match &data {
                    BuildProgressEvent::Completed { .. } => Some(BuildOutcome::Completed),
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
                match recover_or_resubscribe(addr, build_task_id, task, &mut resubscribes, last_seq)
                    .await?
                {
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

/// 断流恢复：查 agent-runner 任务快照（失败纳入退避重试）。已终态(completed/
/// failed/cancelled)按快照映射；仍非终态 → 退避重订阅（5 档：2/4/8/16/32s；退避
/// 期间仍响应取消），重订阅带 `from_seq`（last_seq+1，None=0 全量——尚未收到过
/// 任何事件时的正确语义）。快照与重订阅共享同一退避预算，耗尽 → Err（真因透传）。
async fn recover_or_resubscribe(
    addr: &str,
    build_task_id: &str,
    task: &PublishTask,
    resubscribes: &mut usize,
    last_seq: Option<u64>,
) -> Result<StreamRecovery> {
    const RESUBSCRIBE_DELAYS_SECS: [u64; 5] = [2, 4, 8, 16, 32];
    loop {
        let snap = match client::get_build_snapshot(addr, build_task_id).await {
            Ok(snap) => snap,
            Err(e) => {
                // 快照与流常命中同一次网络瞬断——纳入退避重试而非直接判死
                if *resubscribes >= RESUBSCRIBE_DELAYS_SECS.len() {
                    return Err(anyhow!(
                        "build stream lost and snapshot unreachable after {} recovery \
                         attempts: {e:#}",
                        RESUBSCRIBE_DELAYS_SECS.len()
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
                    error = %e,
                    "build snapshot unreachable after stream loss, retrying after backoff"
                );
                if !backoff_respecting_cancel(addr, build_task_id, task, delay).await? {
                    return Ok(StreamRecovery::Terminal(BuildOutcome::Cancelled));
                }
                continue;
            }
        };
        let terminal = match snap.status.as_str() {
            "completed" => Some(BuildOutcome::Completed),
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
        if !backoff_respecting_cancel(addr, build_task_id, task, delay).await? {
            return Ok(StreamRecovery::Terminal(BuildOutcome::Cancelled));
        }
        return Ok(StreamRecovery::Resubscribed(
            client::subscribe_build_progress(
                addr,
                build_task_id,
                last_seq.map_or(0, |seq| seq + 1),
            ),
        ));
    }
}

/// 退避等待（响应取消）。返回 false = 期间收到取消（已尽力 cancel_build），
/// 调用方应收敛 Cancelled；true = 退避完成可继续。
async fn backoff_respecting_cancel(
    addr: &str,
    build_task_id: &str,
    task: &PublishTask,
    delay_secs: u64,
) -> Result<bool> {
    tokio::select! {
        biased;
        _ = task.cancellation_notified() => {
            if let Err(e) = client::cancel_build(addr, build_task_id).await {
                tracing::warn!(%e, "cancel_build during backoff failed");
            }
            Ok(false)
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => Ok(true),
    }
}

/// [`shared_types::UserappDevCleanup`] 实现：app 删除（purge）时回收 UserAppBuilder
/// 开发资源——per-app 开发容器 + per-app RWO PVC +（Docker 模式）宿主 bind 目录。
///
/// 经契约注入 app_manager（其 runtime 视图无 agent 能力）；幂等，失败 best-effort
/// 由调用方决定（purge 路径 warn 不阻断，下次收敛）。
pub struct UserappDevResourcesCleanup {
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
    /// 宿主注册表（purge 后同步摘除，防下一个请求 ensure 复活已删 app 的容器+PVC）。
    projects: Arc<crate::storage::ProjectStoreBackend>,
}

impl UserappDevResourcesCleanup {
    pub fn new(
        runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
        projects: Arc<crate::storage::ProjectStoreBackend>,
    ) -> Self {
        Self { runtime, projects }
    }
}

#[async_trait::async_trait]
impl shared_types::UserappDevCleanup for UserappDevResourcesCleanup {
    async fn cleanup(&self, app_id: &str) -> Result<(), String> {
        // 1. 停/删开发容器（幂等；失败不阻断 PVC 回收，K8s 下 PVC 有 Pod 引用时
        //    pvc-protection 会挂住删除——容器先删是 PVC 能删的前提）
        if let Err(e) = self
            .runtime
            .stop_container_by_identifier(app_id, &ServiceType::UserAppBuilder)
            .await
        {
            tracing::warn!(
                "[USERAPP_DEV_CLEANUP] stop dev container failed (continuing): app_id={app_id}: {e}"
            );
        } else {
            info!("[USERAPP_DEV_CLEANUP] dev container stopped: app_id={app_id}");
        }

        // 2. per-app 开发 PVC 回收（幂等；Docker 模式 trait no-op）
        self.runtime
            .destroy_workspace_pvc(app_id, &ServiceType::UserAppBuilder)
            .await
            .map_err(|e| format!("destroy dev PVC failed: {e}"))?;

        // 3. Docker 模式宿主 bind 目录清理（./userapp-workspace/{app_id}；
        //    resolve 失败=rcoder 容器无该挂载（如 K8s 模式），跳过即可）
        if let Ok(host_root) = docker_manager::resolve_container_path_to_host(std::path::Path::new(
            shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
        ))
        .await
        {
            let dev_dir = host_root.join(app_id);
            if dev_dir.exists() {
                tokio::fs::remove_dir_all(&dev_dir)
                    .await
                    .map_err(|e| format!("remove dev bind dir {}: {e}", dev_dir.display()))?;
                info!(
                    "[USERAPP_DEV_CLEANUP] dev bind dir removed: {}",
                    dev_dir.display()
                );
            }
        }

        // 4. 摘注册与探活缓存：purge 后注册表残留死 IP 会让下一个请求
        //    ensure→探活失败→重建（已删 app 的容器+PVC 复活）。
        //    durable：purge 期间并发 dev chat 的 durable insert 与本删除同走
        //    同步事务（消除"remove 入队→durable 提交→writer 重放删行"倒挂）
        if self.projects.remove_durable(app_id).await.is_some() {
            info!("[USERAPP_DEV_CLEANUP] dev registry entry removed: app_id={app_id}");
        }
        crate::userapp_forward::invalidate_probe_cache(app_id);

        info!("[USERAPP_DEV_CLEANUP] dev resources cleaned: app_id={app_id}");
        Ok(())
    }
}
