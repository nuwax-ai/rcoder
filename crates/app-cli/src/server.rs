//! 常驻 server：状态机 + 编排主循环（`app-cli serve`）。
//!
//! 形态演进：legacy 直跑（无子命令）是"一次性编排进程"——无 lock 起不来、服务
//! 崩整组重启；server 形态把 app-cli 变成**常驻管理服务**：无论是否部署都在
//! （Idle 态照常应答探针，空容器不 CrashLoop），部署/编排是状态机的一个阶段，
//! 新部署请求可打断当前编排（热切换）。
//!
//! 状态机：
//! ```text
//!  Idle ──(env APP_DEPLOY_URL | /v1/deploy)──▶ Deploying ──▶ Orchestrating ──▶ Running
//!   ▲                                                                    │
//!   └──────────────────── 新部署请求（先停旧服务）◀───────────────────────┘
//!  任一阶段失败 → Failed（/ready 503 摘流、/health 200 不杀容器，可再次部署）
//! ```
//!
//! 探针语义（kubelet 契约）：`/health` 恒 200（进程活）；`/ready` = Idle 200
//! （基础设施就绪——PG/ttyd/dbx 由 supervisord 固定 program 自治）/ Running 跟随
//! bridge readiness / 其余 503（摘流不杀）。

use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::CliArgs;
use crate::log::service::LogLayout;
use crate::manifest::ReleaseLock;
use crate::runtime_status::RuntimeStatusService;
use crate::supervisor;
use crate::supervisord_host::SupervisordHost;

/// server 全局状态（api 层与主循环共享；读多写少，std RwLock 短临界区不跨 await）。
pub struct ServerState {
    phase: RwLock<ServerPhase>,
    release: RwLock<Option<ReleaseLock>>,
    ready: RuntimeStatusService,
    /// 最近一次部署的进度快照（/v1/deploy/status 消费）。
    deploy_status: RwLock<DeployStatus>,
    /// 热部署受理通道（api 端点 → 主循环）。
    deploy_tx: tokio::sync::mpsc::UnboundedSender<DeployRequest>,
    deploy_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<DeployRequest>>,
    cancel: CancellationToken,
    /// 日志布局（跟随服务托管引擎；serve 探测后设置，legacy 默认 Builtin）。
    log_layout: RwLock<LogLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerPhase {
    /// 未部署：基础设施（PG/ttyd/dbx）可用，等待首次部署。
    Idle,
    /// 下载/解压/校验/换 code 进行中（旧服务在下载成功前不受影响）。
    Deploying,
    /// 服务编排中（migrate → start → pingap → readiness）。
    Orchestrating,
    /// 服务运行中（supervise 阻塞）。
    Running,
    /// 最近一次部署/编排失败（现场保留，可再次部署）。
    Failed(String),
}

impl ServerPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServerPhase::Idle => "idle",
            ServerPhase::Deploying => "deploying",
            ServerPhase::Orchestrating => "orchestrating",
            ServerPhase::Running => "running",
            ServerPhase::Failed(_) => "failed",
        }
    }

    /// /ready 判定（Idle=基础设施就绪；Running=bridge readiness；其余摘流）。
    fn readiness_ok(&self, service_ready: bool) -> bool {
        match self {
            ServerPhase::Idle => true,
            ServerPhase::Running => service_ready,
            _ => false,
        }
    }

    /// 热部署可受理的相位（进行中拒绝，防双部署竞争）。
    fn accepts_deploy(&self) -> bool {
        !matches!(self, ServerPhase::Deploying | ServerPhase::Orchestrating)
    }
}

/// /v1/deploy 受理请求（api 端点反序列化后转发主循环）。
#[derive(Debug, Clone)]
pub struct DeployRequest {
    pub url: String,
    pub release_id: String,
    pub sha256: Option<String>,
}

/// 部署进度快照（/v1/deploy/status 响应体）。
#[derive(Debug, Clone, Default, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct DeployStatus {
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ServerState {
    pub fn new(ready: RuntimeStatusService) -> Self {
        let (deploy_tx, deploy_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            phase: RwLock::new(ServerPhase::Idle),
            release: RwLock::new(None),
            ready,
            deploy_status: RwLock::new(DeployStatus {
                phase: ServerPhase::Idle.as_str().to_string(),
                ..Default::default()
            }),
            deploy_tx,
            deploy_rx: tokio::sync::Mutex::new(deploy_rx),
            cancel: CancellationToken::new(),
            log_layout: RwLock::new(LogLayout::Builtin),
        }
    }

    pub fn phase(&self) -> ServerPhase {
        self.phase.read().expect("phase lock").clone()
    }

    pub fn set_phase(&self, phase: ServerPhase) {
        let mut guard = self.phase.write().expect("phase lock");
        *guard = phase.clone();
        drop(guard);
        let mut status = self.deploy_status.write().expect("deploy status lock");
        status.phase = phase.as_str().to_string();
        if let ServerPhase::Failed(err) = &phase {
            status.error = Some(err.clone());
        }
    }

    /// 当前 release（部署编排成功后置入；幂等恢复路径直接从 lock 文件读入）。
    pub fn release(&self) -> Option<ReleaseLock> {
        self.release.read().expect("release lock").clone()
    }

    pub fn set_release(&self, release: ReleaseLock) {
        let rid = release.release_id.clone();
        *self.release.write().expect("release lock") = Some(release);
        self.deploy_status
            .write()
            .expect("deploy status lock")
            .release_id = Some(rid);
    }

    /// 部署代（日志游标 boot_id 语义：换代后旧 cursor 失效重放）。
    pub(crate) fn boot_id(&self) -> String {
        self.release()
            .map(|r| r.release_id)
            .unwrap_or_else(|| "idle".to_string())
    }

    /// /ready 判定（api 探针 handler 消费）。
    pub(crate) fn readiness_ok(&self) -> bool {
        self.phase().readiness_ok(self.ready.is_ready())
    }

    /// 热部署受理（api 端点调用）：相位守卫 + 通知主循环。
    pub(crate) fn try_accept_deploy(&self, req: DeployRequest) -> Result<(), String> {
        if !self.phase().accepts_deploy() {
            return Err(format!(
                "deploy in progress (phase={}); retry after terminal",
                self.phase().as_str()
            ));
        }
        // 受理即切 Deploying：rcoder 轮询方依赖 /v1/deploy/status 区分新旧代——
        // 若保持 Running 直到主循环 pick up，受理后首次轮询会读到**旧代** running
        // 而误判成功（竞态窗口 = POLL_INTERVAL + 调度延迟，全链 e2e 热部署
        // 实测抓到：受理 200 但容器实际编排失败已转 failed）
        self.set_phase(ServerPhase::Deploying);
        self.deploy_tx
            .send(req)
            .map_err(|_| "server loop exited".to_string())
    }

    pub(crate) fn deploy_status(&self) -> DeployStatus {
        self.deploy_status
            .read()
            .expect("deploy status lock")
            .clone()
    }

    /// RuntimeStatusService 句柄（编排器 set_ready / api /ready 消费共享）。
    pub(crate) fn runtime_status(&self) -> RuntimeStatusService {
        self.ready.clone()
    }

    pub(crate) fn log_layout(&self) -> LogLayout {
        *self.log_layout.read().expect("log layout lock")
    }

    pub(crate) fn set_log_layout(&self, layout: LogLayout) {
        *self.log_layout.write().expect("log layout lock") = layout;
    }
}

/// serve 主入口：api 常驻 + 状态机主循环（阻塞至 SIGTERM）。
pub async fn serve(args: &CliArgs) -> Result<()> {
    let ready = RuntimeStatusService::default();
    let state = Arc::new(ServerState::new(ready.clone()));

    // 管理 API 常驻（含探针——Idle/Deploying 态即有人应答，取代 legacy 的
    // idle.rs / LivenessHold 端口托管）
    let api_state = state.clone();
    let api_addr = args.admin_addr.clone();
    let api_workspace = args.workspace.clone();
    let api_log_dir = args.log_dir.clone();
    let api_pingap_bin = args.pingap_bin.clone();
    let api_handle = tokio::spawn(async move {
        if let Err(error) = crate::api::serve(
            &api_addr,
            api_workspace,
            api_log_dir,
            api_pingap_bin,
            api_state,
        )
        .await
        {
            tracing::error!("app-cli server API failed: {error}");
        }
    });

    // 启动部署判定（换 Pod 模式）：env 三元组在位且 code 缺失 → 首次部署；
    // lock 已在（卷上既有部署——Pod 重建/marker 幂等命中）→ 直接编排恢复。
    let mut first_request: Option<InitialAction> = None;
    if crate::deploy::deploy_requested() {
        match crate::deploy::request_from_env() {
            Ok(req) => first_request = Some(InitialAction::Deploy(req)),
            Err(e) => {
                // env 契约违背（URL 在而 RELEASE_ID 缺等）= 配置错误：进 Failed 暴露
                //（legacy 形态此处 bail 非 0；server 常驻以状态上报取代进程退出）
                tracing::error!("server: deploy env contract violated: {e:#}");
                state.set_phase(ServerPhase::Failed(format!("deploy env: {e:#}")));
            }
        }
    } else if args.workspace.join("release.lock.toml").exists() {
        tracing::info!(
            "server: release.lock exists without deploy env — orchestrating existing release"
        );
        first_request = Some(InitialAction::Existing);
    }

    // 服务托管引擎探测：supervisord socket 可用（容器形态）→ 动态 program 托管
    //（per-service 隔离重启）；否则 builtin（裸跑/dev，与 legacy 同引擎）。
    let host = SupervisordHost::detect().await;
    state.set_log_layout(if host.is_some() {
        LogLayout::Supervisord
    } else {
        LogLayout::Builtin
    });

    server_loop(args, &state, host, first_request).await;
    state.cancel.cancel();
    api_handle.abort();
    Ok(())
}

/// 等待结果三态（两引擎共用）。
enum Next {
    /// 回外层等待（Failed/服务退出保持等待，可再部署）。
    Wait,
    Redeploy(InitialAction),
    Exit,
}

/// 状态机主循环：初始动作（env 部署 / 卷上既有版本直接编排 / 空容器挂 Idle）→
/// 编排 supervise；期间可被新部署请求打断（停旧服务 → 换 code → 重新编排）。
enum InitialAction {
    /// env/热部署触发：下载制品后编排。
    Deploy(DeployRequest),
    /// 卷上既有 release.lock（Pod 重建恢复）：跳过下载直接编排。
    Existing,
}

async fn server_loop(
    args: &CliArgs,
    state: &Arc<ServerState>,
    host: Option<SupervisordHost>,
    first: Option<InitialAction>,
) {
    let mut pending: Option<InitialAction> = first;
    loop {
        // 取下一个动作：有待处理的直接用，否则挂 Idle 等受理/信号
        let action = match pending.take() {
            Some(action) => action,
            None => {
                // Failed 不被 Idle 覆盖：保留失败痕迹（deploy_status.error）与
                // 摘流态（/ready 503），直到下一次部署请求进来
                if !matches!(state.phase(), ServerPhase::Failed(_)) {
                    state.set_phase(ServerPhase::Idle);
                }
                let mut rx = state.deploy_rx.lock().await;
                tokio::select! {
                    maybe = rx.recv() => match maybe {
                        Some(req) => InitialAction::Deploy(req),
                        None => return, // api 层全退（不可能，防御）
                    },
                    () = crate::supervisor::sigterm_watch() => return,
                }
            }
        };

        // ── Deploying：下载/解压/校验全部成功才动旧服务（失败旧服务零感知）──
        if let InitialAction::Deploy(request) = &action {
            state.set_phase(ServerPhase::Deploying);
            tracing::info!(
                "server: deploying release_id={} url={}",
                request.release_id,
                request.url
            );
            if let Err(e) = crate::deploy::deploy(
                &args.workspace,
                &request.url,
                &request.release_id,
                request.sha256.as_deref(),
            )
            .await
            {
                tracing::error!("server: deploy stage failed: {e:#}");
                state.set_phase(ServerPhase::Failed(format!("deploy: {e:#}")));
                continue;
            }
        }

        // ── Orchestrating：读 lock → 编排（migrate → services → pingap → readiness）──
        match crate::manifest::read_release_lock(&args.workspace) {
            Ok(release) => {
                state.set_release(release);
                state.set_phase(ServerPhase::Orchestrating);
            }
            Err(e) => {
                tracing::error!("server: read release lock after deploy: {e:#}");
                state.set_phase(ServerPhase::Failed(format!("release lock: {e:#}")));
                continue;
            }
        }

        // ── 引擎分派：supervisord 托管（编排完成即返回，服务由 supervisord
        // per-service 重启）与 builtin（编排+supervise 阻塞在同一 task）──
        let mut hot_rx = state.deploy_rx.lock().await;
        if let Some(host) = &host {
            let runtime_status = state.runtime_status();
            if let Err(e) = host
                .orchestrate(
                    args,
                    &state.release().expect("release set"),
                    &runtime_status,
                )
                .await
            {
                tracing::error!("server: orchestration failed: {e:#}");
                state.set_phase(ServerPhase::Failed(format!("orchestrate: {e:#}")));
                let _ = host.stop_all().await;
                continue;
            }
            state.set_phase(ServerPhase::Running);
            let next = tokio::select! {
                maybe = hot_rx.recv() => match maybe {
                    Some(next_req) => Next::Redeploy(InitialAction::Deploy(next_req)),
                    None => Next::Exit,
                },
                () = crate::supervisor::sigterm_watch() => Next::Exit,
            };
            match next {
                Next::Exit => {
                    let _ = host.stop_all().await;
                    return;
                }
                Next::Wait => {}
                Next::Redeploy(action) => {
                    let _ = host.stop_all().await;
                    pending = Some(action);
                }
            }
            continue;
        }

        // builtin：编排 supervise（可被下一次部署请求打断：cancel → 停服 → 回
        // Deploying）；编排完成进 supervise 时经 on_running 通知 → 相位切 Running。
        let cancel = state.cancel.child_token();
        let runtime_status = state.runtime_status();
        let (running_tx, mut running_rx) = tokio::sync::oneshot::channel::<()>();
        let mut sup = tokio::spawn(supervisor::run_with_cancel(
            args.clone(),
            runtime_status,
            cancel.clone(),
            Some(running_tx),
        ));
        // 先等编排就绪（Running）；就绪后递进一轮等终态/热部署/信号。
        let next = tokio::select! {
            result = &mut running_rx => {
                if result.is_ok() {
                    state.set_phase(ServerPhase::Running);
                }
                tokio::select! {
                    outcome = &mut sup => match outcome {
                        // 服务退出/信号/cancel 后 supervise 正常返回：内置引擎不自动重编排
                //（supervisord 引擎下服务崩溃由 supervisord per-service 重启，不走到这）
                Ok(Ok(())) => {
                    tracing::warn!("server: orchestration ended (service exit or signal)");
                    Next::Wait
                }
                Ok(Err(e)) => {
                    tracing::error!("server: orchestration failed: {e:#}");
                    state.set_phase(ServerPhase::Failed(format!("orchestrate: {e:#}")));
                    Next::Wait
                }
                Err(join) => {
                    tracing::error!("server: orchestration task panicked: {join}");
                    state.set_phase(ServerPhase::Failed("orchestrate panicked".into()));
                    Next::Wait
                }
            },
            maybe = hot_rx.recv() => match maybe {
                Some(next_req) => {
                    tracing::info!("server: hot deploy received, stopping current services");
                    cancel.cancel();
                    if let Err(join) = sup.await {
                        tracing::error!("server: orchestration task panicked during cancel: {join}");
                    }
                    Next::Redeploy(InitialAction::Deploy(next_req))
                }
                None => Next::Exit,
            },
                    () = crate::supervisor::sigterm_watch() => {
                        cancel.cancel();
                        let _ = sup.await;
                        Next::Exit
                    }
                }
            }
            maybe = hot_rx.recv() => match maybe {
                Some(next_req) => {
                    tracing::info!("server: hot deploy received, stopping current services");
                    cancel.cancel();
                    if let Err(join) = sup.await {
                        tracing::error!("server: orchestration task panicked during cancel: {join}");
                    }
                    Next::Redeploy(InitialAction::Deploy(next_req))
                }
                None => Next::Exit,
            },
            () = crate::supervisor::sigterm_watch() => {
                cancel.cancel();
                let _ = sup.await;
                Next::Exit
            },
        };
        match next {
            Next::Exit => return,
            Next::Wait => {}
            Next::Redeploy(action) => pending = Some(action),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ServerState {
        ServerState::new(RuntimeStatusService::default())
    }

    #[test]
    fn readiness_matrix_per_phase() {
        let st = state();
        // Idle：基础设施就绪（空容器可服务），与 runtime_ready 无关
        st.set_phase(ServerPhase::Idle);
        assert!(st.readiness_ok());
        // Running：跟随后端 bridge readiness
        st.set_phase(ServerPhase::Running);
        assert!(!st.readiness_ok(), "runtime not ready → 503");
        st.ready.set_ready(true);
        assert!(st.readiness_ok());
        // 过渡/失败态：摘流
        for phase in [ServerPhase::Deploying, ServerPhase::Orchestrating] {
            st.set_phase(phase);
            assert!(!st.readiness_ok());
        }
        st.set_phase(ServerPhase::Failed("x".into()));
        assert!(!st.readiness_ok());
    }

    #[tokio::test]
    async fn deploy_acceptance_guards_and_channel() {
        let st = state();
        st.set_phase(ServerPhase::Running);
        let req = DeployRequest {
            url: "http://x/p.zip".into(),
            release_id: "rel-1".into(),
            sha256: None,
        };
        assert!(st.try_accept_deploy(req.clone()).is_ok());
        // 进行中相位拒绝（防双部署竞争）
        st.set_phase(ServerPhase::Deploying);
        assert!(st.try_accept_deploy(req.clone()).is_err());
        st.set_phase(ServerPhase::Orchestrating);
        assert!(st.try_accept_deploy(req).is_err());
        // 受理的请求能被主循环收到
        st.set_phase(ServerPhase::Idle);
        st.try_accept_deploy(DeployRequest {
            url: "http://x/p2.zip".into(),
            release_id: "rel-2".into(),
            sha256: None,
        })
        .unwrap();
        // 受理按序到达（Running 期受理的 rel-1 排在前——主循环串行消费）
        let first = st.deploy_rx.lock().await.recv().await.unwrap();
        assert_eq!(first.release_id, "rel-1");
        let second = st.deploy_rx.lock().await.recv().await.unwrap();
        assert_eq!(second.release_id, "rel-2");
    }

    #[test]
    fn boot_id_tracks_release_generation() {
        let st = state();
        assert_eq!(st.boot_id(), "idle");
        let mk_release = |rid: &str| workspace_manifest::ReleaseLock {
            schema_version: 1,
            release_id: rid.into(),
            workspace_name: "ws".into(),
            pingap: workspace_manifest::LockedPingap {
                mode: workspace_manifest::PingapMode::Managed,
                config: None,
                version: "0.13.9".into(),
                commit: "abc".into(),
            },
            minimum_app_cli_version: "0.0.0".into(),
            runtime_image_digest: String::new(),
            services: Vec::new(),
            bridge_service: None,
        };
        st.set_release(mk_release("rel-gen-1"));
        assert_eq!(st.boot_id(), "rel-gen-1");
        st.set_release(mk_release("rel-gen-2"));
        assert_eq!(st.boot_id(), "rel-gen-2");
        // 部署进度快照携带当前 release_id
        assert_eq!(st.deploy_status().release_id.as_deref(), Some("rel-gen-2"));
    }
}
