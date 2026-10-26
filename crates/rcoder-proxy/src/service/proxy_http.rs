//! `PortProxy` 的 `ProxyHttp` trait 实现 —— 请求生命周期（request_filter → upstream → response）。
//!
//! 从 `service/mod.rs` 拆出，使 mod.rs 聚焦 struct 定义 + 构造（new/builders/create_pingora_proxy）。
//! 子模块可直接访问 `PortProxy` 的私有字段（隐私规则：子模块可见祖先模块的私有项）。

use crate::service::dispatch::DispatchRequest;
use async_trait::async_trait;
use pingora_core::protocols::Digest;
use pingora_core::upstreams::peer::{ALPN, HttpPeer};
use pingora_core::{ErrorType, Result as PingoraResult};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use shared_types::RemoteWakeState;
use std::time::Duration;
use tracing::{debug, warn};

use crate::router::RouteType;

use super::{AppConnectRecovery, PortProxy, TrackingCtx, utils};

const CONNECT_RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(250);
const CONNECT_RECOVERY_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
/// dev 等待期重解析切片：每片等待后重新权威解析一次地址（Pod 换 IP 跟进）。
const DEV_RE_RESOLVE_SLICE: Duration = Duration::from_secs(1);

/// Probe only after Pingora's real first connection failed. This uses the same
/// destination as the request but sends no HTTP bytes; Pingora retries the
/// original connection after the Service path becomes reachable.
async fn wait_for_peer_connection(peer: &HttpPeer, deadline: tokio::time::Instant) -> bool {
    let Some(address) = peer._address.as_inet().copied() else {
        return false;
    };
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let attempt_deadline = (now + CONNECT_RECOVERY_ATTEMPT_TIMEOUT).min(deadline);
        if matches!(
            tokio::time::timeout_at(attempt_deadline, tokio::net::TcpStream::connect(address))
                .await,
            Ok(Ok(_))
        ) {
            return true;
        }
        tokio::time::sleep(
            CONNECT_RECOVERY_RETRY_DELAY
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}

impl PortProxy {
    fn validate_request_api_key(&self, session: &Session, path: &str) -> PingoraResult<()> {
        let Some(config) = self.api_key_config.as_ref() else {
            return Ok(());
        };
        let key = session
            .req_header()
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok());
        shared_types::ApiKeyValidator::validate(config, path, key).map_err(|error| {
            let (status, detail) = match error {
                shared_types::ApiKeyAuthError::Invalid => (401, "Invalid API key"),
                shared_types::ApiKeyAuthError::Missing => (401, "Missing x-api-key header"),
                shared_types::ApiKeyAuthError::ConfigError => (500, "Internal configuration error"),
            };
            warn!(%path, status, "[PINGORA_AUTH] {detail}");
            pingora_core::Error::new(ErrorType::HTTPStatus(status)).more_context(detail.to_string())
        })
    }

    /// An address can disappear after the initial routing probe. Only a fresh,
    /// successful absence observation may turn a failed application dispatch
    /// into deployment recovery; DNS/API/transport failures are not absence.
    async fn restore_missing_app_after_dispatch(
        &self,
        app_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<bool, shared_types::WakeFailure> {
        let Some(control) = self.wake_control.as_ref() else {
            return Ok(false);
        };
        if !matches!(
            tokio::time::timeout_at(deadline, control.remote_wake_state_fresh(app_id)).await,
            Ok(RemoteWakeState::Missing)
        ) {
            return Ok(false);
        }
        match tokio::time::timeout_at(deadline, control.ensure_serving(app_id)).await {
            Ok(shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning) => {
                Ok(true)
            }
            Ok(
                shared_types::WakeOutcome::Failed(failure)
                | shared_types::WakeOutcome::Timeout(failure),
            ) => Err(failure),
            Ok(shared_types::WakeOutcome::Blocked { message, blocker }) => {
                Err(shared_types::WakeFailure {
                    blocker: Some(Box::new(blocker)),
                    ..shared_types::WakeFailure::new(
                        shared_types::ERR_CONFLICT,
                        "wake_admission",
                        message,
                    )
                })
            }
            Err(_) => Err(shared_types::WakeFailure::timeout("wake_wait", None, false)),
        }
    }

    /// Fresh diagnostics stay inside the original connection deadline. Each
    /// failed attempt replaces earlier evidence, including an absent observation.
    async fn collect_connection_evidence(
        &self,
        ctx: &mut TrackingCtx,
        app_id: &str,
        stage: &str,
        deadline: tokio::time::Instant,
    ) {
        ctx.fresh_failure_evidence = true;
        if ctx.userapp_route.is_none() {
            ctx.failure_hint = None;
            ctx.failure_hint_at = None;
            return;
        }
        let budget = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(Duration::from_millis(1500));
        if budget.is_zero() {
            // Deadline exhaustion supplies no contradictory fact. Preserve only
            // this request's recent verified snapshot, never the advisor cache.
            if ctx
                .failure_hint_at
                .is_none_or(|observed| observed.elapsed() > Duration::from_secs(2))
            {
                ctx.failure_hint = None;
                ctx.failure_hint_at = None;
            }
            return;
        }
        ctx.failure_hint = None;
        ctx.failure_hint_at = None;
        let Some(advisor) = self.failure_advisor_slot.load_full() else {
            return;
        };
        ctx.failure_hint =
            tokio::time::timeout(budget, advisor.advise_current(app_id, stage, budget))
                .await
                .ok()
                .flatten();
        ctx.failure_hint_at = ctx
            .failure_hint
            .as_ref()
            .map(|_| tokio::time::Instant::now());
        if let Some(state) = ctx.app_connect_recovery.as_mut() {
            state.dev_evidence_status = ctx
                .failure_hint
                .as_ref()
                .and_then(|hint| hint.readiness_status);
        }
    }

    pub(crate) fn capture_proxy_transition(ctx: &mut TrackingCtx) {
        if ctx.wake_failure.is_some() || ctx.userapp_route.is_none() {
            return;
        }
        if ctx
            .failure_hint_at
            .is_none_or(|observed| observed.elapsed() > Duration::from_secs(2))
        {
            ctx.failure_hint = None;
            return;
        }
        let Some(operation_id) = ctx
            .failure_hint
            .as_ref()
            .and_then(|hint| hint.transition_operation_id())
        else {
            return;
        };
        let mut failure = shared_types::WakeFailure::new(
            shared_types::ERR_OPERATION_IN_PROGRESS,
            "proxy_transition",
            "Application service is starting or restarting",
        );
        failure.operation_id = Some(operation_id.to_string());
        failure.retryable = true;
        ctx.wake_failure = Some(failure);
    }

    /// dispatch + 恢复预算约束（T4a）：存在恢复上下文时，定位/ensure 的
    /// dispatch 以 deadline 为上限——超时不取消已受理的独立创建工作
    /// （builder 租约自有生命周期），本请求按恢复终局诚实失败
    /// （503+Retry-After，经 unavailable_response 走 wrapper 终局分支）。
    async fn dispatch_peer_with_recovery_deadline(
        &self,
        route: RouteType,
        params: matchit::Params<'_, '_>,
        ctx: &mut TrackingCtx,
    ) -> PingoraResult<Box<HttpPeer>> {
        let deadline = ctx
            .app_connect_recovery
            .as_ref()
            .map(|state| state.deadline);
        let dispatch = self.dispatch_upstream_peer(route, params, ctx);
        match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, dispatch).await {
                Ok(result) => result,
                Err(_) => {
                    warn!("upstream dispatch exceeded connection recovery deadline");
                    if let Some(state) = ctx.app_connect_recovery.as_mut() {
                        state.unavailable_response = true;
                    }
                    Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)))
                }
            },
            None => dispatch.await,
        }
    }

    /// 恢复预算对 peer 建连超时的收紧：返回 false 表示预算已尽（调用方走
    /// 恢复终局）。首次 dispatch 与重解析后的每个 peer 都须收紧——重解析
    /// 拿到的 peer 带默认超时（10s 建连），不收紧则最终拨号可超出 deadline。
    fn clamp_peer_to_recovery_deadline(
        peer: &mut HttpPeer,
        deadline: tokio::time::Instant,
    ) -> bool {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        peer.options.connection_timeout = Some(
            peer.options
                .connection_timeout
                .unwrap_or(remaining)
                .min(remaining)
                .min(CONNECT_RECOVERY_ATTEMPT_TIMEOUT),
        );
        peer.options.total_connection_timeout = Some(
            peer.options
                .total_connection_timeout
                .unwrap_or(remaining)
                .min(remaining)
                .min(CONNECT_RECOVERY_ATTEMPT_TIMEOUT),
        );
        true
    }
}

#[async_trait]
impl ProxyHttp for PortProxy {
    type CTX = TrackingCtx;

    fn new_ctx(&self) -> Self::CTX {
        TrackingCtx::new()
    }

    /// 请求过滤阶段：Userapp 访问追踪 + 流量唤醒。
    ///
    /// 两类 prod 路由触发唤醒（stopped/starting app → `ensure_running` hold-and-wait ≤60s
    /// 拉起；超时/失败 → 503+Retry-After）：
    /// - `/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/...` 应用业务流量：
    ///   **touch + wake**（touch 记录最近访问，是闲置回收的信号源）；
    /// - `/api/v1/userapp/proxy/{ttyd,dbx}/prod/{user_id}/{app_id}` 工具族：
    ///   **只 wake 不 touch**（终端/DB 客户端连接不算业务活跃——挂终端不阻止
    ///   闲置回收，回收后下次连接自动唤醒再转发，ttyd 前端自动重连兜底首连窗口）。
    /// 其余路由直接放行（Ok(false) → 继续 upstream_peer）。
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        // Custom Page 预览转发内部入口：令牌校验（缺失/不符 → 404，不暴露端点
        // 存在性）+ 实例身份校验（不匹配 → 410 触发转发方缓存失效重解析；
        // 登记缺失（对账中）→ 503）。校验通过记 ctx（后续阶段免重复校验）。
        // 在 dbx/wake 之前——内部入口无唤醒语义，且必须先挡住外部探测。
        if session
            .req_header()
            .uri
            .path()
            .starts_with("/internal/preview-forward")
        {
            return Self::preview_forward_gate(session, ctx, &self.preview_slot).await;
        }

        // Admission must precede touch, wake, dev ensure and prod reconstruction.
        // Internal preview forwarding above keeps its separate token contract.
        self.validate_request_api_key(session, session.req_header().uri.path())?;

        // dbx 入口尾斜杠规范化（先于唤醒判定——重定向后的二次请求才进入 wake，
        // stopped app 不在无子路径请求上空等一轮）。
        let raw_uri = session.req_header().uri.clone();
        if let Some(location) =
            dbx_root_redirect_location(&self.router, raw_uri.path(), raw_uri.query())
        {
            let mut resp = ResponseHeader::build(307, None)?;
            resp.insert_header("Location", location)?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true); // 已直接响应，跳过 upstream
        }

        // 前缀快滤，避免每请求都走 matchit 树匹配（其余路由 /proxy/{port}、
        // /web/ttyd、/computer/vnc、/api/* 等直接放行；dev 流量无闲置回收语义，不触发）
        let path = Self::normalize_path(session.req_header().uri.path());
        // UserApp app 路由上下文（URI 改写前捕获）：错误页/来源识别只覆盖该域；
        // dev 路由无唤醒语义但同样需要失败呈现。
        if path.starts_with("/api/v1/userapp/proxy/app/")
            && let Ok(matched) = self.router.at(path)
        {
            let stage = match matched.value {
                RouteType::ProdAppProxy => Some("prod"),
                RouteType::DevAppProxy => Some("dev"),
                _ => None,
            };
            if let Some(stage) = stage
                && let Some(app_id) = matched.params.get("app_id")
            {
                ctx.userapp_route = Some(crate::service::types::UserAppRouteCtx {
                    app_id: app_id.to_string(),
                    stage,
                });
            }
        }
        if let Some((app_id, touch, stage)) = classify_wake_target(&self.router, path) {
            // ① 访问追踪（仅 prod 业务流量）
            if touch && let Some(ref tracker) = self.access_tracker {
                let _ = tracker.touch(&app_id).await;
            }
            if stage == crate::service::types::ConnectRecoveryStage::Dev {
                // ② dev 连接恢复：独立装配，不依赖 wake_control（dev 无唤醒
                //    语义——流量只 ensure 容器，服务编排由显式操作驱动）。
                //    拒连/超时在 deadline 内重试并等待端口就绪；重试路径仅
                //    重解析地址（find_dev_container 权威定位），不做 wake 检查。
                //    deadline 为入口等待总预算（默认 15s，T4 起自请求受理起算
                //    并纳入配置）。
                ctx.app_connect_recovery = Some(AppConnectRecovery {
                    app_id,
                    stage,
                    deadline: tokio::time::Instant::now() + self.dev_entry_wait(),
                    retry_requested: false,
                    runtime_checked: true,
                    unavailable_response: false,
                    dev_evidence_status: None,
                });
                return Ok(false);
            }
            // ② 流量唤醒（stopped/starting app 触发；手动 stop 与闲置回收统一——
            //    有请求即唤醒，见 AppWakeControl::ensure_running 语义）。
            //    is_stopped 为内存视图：多副本下其他副本 stop 后本副本不知情，
            //    remote_wake_pending 兜底查集群真实状态（TTL 缓存节流）。
            if let Some(ref wc) = self.wake_control {
                let deadline = tokio::time::Instant::now() + wc.wake_timeout();
                ctx.app_connect_recovery = Some(AppConnectRecovery {
                    app_id: app_id.clone(),
                    stage,
                    deadline,
                    retry_requested: false,
                    runtime_checked: false,
                    unavailable_response: false,
                    dev_evidence_status: None,
                });
                let wake_pending = if wc.is_stopped(&app_id) {
                    true
                } else {
                    let probe = async {
                        if touch {
                            wc.remote_serving_pending_result(&app_id).await
                        } else {
                            wc.remote_wake_pending_result(&app_id).await
                        }
                    };
                    match tokio::time::timeout_at(deadline, probe).await {
                        Ok(Ok(pending)) => pending,
                        Ok(Err(failure)) => {
                            self.respond_wake_failure(session, ctx, failure).await;
                            return Ok(true);
                        }
                        Err(_) => {
                            self.respond_wake_failure(
                                session,
                                ctx,
                                shared_types::WakeFailure::timeout(
                                    "wake_runtime_probe",
                                    None,
                                    true,
                                ),
                            )
                            .await;
                            return Ok(true);
                        }
                    }
                };
                if !wake_pending {
                    return Ok(false);
                }
                tracing::info!(
                    "[WAKE] {} traffic waits for app: {}",
                    if touch { "app" } else { "tool" },
                    app_id
                );
                let wake = async {
                    if touch {
                        wc.ensure_serving(&app_id).await
                    } else {
                        wc.ensure_running(&app_id).await
                    }
                };
                match tokio::time::timeout_at(deadline, wake).await {
                    Ok(
                        shared_types::WakeOutcome::Ready
                        | shared_types::WakeOutcome::AlreadyRunning,
                    ) => {}
                    outcome => {
                        let failure = match outcome {
                            Ok(
                                shared_types::WakeOutcome::Timeout(failure)
                                | shared_types::WakeOutcome::Failed(failure),
                            ) => failure,
                            Ok(shared_types::WakeOutcome::Blocked { message, blocker }) => {
                                shared_types::WakeFailure {
                                    blocker: Some(Box::new(blocker)),
                                    ..shared_types::WakeFailure::new(
                                        shared_types::ERR_CONFLICT,
                                        "wake_admission",
                                        message,
                                    )
                                }
                            }
                            Err(_) => shared_types::WakeFailure::timeout("wake_wait", None, false),
                            Ok(
                                shared_types::WakeOutcome::Ready
                                | shared_types::WakeOutcome::AlreadyRunning,
                            ) => {
                                return Ok(false);
                            }
                        };
                        self.respond_wake_failure(session, ctx, failure).await;
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// 上游请求过滤阶段
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        debug!(
            "[PINGORA] upstream_request_filter called: path={}",
            upstream_request.uri.path()
        );

        self.validate_request_api_key(session, upstream_request.uri.path())?;

        let path = Self::normalize_path(upstream_request.uri.path()).to_string();

        // 使用 matchit 匹配路由
        let matched = self.router.at(&path).map_err(|_| {
            warn!("route not found: {}", path);
            pingora_core::Error::new(ErrorType::HTTPStatus(404))
        })?;

        let original_uri = upstream_request.uri.clone();

        self.dispatch_upstream_request(DispatchRequest {
            route: *matched.value,
            params: matched.params,
            original_uri: &original_uri,
            path: &path,
            upstream_request,
            ctx,
        })
        .await?;

        Ok(())
    }

    /// 选择上游服务器
    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        let req_header = session.req_header();
        let path = Self::normalize_path(req_header.uri.path());

        debug!("[PINGORA] upstream_peer called: path={}", path);

        // 使用 matchit 匹配路由
        let matched = self.router.at(path).map_err(|_| {
            warn!("route not found: {}", path);
            pingora_core::Error::new(ErrorType::HTTPStatus(404))
        })?;

        let recovery = ctx.app_connect_recovery.as_mut().and_then(|state| {
            if state.retry_requested {
                state.retry_requested = false;
                let check_runtime = !state.runtime_checked;
                state.runtime_checked = true;
                Some((
                    state.app_id.clone(),
                    state.stage,
                    state.deadline,
                    check_runtime,
                ))
            } else {
                None
            }
        });
        if let Some((app_id, stage, deadline, check_runtime)) = &recovery {
            // dev 无唤醒语义：跳过 wake control（重解析地址发生在 dispatch
            // 阶段的 find_dev_container；本块仅等待端口就绪）。prod 首次重试
            // 前核对集群真实运行态并按需 ensure_running。
            if *stage == crate::service::types::ConnectRecoveryStage::Prod {
                let Some(wc) = self.wake_control.as_ref() else {
                    return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
                };
                if *check_runtime {
                    let state =
                        tokio::time::timeout_at(*deadline, wc.remote_wake_state_fresh(app_id))
                            .await;
                    match state {
                        Ok(state @ (RemoteWakeState::WakePending | RemoteWakeState::Missing)) => {
                            let serving = *matched.value == RouteType::ProdAppProxy;
                            if state == RemoteWakeState::Missing && !serving {
                                return Err(pingora_core::Error::new(ErrorType::HTTPStatus(502)));
                            }
                            let wake = async {
                                if serving {
                                    wc.ensure_serving(app_id).await
                                } else {
                                    wc.ensure_running(app_id).await
                                }
                            };
                            match tokio::time::timeout_at(*deadline, wake).await {
                                Ok(
                                    shared_types::WakeOutcome::Ready
                                    | shared_types::WakeOutcome::AlreadyRunning,
                                ) => {}
                                outcome => {
                                    ctx.wake_failure = match outcome {
                                        Ok(
                                            shared_types::WakeOutcome::Failed(failure)
                                            | shared_types::WakeOutcome::Timeout(failure),
                                        ) => Some(failure),
                                        Ok(shared_types::WakeOutcome::Blocked {
                                            message,
                                            blocker,
                                        }) => Some(shared_types::WakeFailure {
                                            blocker: Some(Box::new(blocker)),
                                            ..shared_types::WakeFailure::new(
                                                shared_types::ERR_CONFLICT,
                                                "wake_admission",
                                                message,
                                            )
                                        }),
                                        Err(_) => Some(shared_types::WakeFailure::timeout(
                                            "wake_wait",
                                            None,
                                            false,
                                        )),
                                        Ok(_) => None,
                                    };
                                    if let Some(ctx_state) = ctx.app_connect_recovery.as_mut() {
                                        ctx_state.unavailable_response = true;
                                    }
                                    return Err(pingora_core::Error::new(ErrorType::HTTPStatus(
                                        503,
                                    )));
                                }
                            }
                        }
                        Ok(RemoteWakeState::Running) => {}
                        Ok(RemoteWakeState::Unavailable) => {
                            return Err(pingora_core::Error::new(ErrorType::HTTPStatus(502)));
                        }
                        Err(_) => {
                            if let Some(ctx_state) = ctx.app_connect_recovery.as_mut() {
                                ctx_state.unavailable_response = true;
                            }
                            return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
                        }
                    }
                }
            }
        }
        // T4a：定位/ensure 纳入恢复总预算——超时不取消已受理的独立创建
        // 工作（builder 租约自有生命周期），本请求诚实失败、后续请求受益。
        let mut peer = match self
            .dispatch_peer_with_recovery_deadline(*matched.value, matched.params.clone(), ctx)
            .await
        {
            Ok(peer) => peer,
            Err(error) => {
                let restore = ctx
                    .app_connect_recovery
                    .as_ref()
                    .filter(|_| *matched.value == RouteType::ProdAppProxy)
                    .map(|state| (state.app_id.clone(), state.deadline));
                let Some((app_id, deadline)) = restore else {
                    return Err(error);
                };
                match self
                    .restore_missing_app_after_dispatch(&app_id, deadline)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return Err(error),
                    Err(failure) => {
                        let status = failure.clone().into_app_error().status_code().as_u16();
                        ctx.wake_failure = Some(failure);
                        return Err(pingora_core::Error::new(ErrorType::HTTPStatus(status)));
                    }
                }
                self.dispatch_peer_with_recovery_deadline(
                    *matched.value,
                    matched.params.clone(),
                    ctx,
                )
                .await?
            }
        };
        if let Some(deadline) = ctx
            .app_connect_recovery
            .as_ref()
            .map(|state| state.deadline)
            && !Self::clamp_peer_to_recovery_deadline(&mut peer, deadline)
        {
            if let Some(state) = ctx.app_connect_recovery.as_mut() {
                state.unavailable_response = true;
            }
            return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
        }
        if let Some((app_id, stage, deadline, _)) = recovery {
            let recovered = if stage == crate::service::types::ConnectRecoveryStage::Dev {
                // T4b：dev 周期重解析等待——等待期间 Pod 换 IP 时跟进权威
                // 新地址（重解析经 dispatch 的 locate；metrics 由 ctx 守卫
                // 每请求只计一次）。
                let mut recovered = false;
                loop {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let slice_deadline = (now + DEV_RE_RESOLVE_SLICE).min(deadline);
                    if wait_for_peer_connection(&peer, slice_deadline).await {
                        recovered = true;
                        break;
                    }
                    // 首个失败切片后提前采集失败分档证据（V2-08：预算内、
                    // 终局只读不再加管理面等待——deadline 耗尽时顾问拿不到
                    // 预算，证据必须在等待窗口内落进恢复上下文）。
                    self.collect_connection_evidence(ctx, &app_id, "dev", deadline)
                        .await;
                    // 切片等待未就绪：预算内重解析一次（ensure 的独立受理
                    // 工作不受本请求等待影响）。
                    match self
                        .dispatch_peer_with_recovery_deadline(
                            *matched.value,
                            matched.params.clone(),
                            ctx,
                        )
                        .await
                    {
                        Ok(re_resolved) => {
                            peer = re_resolved;
                            // 重解析的 peer 带默认建连超时——同样收紧到剩余预算，
                            // 否则循环终局返回的 peer 拨号可超出 deadline。
                            if !Self::clamp_peer_to_recovery_deadline(&mut peer, deadline) {
                                break;
                            }
                        }
                        Err(error) => {
                            warn!(%app_id, "dev re-resolve during connection wait failed: {error}");
                            break;
                        }
                    }
                }
                recovered
            } else {
                let mut recovered = false;
                loop {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        break;
                    }
                    if wait_for_peer_connection(&peer, (now + DEV_RE_RESOLVE_SLICE).min(deadline))
                        .await
                    {
                        recovered = true;
                        break;
                    }
                    if tokio::time::Instant::now() < deadline {
                        self.collect_connection_evidence(ctx, &app_id, "prod", deadline)
                            .await;
                    }
                }
                recovered
            };
            if !recovered {
                warn!(%app_id, "UserApp service connection did not recover before wait deadline");
                if let Some(state) = ctx.app_connect_recovery.as_mut() {
                    state.unavailable_response = true;
                }
                return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
            }
        }
        Ok(peer)
    }

    /// 代理失败终局：UserApp app 路由给友好表示（真实状态码）；其余路由
    /// 保持 Pingora 默认（空体 respond_error）。响应已开始的流不会进入本
    /// 钩子——不存在中途注入 HTML 的路径。
    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        error: &pingora_core::Error,
        ctx: &mut Self::CTX,
    ) -> pingora_proxy::FailToProxy
    where
        Self::CTX: Send + Sync,
    {
        let connect_failure =
            ctx.userapp_route.is_some() && is_entry_connect_failure(error.etype());
        let code = if connect_failure {
            503
        } else {
            default_error_status(error)
        };
        if connect_failure {
            if !ctx.fresh_failure_evidence
                && let Some(route) = ctx.userapp_route.clone()
            {
                // A no-recovery path may spend only a short diagnostics budget.
                self.collect_connection_evidence(
                    ctx,
                    &route.app_id,
                    route.stage,
                    tokio::time::Instant::now() + Duration::from_secs(1),
                )
                .await;
            }
            Self::capture_proxy_transition(ctx);
        }
        if ctx.userapp_route.is_some() && code > 0 {
            // 原因用封闭的错误类型词表（etype），不透出内部地址/凭据细节
            let reason = error.etype().as_str().to_string();
            self.respond_userapp_error(
                session,
                ctx,
                code,
                crate::error_page::ErrorPageCause::Generic,
                &reason,
                connect_failure.then_some(15),
                // 错误链完整进日志（可能含内部地址——只进日志不进页面）
                &format!("pingora error: {error}"),
            )
            .await;
            return pingora_proxy::FailToProxy {
                error_code: code,
                can_reuse_downstream: false,
            };
        }
        if code > 0
            && let Err(write_error) = session.respond_error(code).await
        {
            tracing::warn!("send default error response failed: {write_error}");
        }
        pingora_proxy::FailToProxy {
            error_code: code,
            can_reuse_downstream: false,
        }
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut error: Box<pingora_core::Error>,
    ) -> Box<pingora_core::Error> {
        if let Some(state) = ctx.app_connect_recovery.as_mut()
            && matches!(
                error.etype(),
                ErrorType::ConnectRefused | ErrorType::ConnectNoRoute | ErrorType::ConnectTimedout
            )
        {
            if tokio::time::Instant::now() < state.deadline {
                state.retry_requested = true;
                error.set_retry(true);
            } else {
                state.unavailable_response = true;
            }
        }
        error
    }

    /// 连接到上游后的回调
    ///
    /// 用于记录连接协议信息（HTTP/1.1 或 HTTP/2）
    /// 注意: http_version 显示的是 ALPN 配置偏好，实际协商结果可在 Pingora 底层日志查看
    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        reused: bool,
        peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        digest: Option<&Digest>,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        ctx.app_connect_recovery = None;
        ctx.failure_hint = None;
        ctx.fresh_failure_evidence = false;
        ctx.failure_hint_at = None;
        // 记录连接是否被重用
        ctx.connection_reused = reused;

        // 根据 peer 的 ALPN 配置推断协议
        let alpn_str = match peer.options.alpn {
            ALPN::H2 => "HTTP/2 (H2)",
            ALPN::H2H1 => "HTTP/2 preferred (H2H1)",
            ALPN::H1 => "HTTP/1.1 (H1)",
            ALPN::Custom(_) => "Custom ALPN",
        };
        ctx.http_version = Some(alpn_str.to_string());

        // 获取 TLS 版本信息
        let tls_info = digest
            .and_then(|d| d.ssl_digest.as_ref())
            .map(|ssl| format!("TLS {}", ssl.version))
            .unwrap_or_else(|| "No TLS".to_string());

        // 只在 API 代理场景打印详细日志
        if ctx.upstream_host.is_some() {
            debug!(
                "[API_PROXY] Connection established: ALPN={}, {}, reused={}",
                alpn_str, tls_info, reused
            );
        }

        Ok(())
    }

    /// 响应过滤阶段
    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()>
    where
        Self::CTX: Send,
    {
        // 预览跨 Pod 转发收到宿主 410（身份不匹配：IP 复用/实例换代）——
        // 立即失效本端口的解析缓存，下次请求重解析到新宿主（免等 TTL）。
        if ctx.preview_peer.is_some()
            && upstream_response.status == 410
            && let Some(origin_port) = ctx.preview_origin_port
            && let Some(deps) = self.preview_slot.load().as_ref()
        {
            tracing::info!(
                port = origin_port,
                "preview forward got 410 from host; invalidating route cache"
            );
            deps.coordination.invalidate_route(origin_port).await;
        }
        // 记录响应状态
        let status = upstream_response.status;
        let status_text = status.to_string();
        let duration = ctx.start.elapsed();

        // 记录指标
        self.metrics.record_response(&status_text, duration);

        // 如果有目标端口，记录端口指标
        if let Some(port) = ctx.target_port {
            self.metrics
                .record_response_port(port, &status_text, duration);
        }

        // 减少活跃连接数
        self.metrics.dec_active();
        ctx.prod_metrics_counted = false;
        ctx.dev_metrics_counted = false;

        // 记录上游状态码：upstream_response_body_filter 据此收集 4xx/5xx 错误体
        ctx.upstream_status = Some(status.as_u16());

        // ── Pingap 自产错误识别（T11）─────────────────────────────────────
        // 应用容器入口（:9080 pingap）返回的 502/503/504 带 `X-Pingap-EType`：
        // 仅当失败顾问确认当前实例生效配置具备来源剥离规则（应用同名头已被
        // 移除）时才替换正文；未知来源/custom/旧配置一律透传原响应。
        if let Some(route) = ctx.userapp_route.clone()
            && (502..=504).contains(&status.as_u16())
            && upstream_response
                .headers
                .get(shared_types::X_PINGAP_ETYPE_HEADER)
                .is_some()
        {
            // 空体标记错误（Content-Length: 0）不替换：pingora 对 eos-at-header
            // 的响应不再触发 body filter，改写头会造成宣告长度与零正文不符的
            // 截断响应——原样透传。
            let zero_body = upstream_response
                .headers
                .get("content-length")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.trim_ascii() == "0");
            if !zero_body {
                self.maybe_replace_pingap_error(session, upstream_response, ctx, &route)
                    .await;
            }
        }

        // 只在 API 代理场景打印详细日志
        if ctx.upstream_host.is_some() {
            debug!(
                "[API_PROXY] Response: status={}, duration={:?}",
                status_text, duration
            );
        }

        Ok(())
    }

    /// 上游响应体过滤（经 ProxyServiceWrapper 委托真正生效）
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Option<Duration>> {
        // 已确认来源的 Pingap 自产错误替换：首块输出新正文一次、丢弃原错误
        // 体（不拼接）；旧 Content-Encoding 已在 header 阶段移除，此处恒为
        // 身份编码，按新表示重建传输边界。
        if ctx.replace_error_body.is_some() {
            if ctx.error_replacement_emitted {
                *body = None;
            } else {
                *body = ctx.replace_error_body.clone();
                ctx.error_replacement_emitted = true;
            }
            if _end_of_stream {
                ctx.replace_error_body = None;
            }
            return Ok(None);
        }

        // 如果是 4xx/5xx 响应，收集错误响应体（封顶 64KB：网关 HTML 大页/
        // 误配代理到的默认站点会让单请求内存随 body 线性增长）
        if let Some(status) = ctx.upstream_status
            && status >= 400
            && let Some(body_bytes) = body
        {
            const MAX_ERROR_BODY: usize = 64 * 1024;
            let remaining = MAX_ERROR_BODY.saturating_sub(ctx.error_body_buf.len());
            if remaining > 0 {
                ctx.error_body_buf
                    .extend_from_slice(&body_bytes[..remaining.min(body_bytes.len())]);
            }
            // 流结束时消费：错误体首 1KB 打日志（排障——网关 HTML 大页/
            // 误配默认站点的根因就在 body 里；buffer 本身 64KB 封顶防膨胀）
            if _end_of_stream && !ctx.error_body_buf.is_empty() {
                let head: Vec<u8> = ctx.error_body_buf.iter().copied().take(1024).collect();
                debug!(
                    "[PORT_PROXY] upstream {} error body: {}",
                    ctx.upstream_status.unwrap_or(0),
                    String::from_utf8_lossy(&head)
                );
            }
        }

        Ok(None)
    }
}

impl PortProxy {
    /// 规范化路径（去除尾部斜杠）
    fn normalize_path(raw: &str) -> &str {
        utils::normalize_path(raw)
    }

    /// Pingap 自产错误（已带标记头）的正文替换决策：来源确认才替换。
    ///
    /// 替换只动表示：状态码保留；`Content-Encoding`/旧 `Content-Length`/
    /// 实体校验头移除，按新表示重建；正文经 body filter 一次性输出。
    /// 未知来源/custom/旧配置无证据 → 原样透传（不猜测、不无条件换 502）。
    async fn maybe_replace_pingap_error(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut TrackingCtx,
        route: &crate::service::types::UserAppRouteCtx,
    ) {
        // EType 归类先行。standby 标记值可被应用伪造（custom/旧配置不剥离
        // 应用同名头），来源确认不可省略——但与昂贵的文案观察分离（Codex P2）：
        // 来源走缓存路径（advise，30s TTL——error_origin_contract 是配置衍生
        // 的慢变量），未确认仍透传；文案细分走新鲜观察尽力而为，缺失时
        // standby 兜底 Restarting（摘流正文本身即"重启中"语义）。
        let etype_header = upstream_response
            .headers
            .get(shared_types::X_PINGAP_ETYPE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let standby_window = etype_header.as_deref().is_some_and(is_standby_etype_header);
        let advisor = self.failure_advisor_slot.load_full();
        // 失败路径剩余预算（无唤醒上下文时用短上限——诊断绝不延长已耗尽的
        // 代理等待）
        let budget = ctx
            .app_connect_recovery
            .as_ref()
            .map(|recovery| {
                recovery
                    .deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(Duration::from_millis(1500))
            })
            .unwrap_or_else(|| Duration::from_millis(1000));
        let hint = if standby_window {
            // 来源：三态轻量判定（Codex P1/P2）——Confirmed=缓存命中（TTL 内
            // 即时替换）；Unconfirmed=显式否定（custom/伪造，透传）；Unknown=
            // 冷缓存（实现方后台预热，本请求透传，浏览器按 Retry-After 重试
            // 命中——冷启动竞争由预热关闭而非等待 1s 观察竞速）。
            let origin = match advisor.as_ref() {
                Some(advisor) => advisor.standby_origin(&route.app_id, route.stage).await,
                None => crate::error_page::StandbyOrigin::Unknown,
            };
            if origin != crate::error_page::StandbyOrigin::Confirmed {
                return;
            }
            // 文案：新鲜观察尽力而为；超时（None）由 Restarting 兜底；**显式
            // 否定**（Some 且未确认——门命中旧缓存后配置已换代，custom/伪造）
            // 放弃本次替换：origin 缓存已被同次观察落账否定，下轮门即拒绝
            // （Codex 五轮：暖门不得覆盖新鲜否定）。
            match advisor.as_ref() {
                Some(advisor) => {
                    let fresh = advisor
                        .advise_current(&route.app_id, route.stage, budget)
                        .await;
                    if fresh.as_ref().is_some_and(|h| !h.error_origin_confirmed) {
                        return;
                    }
                    fresh.filter(|h| h.error_origin_confirmed)
                }
                None => None,
            }
        } else {
            // 既有 T11 路径：新鲜观察一次承担来源与文案。
            match advisor.as_ref() {
                Some(advisor) => {
                    advisor
                        .advise_current(&route.app_id, route.stage, budget)
                        .await
                }
                None => None,
            }
        };
        // 非 standby：来源未确认（custom/旧配置）→ 原样透传（Codex P1/P2 回归门）。
        if !standby_window && !hint.as_ref().is_some_and(|h| h.error_origin_confirmed) {
            return;
        }
        let Some(renderer) = self.error_pages_slot.load_full() else {
            return;
        };
        // 替换决策读取页面前同样触发按需刷新（错误请求是收敛路径之一）
        renderer.request_refresh();
        let connect_failure = etype_header.as_deref().is_some_and(is_connect_etype_header);
        if connect_failure {
            if let Err(error) = upstream_response.set_status(503) {
                tracing::warn!("normalize trusted proxy connection failure status failed: {error}");
                return;
            }
            ctx.failure_hint = hint.clone();
            ctx.failure_hint_at = Some(tokio::time::Instant::now());
            ctx.fresh_failure_evidence = true;
            Self::capture_proxy_transition(ctx);
        }
        let cause = replacement_cause(hint.as_ref(), standby_window);
        let status = upstream_response.status.as_u16();
        let diagnostic_id = crate::error_page::new_diagnostic_id();
        // 语言协商与 write_error_response 同一函数（Accept-Language → locale）
        let locale = shared_types::parse_accept_language(
            session
                .req_header()
                .headers
                .get("accept-language")
                .and_then(|value| value.to_str().ok()),
        );
        let (content_type, body): (&'static str, bytes::Bytes) =
            match crate::error_page::negotiate(session) {
                crate::error_page::ErrorRepresentation::Document => {
                    let (title, message) = cause.copywriting(locale);
                    let vars = crate::error_page::ErrorPageVars {
                        title,
                        message,
                        diagnostic_id: diagnostic_id.clone(),
                        status: status.to_string(),
                        cause_slug: crate::error_page::cause_slug(cause).to_string(),
                    };
                    (
                        "text/html; charset=utf-8",
                        renderer.render(&vars, locale).into(),
                    )
                }
                crate::error_page::ErrorRepresentation::Machine => {
                    let (_title, message) = cause.copywriting(locale);
                    let payload = crate::error_page::MachineErrorPayload::new(
                        status,
                        if standby_window {
                            "proxy_standby"
                        } else {
                            "pingap_self_produced_error"
                        },
                        message,
                        diagnostic_id.clone(),
                    )
                    .with_operation_id(
                        hint.as_ref()
                            .and_then(|h| h.transition_operation_id().map(str::to_string)),
                    );
                    ("application/json", payload.to_body().into())
                }
            };
        // 摘流方宣告的重试节奏在替换前捕获（strip 循环会移除该头）
        let announced_retry_after = upstream_response
            .headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // 按新表示重建头：移除编码/实体校验/缓存建议，替换 Content-Type/Length。
        // 个别头写失败仅记日志——不撤销替换（正文已按新表示重建）。
        for header in [
            "content-encoding",
            "content-length",
            "transfer-encoding",
            "etag",
            "last-modified",
            "cache-control",
            "retry-after",
        ] {
            if let Some(_removed) = upstream_response.remove_header(header) {
                // 占用返回值；无旧值 = 头本就不存在（幂等）
            }
        }
        if let Err(error) = upstream_response.insert_header("content-type", content_type) {
            tracing::warn!("rewrite content-type on replaced response failed: {error}");
        }
        if let Err(error) =
            upstream_response.insert_header("content-length", body.len().to_string())
        {
            tracing::warn!("rewrite content-length on replaced response failed: {error}");
        }
        if let Err(error) = upstream_response.insert_header("cache-control", "no-store") {
            tracing::warn!("rewrite cache-control on replaced response failed: {error}");
        }
        if let Err(error) = upstream_response
            .insert_header(crate::error_page::DIAGNOSTIC_HEADER, diagnostic_id.as_str())
        {
            tracing::warn!("rewrite diagnostic header on replaced response failed: {error}");
        }
        if let Some(value) = replacement_retry_after(
            connect_failure,
            standby_window,
            announced_retry_after.as_deref(),
        ) && let Err(error) = upstream_response.insert_header("retry-after", value.as_str())
        {
            tracing::warn!("rewrite retry-after on replaced response failed: {error}");
        }
        if connect_failure
            && let Some(operation_id) = hint
                .as_ref()
                .and_then(|h| h.transition_operation_id().map(str::to_string))
            && let Err(error) =
                upstream_response.insert_header("x-rcoder-operation-id", &operation_id)
        {
            tracing::warn!("insert current proxy operation identity failed: {error}");
        }
        // 移除标记头：替换后的表示不再携带来源标记（避免下游重复判定）
        if let Some(_removed) = upstream_response.remove_header(shared_types::X_PINGAP_ETYPE_HEADER)
        {
        }
        ctx.replace_error_body = Some(body);
        ctx.error_replacement_emitted = false;
        tracing::info!(
            app_id = %route.app_id,
            stage = route.stage,
            %status,
            diagnostic_id = %diagnostic_id,
            readiness = ?hint.as_ref().map(|h| h.readiness_status),
            standby = standby_window,
            "replaced confirmed pingap self-produced error body"
        );
    }

    /// UserApp 失败出口的统一友好表示。
    ///
    /// 未装配错误页呈现器时保持旧的极简响应（header-only）——能力接入前
    /// 行为零回退。真实状态码保留；HEAD 无正文；写失败不二次发送。
    async fn respond_wake_failure(
        &self,
        session: &mut Session,
        ctx: &mut TrackingCtx,
        failure: shared_types::WakeFailure,
    ) {
        let status = failure.clone().into_app_error().status_code().as_u16();
        let code = failure.code.clone();
        // 错误码是终局证据：按码映射文案档位（404 已删除→Missing、409 围栏
        // →Blocked 等），不再恒用 Generic 等 advisor 观察
        let cause = crate::error_page::wake_failure_cause(&failure);
        let detail = shared_types::sanitize_error_text(&failure.message);
        let retry = failure.retryable.then_some(15);
        ctx.wake_failure = Some(failure);
        self.respond_userapp_error(session, ctx, status, cause, code.as_ref(), retry, &detail)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn respond_userapp_error(
        &self,
        session: &mut Session,
        ctx: &mut TrackingCtx,
        status: u16,
        cause: crate::error_page::ErrorPageCause,
        reason: &str,
        retry_after_secs: Option<u64>,
        detail: &str,
    ) {
        // 文案证据：失败顾问有当前实例观察时用确定文案（starting/stopped/
        // failed）；Generic 入参或无证据一律通用文案（不凭连接拒绝宣称启动中）。
        let cause = if ctx.fresh_failure_evidence {
            ctx.failure_hint
                .as_ref()
                .map(|hint| hint.page_cause())
                .unwrap_or(cause)
        } else if matches!(cause, crate::error_page::ErrorPageCause::Generic)
            && let Some(advisor) = self.failure_advisor_slot.load_full()
            && let Some(route) = ctx.userapp_route.clone()
        {
            let budget = ctx
                .app_connect_recovery
                .as_ref()
                .map(|recovery| {
                    recovery
                        .deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .min(Duration::from_millis(1500))
                })
                .unwrap_or_else(|| Duration::from_millis(1000));
            advisor
                .advise(&route.app_id, route.stage, budget)
                .await
                .map(|hint| hint.page_cause())
                .unwrap_or(cause)
        } else {
            cause
        };
        if let Some(renderer) = self.error_pages_slot.load_full() {
            let context = ctx
                .userapp_route
                .as_ref()
                .map(|route| format!("app={} stage={}", route.app_id, route.stage))
                .unwrap_or_else(|| "userapp app proxy".to_string());
            crate::error_page::write_error_response(
                session,
                &renderer,
                status,
                cause,
                reason,
                retry_after_secs,
                &context,
                detail,
                ctx.wake_failure.as_ref(),
            )
            .await;
            return;
        }
        // 无呈现器回退：同样必须守卫"响应已开始"（与 write_error_response
        // 同语义）——中途断流时连极简响应也不能追加。
        let already_final = session
            .as_downstream_mut()
            .response_written()
            .is_some_and(|written| {
                !written.status.is_informational() || written.status.as_u16() == 101
            });
        if already_final {
            return;
        }
        let response = ResponseHeader::build(status, None).ok();
        if let Some(mut response) = response {
            if let Some(failure) = &ctx.wake_failure {
                if let Err(error) =
                    response.insert_header("x-rcoder-error-code", failure.code.as_ref())
                {
                    tracing::warn!("insert wake error code header failed: {error}");
                }
                if let Some(operation_id) = &failure.operation_id
                    && let Err(error) =
                        response.insert_header("x-rcoder-operation-id", operation_id)
                {
                    tracing::warn!("insert wake operation header failed: {error}");
                }
            }
            if let Some(seconds) = retry_after_secs
                && let Err(error) = response.insert_header("Retry-After", seconds.to_string())
            {
                tracing::warn!("insert retry-after into legacy error response failed: {error}");
            }
            if let Err(error) = session
                .write_response_header(Box::new(response), true)
                .await
            {
                tracing::warn!("write legacy error response failed: {error}");
            }
        }
    }
}

fn is_entry_connect_failure(error: &ErrorType) -> bool {
    matches!(
        error,
        ErrorType::ConnectRefused | ErrorType::ConnectNoRoute | ErrorType::ConnectTimedout
    )
}
fn is_connect_etype_header(value: &str) -> bool {
    matches!(
        value,
        "ConnectRefused" | "ConnectNoRoute" | "ConnectTimedout"
    )
}
/// standby 摘流窗口标记（managed 生成配置显式携带，见 app-cli compiler；
/// 该窗口无 upstream 代理，应用不可能注入同名头）。
fn is_standby_etype_header(value: &str) -> bool {
    value.eq_ignore_ascii_case(shared_types::PINGAP_STANDBY_ETYPE)
}

/// 替换文案档位：advisor 观察确认时用其细分（starting/stopped/failed/…）；
/// standby 窗口观察缺失（advisor 超时/不可达——其 1s 预算常输给完整观察
/// 时延）或观察为 Generic 时兜底 Restarting（摘流正文本身即"重启中"语义）；
/// 非 standby 且未确认的情况不会经过门控，返回 Generic 仅为穷尽。
fn replacement_cause(
    hint: Option<&crate::error_page::UserAppProxyFailureHint>,
    standby_window: bool,
) -> crate::error_page::ErrorPageCause {
    match hint {
        Some(h) if h.error_origin_confirmed => match h.page_cause() {
            crate::error_page::ErrorPageCause::Generic if standby_window => {
                crate::error_page::ErrorPageCause::Restarting
            }
            other => other,
        },
        _ if standby_window => crate::error_page::ErrorPageCause::Restarting,
        _ => crate::error_page::ErrorPageCause::Generic,
    }
}

/// 替换后的 Retry-After 决策：连接失败统一 15s（唤醒恢复节奏）；standby
/// 摘流保留入口宣告值（2s——发布确认预算节奏）；其余替换不宣告重试。
fn replacement_retry_after(
    connect_failure: bool,
    standby_window: bool,
    announced: Option<&str>,
) -> Option<String> {
    if connect_failure {
        return Some("15".to_string());
    }
    if standby_window {
        return announced
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }
    None
}

/// Pingora 默认 fail_to_proxy 的状态映射（proxy_trait.rs 631-668 同款）——
/// UserApp 路由经此拿到与默认一致的真实状态码后再做友好表示。
fn default_error_status(error: &pingora::Error) -> u16 {
    use pingora::ErrorSource;
    use pingora::ErrorType::{ConnectionClosed, HTTPStatus, ReadError, WriteError};
    match error.etype() {
        HTTPStatus(code) => *code,
        _ => match error.esource() {
            ErrorSource::Upstream => 502,
            ErrorSource::Downstream => match error.etype() {
                WriteError | ReadError | ConnectionClosed => 0,
                _ => 400,
            },
            ErrorSource::Internal | ErrorSource::Unset => 500,
        },
    }
}

/// 唤醒/连接恢复目标分类：路径 → `(app_id, touch, stage)`。
///
/// - `/api/v1/userapp/proxy/app/prod/...` 应用业务流量 → touch=true（闲置回收信号源）、Prod；
/// - `/api/v1/userapp/proxy/{ttyd,dbx}/prod/{app_id}` 工具族 → touch=false（终端/DB
///   连接不算业务活跃，不刷新闲置计时——挂终端不阻止回收，回收后下次连接再唤醒）、Prod；
/// - dev 三族（app/dbx/ttyd）→ 连接恢复覆盖（P0），touch=false 且 Dev 阶段
///   （dev 访问追踪信号尚未按 stage 对接正确的回收器，不冒充业务活跃）；
/// - 其余路由 → None（不触发唤醒/恢复）。
fn classify_wake_target(
    router: &matchit::Router<RouteType>,
    path: &str,
) -> Option<(String, bool, crate::service::types::ConnectRecoveryStage)> {
    use crate::service::types::ConnectRecoveryStage;
    if !path.starts_with("/api/v1/userapp/proxy/") {
        return None;
    }
    let matched = router.at(path).ok()?;
    let stage = match matched.value {
        RouteType::ProdAppProxy | RouteType::RuntimeTtydProxy | RouteType::ProdDbxProxy => {
            ConnectRecoveryStage::Prod
        }
        RouteType::DevAppProxy | RouteType::DevDbxProxy | RouteType::DevTtydProxy => {
            ConnectRecoveryStage::Dev
        }
        _ => return None,
    };
    let touch =
        stage == ConnectRecoveryStage::Prod && matches!(matched.value, RouteType::ProdAppProxy);
    matched
        .params
        .get("app_id")
        .map(|s| (s.to_string(), touch, stage))
}

/// dbx 入口无尾斜杠 → 307 目标（原路径 + `/`，query 原样保留；相对 Location，
/// 不拼 origin——代理可被挂任意 host/端口下）。
///
/// dbx-web 是相对路径 SPA（index.html `./assets/...`），浏览器以 URL 目录段为
/// 基准解析相对引用——无尾斜杠时 `{app_id}` 被当文件名、基准向上错位一级
/// （`./assets` 落到 `/{user_id}/assets` → 静态资源 404 白屏）。判定=路由命中
/// dbx 族且逻辑上无子路径（`{*path}` 缺失或为空——两种 matchit 命中形态等价，
/// 不依赖 matchit 在精确/通配间的择优）；带尾斜杠/带子路径请求不受影响，
/// `!path.ends_with('/')` 兼作 matchit 尾斜杠行为变化时的死循环双保险。
fn dbx_root_redirect_location(
    router: &matchit::Router<RouteType>,
    path: &str,
    query: Option<&str>,
) -> Option<String> {
    if !path.starts_with("/api/v1/userapp/proxy/dbx/") || path.ends_with('/') {
        return None;
    }
    let matched = router.at(path).ok()?;
    match matched.value {
        RouteType::DevDbxProxy | RouteType::ProdDbxProxy
            if matched.params.get("path").is_none_or(|p| p.is_empty()) =>
        {
            Some(match query {
                Some(q) => format!("{path}/?{q}"),
                None => format!("{path}/"),
            })
        }
        _ => None,
    }
}

/// 预览转发闸门挂载（impl PortProxy 块，非 ProxyHttp trait 成员）。
impl PortProxy {
    /// 预览转发内部入口闸门（request_filter 专用短路）。
    ///
    /// 语义（spec 行为不变量 11/12）：
    /// - 槽未装配/令牌缺失或不符 → 404（外部探测者不可区分端点是否存在）；
    /// - 实例/宿主/端口身份不匹配（IP 复用、实例换代）→ 410（转发方据此失效
    ///   缓存重解析）；
    /// - 权威库称本机宿主但本地登记缺失（对账/心跳收敛中）→ 503，不转发；
    /// - 校验通过 → 记 `ctx.preview_forward_port` 放行（后续阶段不再重复校验）。
    async fn preview_forward_gate(
        session: &mut Session,
        ctx: &mut TrackingCtx,
        preview_slot: &std::sync::Arc<
            arc_swap::ArcSwapOption<std::sync::Arc<super::types::PreviewRouteDeps>>,
        >,
    ) -> PingoraResult<bool> {
        async fn respond(status: u16, session: &mut Session) -> PingoraResult<bool> {
            let resp = ResponseHeader::build(status, None)?;
            session.write_response_header(Box::new(resp), true).await?;
            Ok(true)
        }
        let Some(deps) = preview_slot.load().as_ref().map(std::sync::Arc::clone) else {
            return respond(404, session).await;
        };
        let token_ok = session
            .req_header()
            .headers
            .get("x-preview-internal-token")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|provided| provided == deps.internal_token);
        if !token_ok {
            return respond(404, session).await;
        }
        // 路径手工解析（request_filter 阶段无 matchit 参数）：
        // /internal/preview-forward/{instance_id}/{port}/{rest...}
        let segments: Vec<&str> = session
            .req_header()
            .uri
            .path()
            .trim_start_matches('/')
            .split('/')
            .collect();
        // [internal, preview-forward, {instance_id}, {port}, {rest...}...]
        if segments.len() < 5 || segments[0] != "internal" || segments[1] != "preview-forward" {
            // 根形态（无 rest）由路由层兜底，但无 rest 的转发无意义——404
            return respond(404, session).await;
        }
        let instance_id = segments[2];
        let Ok(vite_port) = segments[3].parse::<u16>() else {
            return respond(404, session).await;
        };
        match deps
            .coordination
            .check_forward(instance_id, vite_port)
            .await
        {
            shared_types::PreviewForwardCheck::Allowed => {
                ctx.preview_forward_port = Some(vite_port);
                Ok(false)
            }
            shared_types::PreviewForwardCheck::IdentityMismatch => {
                tracing::info!(
                    instance_id,
                    vite_port,
                    "preview forward identity mismatch -> 410 (forwarder should re-resolve)"
                );
                respond(410, session).await
            }
            shared_types::PreviewForwardCheck::NotReady => respond(503, session).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::create_router;
    use matchit::Router;

    fn test_router() -> Router<RouteType> {
        create_router().expect("router build")
    }

    /// 工具族路由：识别为唤醒目标且不 touch（挂终端不算业务活跃）。
    #[test]
    fn tool_routes_wake_without_touch() {
        let router = test_router();
        // classify 接收 request_filter 规范化后的路径（尾部斜杠已剥）
        for path in [
            "/api/v1/userapp/proxy/ttyd/prod/u1/app-1",
            "/api/v1/userapp/proxy/ttyd/prod/u1/app-1/token.js",
            "/api/v1/userapp/proxy/dbx/prod/u1/app-1",
        ] {
            let (app_id, touch, stage) =
                classify_wake_target(&router, path).unwrap_or_else(|| panic!("{path} unmatched"));
            assert_eq!(app_id, "app-1", "{path}");
            assert!(!touch, "tool traffic must not refresh idle timer: {path}");
            assert_eq!(
                stage,
                crate::service::types::ConnectRecoveryStage::Prod,
                "prod tool family: {path}"
            );
        }
    }

    /// 应用业务流量：touch + wake。
    #[test]
    fn app_traffic_wakes_with_touch() {
        let router = test_router();
        let (app_id, touch, stage) =
            classify_wake_target(&router, "/api/v1/userapp/proxy/app/prod/u1/app-1/x")
                .expect("app proxy route must match");
        assert_eq!(app_id, "app-1");
        assert!(touch);
        assert_eq!(stage, crate::service::types::ConnectRecoveryStage::Prod);
    }

    /// 非唤醒路由不触发。
    #[test]
    fn other_routes_do_not_wake() {
        let router = test_router();
        assert!(classify_wake_target(&router, "/api/v1/userapp/build").is_none());
        assert!(classify_wake_target(&router, "/web/ttyd/u1").is_none());
        // 旧路径形态（前缀统一前）不再命中任何 userApp 路由
        assert!(classify_wake_target(&router, "/userapp/proxy/ttyd/u1/app-1").is_none());
        assert!(classify_wake_target(&router, "/proxy/userapp/proxy/1/app-1/x").is_none());
    }

    /// P0：dev 三族纳入连接恢复（touch=false，Dev 阶段）。
    #[test]
    fn dev_families_enter_recovery_without_touch() {
        let router = test_router();
        for path in [
            "/api/v1/userapp/proxy/app/dev/u1/app-1/x",
            "/api/v1/userapp/proxy/dbx/dev/u1/app-1",
            "/api/v1/userapp/proxy/ttyd/dev/u1/app-1",
        ] {
            let (app_id, touch, stage) =
                classify_wake_target(&router, path).unwrap_or_else(|| panic!("{path} unmatched"));
            assert_eq!(app_id, "app-1", "{path}");
            assert!(
                !touch,
                "dev traffic must not feed prod idle signals: {path}"
            );
            assert_eq!(
                stage,
                crate::service::types::ConnectRecoveryStage::Dev,
                "dev family must classify as Dev: {path}"
            );
        }
    }

    /// dbx 入口无尾斜杠：307 到同路径 + `/`（query 原样保留），dev/prod 双阶段。
    #[test]
    fn dbx_root_without_trailing_slash_redirects() {
        let router = test_router();
        assert_eq!(
            dbx_root_redirect_location(&router, "/api/v1/userapp/proxy/dbx/dev/4/5", None),
            Some("/api/v1/userapp/proxy/dbx/dev/4/5/".to_string())
        );
        assert_eq!(
            dbx_root_redirect_location(
                &router,
                "/api/v1/userapp/proxy/dbx/prod/4/app-5",
                Some("a=1")
            ),
            Some("/api/v1/userapp/proxy/dbx/prod/4/app-5/?a=1".to_string())
        );
    }

    /// 带尾斜杠/带子路径已是正确基准不重定向；其他工具族与无关路由不误伤。
    #[test]
    fn dbx_paths_with_content_do_not_redirect() {
        let router = test_router();
        let base = "/api/v1/userapp/proxy/dbx/dev/4/5";
        assert_eq!(
            dbx_root_redirect_location(&router, &format!("{base}/"), None),
            None
        );
        assert_eq!(
            dbx_root_redirect_location(&router, &format!("{base}/assets/index.js"), None),
            None
        );
        assert_eq!(
            dbx_root_redirect_location(&router, &format!("{base}/api/auth/check"), None),
            None
        );
        // ttyd/vnc 非 dbx 族不受影响（ttyd 为 WS 直连形态，无相对路径基准问题）
        assert_eq!(
            dbx_root_redirect_location(&router, "/api/v1/userapp/proxy/ttyd/dev/4/5", None),
            None
        );
        assert_eq!(
            dbx_root_redirect_location(&router, "/api/v1/userapp/proxy/ttyd/prod/4/5", None),
            None
        );
        assert_eq!(
            dbx_root_redirect_location(&router, "/web/ttyd/u1", None),
            None
        );
    }
}

#[cfg(test)]
mod official_restart_failure_tests {
    use super::*;

    #[test]
    fn only_pre_http_entry_connect_errors_are_unavailability() {
        for error in [
            ErrorType::ConnectRefused,
            ErrorType::ConnectNoRoute,
            ErrorType::ConnectTimedout,
        ] {
            assert!(is_entry_connect_failure(&error));
        }
        for error in [
            ErrorType::ReadTimedout,
            ErrorType::WriteTimedout,
            ErrorType::HTTPStatus(502),
            ErrorType::HTTPStatus(503),
            ErrorType::HTTPStatus(504),
        ] {
            assert!(!is_entry_connect_failure(&error));
        }
        assert!(is_connect_etype_header("ConnectRefused"));
        assert!(!is_connect_etype_header("HTTPStatus"));
        assert!(!is_connect_etype_header("ReadTimedout"));
        assert!(is_standby_etype_header("rcoder-standby"));
        assert!(is_standby_etype_header("RCoder-Standby"));
        assert!(!is_standby_etype_header("ConnectRefused"));
        assert!(!is_standby_etype_header("custom-app-value"));
    }

    /// 替换后的 Retry-After：连接失败统一 15s；standby 保留摘流方宣告节奏；
    /// 其余替换不宣告。
    #[test]
    fn replacement_retry_after_follows_failure_class() {
        assert_eq!(
            replacement_retry_after(true, false, Some("2")),
            Some("15".to_string())
        );
        assert_eq!(
            replacement_retry_after(false, true, Some("2")),
            Some("2".to_string()),
            "standby must preserve the advertised publication retry cadence"
        );
        assert_eq!(replacement_retry_after(false, true, Some("  ")), None);
        assert_eq!(replacement_retry_after(false, true, None), None);
        assert_eq!(replacement_retry_after(false, false, Some("2")), None);
    }

    /// 替换文案档位：advisor 确认观察给细分文案；standby 观察缺失兜底
    /// Restarting（不再因 advisor 超时放弃替换——2026-10-27 事故）。
    #[test]
    fn replacement_cause_survives_missing_advisor_for_standby() {
        let confirmed = |readiness: shared_types::UserAppReadinessStatus| {
            crate::error_page::UserAppProxyFailureHint {
                readiness_status: Some(readiness),
                error_origin_confirmed: true,
                proxy_confirmation: None,
            }
        };
        assert_eq!(
            replacement_cause(
                Some(&confirmed(shared_types::UserAppReadinessStatus::Stopped)),
                true
            ),
            crate::error_page::ErrorPageCause::Stopped,
            "advisor evidence still refines the standby copy"
        );
        assert_eq!(
            replacement_cause(None, true),
            crate::error_page::ErrorPageCause::Restarting,
            "missing advisor must not forfeit the standby replacement"
        );
        assert_eq!(
            replacement_cause(None, false),
            crate::error_page::ErrorPageCause::Generic,
            "non-standby without confirmation stays generic"
        );
        // standby + 确认来源但观察为 Generic（无细分证据）：兜底 Restarting。
        let generic_readiness = crate::error_page::UserAppProxyFailureHint {
            readiness_status: None,
            error_origin_confirmed: true,
            proxy_confirmation: None,
        };
        assert_eq!(
            replacement_cause(Some(&generic_readiness), true),
            crate::error_page::ErrorPageCause::Restarting,
            "standby without refinement evidence falls back to restarting copy"
        );
        // 停止窗口（2026-10-27 缺陷单）：advisor 观察到 Stopping 时保留停止
        // 档文案，不被 standby 的 Restarting 兜底升格。
        let stopping_readiness = crate::error_page::UserAppProxyFailureHint {
            readiness_status: Some(shared_types::UserAppReadinessStatus::Stopping),
            error_origin_confirmed: true,
            proxy_confirmation: None,
        };
        assert_eq!(
            replacement_cause(Some(&stopping_readiness), true),
            crate::error_page::ErrorPageCause::Stopping,
            "observed stopping evidence must keep the stopping copy in standby windows"
        );
        assert_eq!(
            replacement_cause(Some(&stopping_readiness), false),
            crate::error_page::ErrorPageCause::Stopping,
        );
    }

    #[test]
    fn only_fresh_transition_supplies_native_operation_identity() {
        let mut ctx = TrackingCtx::new();
        ctx.userapp_route = Some(crate::service::types::UserAppRouteCtx {
            app_id: "native-app".into(),
            stage: "dev",
        });
        ctx.failure_hint = Some(crate::error_page::UserAppProxyFailureHint {
            readiness_status: Some(shared_types::UserAppReadinessStatus::Starting),
            error_origin_confirmed: false,
            proxy_confirmation: Some(shared_types::ProxyConfirmation {
                contract: shared_types::ProxyConfirmationContract::OfficialRestartHttpV1,
                confirmed: false,
                phase: shared_types::ProxyPhase::Stopping,
                operation_id: "native-exact-operation".into(),
                proxy_instance_id: None,
                config_digest: None,
            }),
        });
        PortProxy::capture_proxy_transition(&mut ctx);
        assert!(
            ctx.wake_failure.is_none(),
            "missing observation age cannot be fresh evidence"
        );
        ctx.failure_hint = Some(crate::error_page::UserAppProxyFailureHint {
            readiness_status: Some(shared_types::UserAppReadinessStatus::Starting),
            error_origin_confirmed: false,
            proxy_confirmation: Some(shared_types::ProxyConfirmation {
                contract: shared_types::ProxyConfirmationContract::OfficialRestartHttpV1,
                confirmed: false,
                phase: shared_types::ProxyPhase::Starting,
                operation_id: "native-exact-operation".into(),
                proxy_instance_id: None,
                config_digest: None,
            }),
        });
        ctx.failure_hint_at = Some(tokio::time::Instant::now());
        PortProxy::capture_proxy_transition(&mut ctx);
        assert_eq!(
            ctx.wake_failure.as_ref().unwrap().operation_id.as_deref(),
            Some("native-exact-operation")
        );
        let original = shared_types::WakeFailure::timeout(
            "wake_observation",
            Some("parent-wake-operation".into()),
            true,
        );
        ctx.wake_failure = Some(original);
        PortProxy::capture_proxy_transition(&mut ctx);
        assert_eq!(
            ctx.wake_failure.as_ref().unwrap().operation_id.as_deref(),
            Some("parent-wake-operation"),
            "native child must never overwrite an actual parent wake identity"
        );
    }
}
