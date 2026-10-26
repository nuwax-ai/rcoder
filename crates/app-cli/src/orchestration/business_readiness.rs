//! 业务就绪观察器：只读回答「当前实例内，声明的服务集合 + Pingap 入口是否
//! 满足健康契约」，与容器探针（`/health`、`/ready`）严格分离。
//!
//! ## 与既有健康面的分工（Spec）
//! - `RuntimeStatusService`（bool）是编排探针面：`/ready` 消费，本模块**只读**
//!   不回写（探测结果绝不调用 `set_ready`）。
//! - 本模块按 `release.lock` 声明的服务契约做 HTTP 探测 + Pingap admin 只读
//!   快照，推导 `UserAppBusinessReadiness`（shared_types 单一事实源，rcoder
//!   `/{app_id}/{app_stage}/readiness` 直接消费）。
//!
//! ## 只读纪律
//! 不启动/停止任何进程，不触碰 supervisord 写端点，不等待启动轮询 helper
//! （那些有 25/120s 预算）；单轮探测自带 3s 总预算、单项 2s，预算耗尽把
//! 未完成项记 `unknown`（不折算成失败）。
//!
//! ## 观察身份与失效
//! 观察序号由进程内状态指纹（相位/release/ready/期望代理 hash/当前操作）
//! 单调派生；换代事件天然使指纹变化 → 序号推进 → 缓存失效。探测前后各读
//! 一次指纹，不一致丢弃本轮结果（预算允许重试一次），避免迟到探测把旧
//! ready 写进新代。

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use shared_types::{
    UserAppBusinessReadiness, UserAppProxyReadiness, UserAppReadinessReason,
    UserAppReadinessStatus, UserAppServiceReadiness,
};
use workspace_manifest::ProjectKind;

use crate::proxy::admin_probe;
use crate::server::{ServerPhase, ServerState};

/// 单项 HTTP/TCP 探测超时。
const PROBE_ITEM_TIMEOUT: Duration = Duration::from_secs(2);
/// 单轮观察总预算（超预算的未完成项 → unknown，不算失败）。
const OBSERVE_BUDGET: Duration = Duration::from_secs(3);
/// 并发探测上限。
const PROBE_CONCURRENCY: usize = 8;
/// 相同观察的短缓存（合并并发查询；指纹变化立即失效）。
const CACHE_TTL: Duration = Duration::from_secs(1);
type PhysicalVerifier = for<'a> fn(
    &'a crate::proxy::apply_status::ConfirmedPublication,
    tokio::time::Instant,
) -> futures::future::BoxFuture<'a, anyhow::Result<()>>;

fn verify_owned_physical(
    receipt: &crate::proxy::apply_status::ConfirmedPublication,
    deadline: tokio::time::Instant,
) -> futures::future::BoxFuture<'_, anyhow::Result<()>> {
    Box::pin(crate::proxy::observation::verify_physical(
        receipt, deadline,
    ))
}

/// 进程内状态指纹：任何应使旧观察失效的变化都会改变指纹。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservationFingerprint {
    phase: String,
    release_id: Option<String>,
    business_ready_flag: bool,
    initializing: bool,
    expected_proxy_hash: Option<String>,
    applied_publication: Option<crate::proxy::apply_status::ConfirmedPublication>,
    proxy_confirmation: Option<shared_types::ProxyConfirmation>,
    publication_uncertain: bool,
    current_operation: Option<String>,
    dev_profile: Option<bool>,
}

impl ObservationFingerprint {
    fn capture(server: &ServerState) -> Self {
        let phase = match server.phase() {
            ServerPhase::Idle => "idle",
            ServerPhase::Deploying => "deploying",
            ServerPhase::Orchestrating => "orchestrating",
            ServerPhase::Running => "running",
            ServerPhase::Failed(_) => "failed",
        };
        Self {
            phase: phase.to_string(),
            release_id: server.release().map(|release| release.release_id),
            business_ready_flag: server.runtime_status().is_ready(),
            initializing: server.initializing(),
            expected_proxy_hash: crate::proxy::compiler::expected_hash(),
            applied_publication: crate::proxy::compiler::confirmed_publication()
                .map(|(_, confirmed)| confirmed),
            proxy_confirmation: crate::proxy::compiler::official_confirmation(),
            publication_uncertain: crate::proxy::compiler::publication_uncertain(),
            current_operation: server.current_runtime_operation(),
            dev_profile: server.proxy_context().map(|context| context.dev_profile),
        }
    }
}

struct CachedReadiness {
    fingerprint: ObservationFingerprint,
    revision: u64,
    at: std::time::Instant,
    result: Arc<UserAppBusinessReadiness>,
}

/// 业务就绪观察器（每个 app-cli 进程一个，由 api 装配持有）。
pub struct BusinessReadinessObserver {
    server: Arc<ServerState>,
    /// 串行化探测轮次：并发查询合并为一次探测（后到者命中新鲜缓存）。
    probe_gate: tokio::sync::Mutex<()>,
    cache: std::sync::Mutex<Option<CachedReadiness>>,
    /// 进程内单调观察序号（指纹每变化一次 +1；跨调用持久，不随缓存清除回落）。
    revision: std::sync::atomic::AtomicU64,
    physical_verifier: PhysicalVerifier,
}

impl BusinessReadinessObserver {
    pub fn new(server: Arc<ServerState>) -> Self {
        Self {
            server,
            probe_gate: tokio::sync::Mutex::new(()),
            cache: std::sync::Mutex::new(None),
            revision: std::sync::atomic::AtomicU64::new(0),
            physical_verifier: verify_owned_physical,
        }
    }

    /// 观察序号推进：指纹变化时递增。探测前后各取一次用于换代检测。
    fn current_revision(&self, fingerprint: &ObservationFingerprint) -> u64 {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.as_ref() {
            Some(cached) if &cached.fingerprint == fingerprint => cached.revision,
            _ => {
                // 指纹变化：清缓存 + 原子计数器前进一步（单调，不回落）。
                *guard = None;
                self.revision
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                    + 1
            }
        }
    }

    /// 单轮业务就绪观察（缓存 + 并发合并）。
    pub async fn observe(&self) -> Arc<UserAppBusinessReadiness> {
        // 快路径：新鲜缓存直接命中（无锁竞争下的高频轮询友好）。
        if let Some(result) = self.fresh_cached() {
            return result;
        }
        // 合并并发：同一时刻只跑一轮探测；拿到锁后先再看缓存（前一个完成者已刷新）。
        let _guard = self.probe_gate.lock().await;
        if let Some(result) = self.fresh_cached() {
            return result;
        }
        let (fingerprint, result) = self.probe_round().await;
        let result = Arc::new(result);
        self.store_cache(fingerprint, result.clone());
        result
    }

    fn fresh_cached(&self) -> Option<Arc<UserAppBusinessReadiness>> {
        let fingerprint = ObservationFingerprint::capture(&self.server);
        let guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.as_ref() {
            Some(cached)
                if cached.fingerprint == fingerprint && cached.at.elapsed() < CACHE_TTL =>
            {
                Some(cached.result.clone())
            }
            _ => None,
        }
    }

    fn store_cache(
        &self,
        fingerprint: ObservationFingerprint,
        result: Arc<UserAppBusinessReadiness>,
    ) {
        // Store the identity actually observed. Recapturing here could label a
        // completed old Ready result with a new proxy incarnation's fingerprint.
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let revision = result.observation_revision;
        *guard = Some(CachedReadiness {
            fingerprint,
            revision,
            at: std::time::Instant::now(),
            result,
        });
    }

    /// 无网络 I/O 的探测轮：真实探测 + 前后指纹复核。
    ///
    /// 两轮共用一个总 deadline（换代重试不额外放大预算，保证上层 rcoder 的
    /// 通道超时（直连 4s/exec 6s）不会被第二轮击穿成 ADMIN_UNREACHABLE）。
    async fn probe_round(&self) -> (ObservationFingerprint, UserAppBusinessReadiness) {
        let fingerprint_before = ObservationFingerprint::capture(&self.server);
        let revision = self.current_revision(&fingerprint_before);
        let deadline = tokio::time::Instant::now() + OBSERVE_BUDGET;
        let mut result = Self::probe_once(&self.server, deadline, self.physical_verifier).await;
        let fingerprint_after = ObservationFingerprint::capture(&self.server);
        let mut final_revision = revision;
        let mut final_fingerprint = fingerprint_before.clone();
        if fingerprint_after != fingerprint_before {
            // 换代窗口：剩余预算内快速重试一次，仍不稳定则按 INSTANCE_CHANGED 丢弃。
            let retry = Self::probe_once(&self.server, deadline, self.physical_verifier).await;
            let fingerprint_retry = ObservationFingerprint::capture(&self.server);
            if fingerprint_retry == fingerprint_after {
                result = retry;
                // 探测期间发生了换代：观察序号按换代后指纹重算（单调推进，
                // 不得把换代前后的两代标成同一序号）。
                final_revision = self.current_revision(&fingerprint_retry);
                final_fingerprint = fingerprint_retry;
            } else {
                tracing::warn!(
                    "business readiness observation discarded: instance transition during probe"
                );
                return (
                    fingerprint_retry,
                    UserAppBusinessReadiness {
                        ready: false,
                        status: UserAppReadinessStatus::Unknown,
                        reason_code: Some(UserAppReadinessReason::InstanceChanged),
                        checked_at: now_rfc3339(),
                        runtime_instance_id: runtime_instance_id(&self.server),
                        serving_release_id: None,
                        target_release_id: None,
                        operation_id: self.server.current_runtime_operation(),
                        observation_revision: revision,
                        proxy: UserAppProxyReadiness {
                            ready: false,
                            status: UserAppReadinessStatus::Unknown,
                            reason_code: Some(UserAppReadinessReason::InstanceChanged),
                            error_origin_contract: None,
                            confirmation: None,
                        },
                        services: Vec::new(),
                    },
                );
            }
        }
        result.observation_revision = final_revision;
        (final_fingerprint, result)
    }

    /// 一次完整观察：状态机短路 → 并发服务探测 → Pingap admin 快照 → 集中推导。
    async fn probe_once(
        server: &Arc<ServerState>,
        deadline: tokio::time::Instant,
        physical: PhysicalVerifier,
    ) -> UserAppBusinessReadiness {
        let checked_at = now_rfc3339();
        let instance_id = runtime_instance_id(server);
        let operation_id = server.current_runtime_operation();
        let phase = server.phase();
        let release = server.release();
        let initializing = server.initializing();
        let proxy_confirmation = crate::proxy::compiler::official_confirmation();

        // 初始化恢复期（P1-01）：API 已 bind 但 kernel/恢复未完成——一切业务
        // 观察都是过早的。初始化不需要等完，但不能宣称任何业务事实。
        if initializing {
            return UserAppBusinessReadiness {
                ready: false,
                status: UserAppReadinessStatus::Starting,
                reason_code: Some(UserAppReadinessReason::ServiceStarting),
                checked_at,
                runtime_instance_id: instance_id,
                serving_release_id: None,
                target_release_id: None,
                operation_id,
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status: UserAppReadinessStatus::Starting,
                    reason_code: Some(UserAppReadinessReason::ProxyNotStarted),
                    error_origin_contract: None,
                    confirmation: None,
                },
                services: Vec::new(),
            };
        }

        let Some(release) = release else {
            // 从未部署过任何 release：空 owner 空态。
            return UserAppBusinessReadiness {
                ready: false,
                status: UserAppReadinessStatus::NotDeployed,
                reason_code: None,
                checked_at,
                runtime_instance_id: instance_id,
                serving_release_id: None,
                target_release_id: None,
                operation_id,
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status: UserAppReadinessStatus::NotDeployed,
                    reason_code: None,
                    error_origin_contract: None,
                    confirmation: None,
                },
                services: Vec::new(),
            };
        };

        // 停止意图已受理且尚未收束（内核中存在 admitted、非终态的 Stop 运行
        // 操作）：整个停机窗口（standby 发布 → 业务停止 → 终态持久化）顶层
        // 一律 stopping/STOP_ACCEPTED。先于 Idle 和代理换代判断：Idle 可能
        // 只是恢复驱动者停驻，不能证明未终态 Stop 已完成清理；standby 发布
        // 本身也是一次 official 换代事务。Restart/热部署没有 Stop 受理，
        // 不受影响。扫描失败则无法确认是否有未收束停止，显式返回 unknown，
        // 不借用旧 ready/stopped 结果；观察本身不恢复或改写记录。
        let stop_observation =
            server
                .runtime_kernel()
                .and_then(|kernel| match kernel.outstanding_stop_operations() {
                    Ok(stops) => stops.into_iter().min().map(|id| {
                        (
                            UserAppReadinessStatus::Stopping,
                            UserAppReadinessReason::StopAccepted,
                            Some(id),
                        )
                    }),
                    Err(error) => {
                        tracing::debug!(
                            "business readiness: outstanding stop scan failed: {error:#}"
                        );
                        Some((
                            UserAppReadinessStatus::Unknown,
                            UserAppReadinessReason::ObserveIncomplete,
                            operation_id.clone(),
                        ))
                    }
                });
        if let Some((status, reason, operation_id)) = stop_observation {
            return UserAppBusinessReadiness {
                ready: false,
                status,
                reason_code: Some(reason),
                checked_at,
                runtime_instance_id: instance_id,
                serving_release_id: None,
                target_release_id: None,
                operation_id,
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status,
                    reason_code: Some(reason),
                    // 停止窗口入口仍是 standby mock（先摘流后停业务）：附加
                    // 已核验的来源契约，故障页管线才能替换停止中/已停止文案
                    error_origin_contract: standby_origin_contract(proxy_confirmation.as_ref()),
                    confirmation: proxy_confirmation,
                },
                services: Vec::new(),
            };
        }

        // 无未收束 Stop 后，Idle + ready=false 才能视为确认停止。
        // 正常 StopBusiness 先持久化终态，再回 Idle；此处无需网络探测。
        if matches!(phase, ServerPhase::Idle) && !server.runtime_status().is_ready() {
            return UserAppBusinessReadiness {
                ready: false,
                status: UserAppReadinessStatus::Stopped,
                reason_code: None,
                checked_at,
                runtime_instance_id: instance_id,
                serving_release_id: None,
                target_release_id: None,
                operation_id,
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status: UserAppReadinessStatus::Stopped,
                    reason_code: None,
                    error_origin_contract: standby_origin_contract(proxy_confirmation.as_ref()),
                    confirmation: None,
                },
                services: Vec::new(),
            };
        }

        // A frozen official replacement cannot borrow the previous serving
        // observation while its owned process is stopping or being verified.
        if let Some(status) = proxy_transition_status(proxy_confirmation.as_ref()) {
            let reason = match status {
                UserAppReadinessStatus::Starting => UserAppReadinessReason::ServiceStarting,
                UserAppReadinessStatus::Failed => UserAppReadinessReason::OrchestrationFailed,
                UserAppReadinessStatus::NotDeployed
                | UserAppReadinessStatus::Provisioning
                | UserAppReadinessStatus::Stopping
                | UserAppReadinessStatus::Stopped
                | UserAppReadinessStatus::Ready
                | UserAppReadinessStatus::Degraded
                | UserAppReadinessStatus::Unknown
                | UserAppReadinessStatus::Unsupported => UserAppReadinessReason::ObserveIncomplete,
            };
            return UserAppBusinessReadiness {
                ready: false,
                status,
                reason_code: Some(reason),
                checked_at,
                runtime_instance_id: instance_id,
                serving_release_id: None,
                target_release_id: Some(release.release_id),
                operation_id: proxy_confirmation
                    .as_ref()
                    .map(|confirmation| confirmation.operation_id.clone())
                    .or(operation_id),
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status,
                    reason_code: Some(reason),
                    error_origin_contract: None,
                    confirmation: proxy_confirmation,
                },
                services: Vec::new(),
            };
        }

        // 参与汇总集合：enabled + kind=web + 声明 [proxy]（worker/disabled 排除；
        // 排除后为空 → unsupported/NO_PROXIED_WEB_SERVICES，不表示启动失败）。
        let dev_profile = server
            .proxy_context()
            .map(|context| context.dev_profile)
            .unwrap_or_else(crate::supervisor::dev_run_profile);
        let summary: Vec<workspace_manifest::LockedService> = release
            .services
            .iter()
            .filter(|service| {
                service.enabled && service.kind == ProjectKind::Web && service.proxy.is_some()
            })
            .cloned()
            .collect();

        if summary.is_empty() {
            return UserAppBusinessReadiness {
                ready: false,
                status: UserAppReadinessStatus::Unsupported,
                reason_code: Some(UserAppReadinessReason::NoProxiedWebServices),
                checked_at,
                runtime_instance_id: instance_id,
                serving_release_id: None,
                target_release_id: None,
                operation_id,
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status: UserAppReadinessStatus::Unsupported,
                    reason_code: Some(UserAppReadinessReason::NoProxiedWebServices),
                    error_origin_contract: None,
                    confirmation: None,
                },
                services: Vec::new(),
            };
        }

        // custom 代理模式：平台不生成路由，无法建立「声明服务 ↔ 实际路由」映射。
        if release.pingap.mode == workspace_manifest::PingapMode::Custom {
            return UserAppBusinessReadiness {
                ready: false,
                status: UserAppReadinessStatus::Unsupported,
                reason_code: Some(UserAppReadinessReason::CustomRouteUnverified),
                checked_at,
                runtime_instance_id: instance_id,
                serving_release_id: None,
                target_release_id: None,
                operation_id,
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status: UserAppReadinessStatus::Unsupported,
                    reason_code: Some(UserAppReadinessReason::CustomRouteUnverified),
                    error_origin_contract: None,
                    confirmation: None,
                },
                services: Vec::new(),
            };
        }

        // ── 真实探测：服务 HTTP（并发 ≤8）+ Pingap admin 快照 + 入口 TCP ──
        // 总预算由调用方 deadline 给定（探测轮内两轮共用；单项另受 2s 上限），
        // 预算耗尽的未完成项记 unknown（不折算成失败，也不从汇总集合消失）。
        let (admin_snapshot, publication, entry_reachable) = tokio::join!(
            fetch_admin_snapshot(deadline),
            verify_publication(deadline, physical),
            async {
                tokio::time::timeout(
                    item_budget(deadline),
                    tokio::net::TcpStream::connect((
                        "127.0.0.1",
                        crate::proxy::pingap::PINGAP_PORT,
                    )),
                )
                .await
                .map(|connected| connected.is_ok())
                .unwrap_or(false)
            }
        );

        let probes = futures::stream::iter(
            summary
                .iter()
                .cloned()
                .map(|service| (service, admin_snapshot.clone()))
                .collect::<Vec<_>>(),
        )
        .map(|(service, admin)| async move {
            probe_service(&service, dev_profile, admin.as_ref(), deadline).await
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect::<Vec<UserAppServiceReadiness>>()
        .await;
        let mut services = probes;
        services.sort_by(|a, b| a.service_id.cmp(&b.service_id));

        // 代理就绪：入口监听 + 生效 hash 匹配（upstream 健康归各服务结果）。
        let (proxy_ready, proxy_status, proxy_reason, origin_contract) = derive_proxy(
            admin_snapshot.as_ref(),
            entry_reachable,
            phase.clone(),
            publication.0,
        );

        let target_release_id = target_release_hint(server, publication.1.as_deref().unwrap_or(""));

        let readiness = derive_top_level(phase.clone(), &services, proxy_ready, proxy_status);
        UserAppBusinessReadiness {
            ready: readiness.is_ready(),
            status: readiness,
            reason_code: top_level_reason(readiness, &services),
            checked_at,
            runtime_instance_id: instance_id,
            serving_release_id: if readiness.is_ready() {
                publication.1
            } else {
                None
            },
            target_release_id,
            operation_id,
            observation_revision: 0,
            proxy: UserAppProxyReadiness {
                ready: proxy_ready,
                status: proxy_status,
                reason_code: proxy_reason,
                error_origin_contract: origin_contract,
                confirmation: proxy_confirmation.map(|mut confirmation| {
                    confirmation.confirmed &= proxy_ready;
                    confirmation
                }),
            },
            services,
        }
    }
}

/// RuntimeKernel 进程身份（每次启动重新生成）；legacy run 无内核为 None。
fn runtime_instance_id(server: &ServerState) -> Option<String> {
    server
        .runtime_kernel()
        .map(|kernel| kernel.identity().runtime_instance_id.clone())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 准备中的目标版本（热部署请求 token；与当前 serving 不同才有意义）。
fn target_release_hint(server: &ServerState, current: &str) -> Option<String> {
    match server.phase() {
        ServerPhase::Deploying | ServerPhase::Orchestrating => {}
        ServerPhase::Idle | ServerPhase::Running | ServerPhase::Failed(_) => return None,
    }
    let operation = server.deploy_status().operation?;
    match operation.phase {
        shared_types::AppCliDeployPhase::Deploying
        | shared_types::AppCliDeployPhase::Orchestrating => {}
        shared_types::AppCliDeployPhase::Idle
        | shared_types::AppCliDeployPhase::Running
        | shared_types::AppCliDeployPhase::Failed => return None,
    }
    if let Some(active) = server.current_runtime_operation()
        && active != operation.operation_id
    {
        return None;
    }
    (!operation.request_release_id.trim().is_empty() && operation.request_release_id != current)
        .then_some(operation.request_release_id)
}

/// 单服务探测：HTTP 健康契约（+ 静态入口核验 + upstream 可用性）。
#[allow(clippy::too_many_lines)]
async fn probe_service(
    service: &workspace_manifest::LockedService,
    dev_profile: bool,
    admin: Option<&admin_probe::AdminBasicSnapshot>,
    deadline: tokio::time::Instant,
) -> UserAppServiceReadiness {
    let readiness_path = if service.health.readiness_path.is_empty() {
        "/health".to_string()
    } else {
        service.health.readiness_path.clone()
    };
    let url = format!("http://127.0.0.1:{}{readiness_path}", service.port);

    let mut failure: Option<(UserAppReadinessStatus, UserAppReadinessReason)> = None;

    // 1) HTTP 健康契约（2xx 满足；不跟随重定向、不解析正文）。
    let budget = item_budget(deadline);
    if budget.is_zero() {
        return UserAppServiceReadiness {
            service_id: service.service_id.clone(),
            ready: false,
            status: UserAppReadinessStatus::Unknown,
            reason_code: Some(UserAppReadinessReason::ObserveIncomplete),
        };
    }
    let http_ok = match tokio::time::timeout(budget, http_probe(&url, budget)).await {
        Ok(Ok(true)) => true,
        Ok(Ok(false)) => {
            failure = Some((
                UserAppReadinessStatus::Degraded,
                UserAppReadinessReason::HealthHttpFailure,
            ));
            false
        }
        Ok(Err(_connect_error)) => {
            // 端口未监听：编排语义上是「尚未起来」而非「退化」。
            failure = Some((
                UserAppReadinessStatus::Starting,
                UserAppReadinessReason::ServiceStarting,
            ));
            false
        }
        Err(_elapsed) => {
            failure = Some((
                UserAppReadinessStatus::Unknown,
                UserAppReadinessReason::ProbeTimeout,
            ));
            false
        }
    };

    // 2) 静态托管补强：/health 恒 200 不能证明产物可服务——默认健康路径下
    //    额外核验静态根入口存在；devrun 不要求 dist（ Spec：devrun 按 HTTP 检查）。
    //    reconcile 进行中（try_lock 失败）时跳过该补强，下一轮轮询再核。
    if http_ok
        && crate::static_hosting::hosts_statically(service, dev_profile)
        && readiness_path == "/health"
        && !static_entry_available(service.port).await
    {
        failure = Some((
            UserAppReadinessStatus::Starting,
            UserAppReadinessReason::StaticEntryUnavailable,
        ));
    }

    // 3) 映射 upstream 可用性（平台生成的 managed/extend 配置才有映射）。
    if failure.is_none()
        && let Some(admin) = admin
        && let Some(upstream) = admin.upstreams.get(&service.service_id)
        && upstream.total > 0
        && upstream.healthy == 0
    {
        // 服务自身 HTTP 通过但 Pingap 尚未放行：该服务整体仍未就绪。
        failure = Some((
            UserAppReadinessStatus::Starting,
            UserAppReadinessReason::UpstreamUnhealthy,
        ));
    }

    match failure {
        None => UserAppServiceReadiness {
            service_id: service.service_id.clone(),
            ready: true,
            status: UserAppReadinessStatus::Ready,
            reason_code: None,
        },
        Some((status, reason)) => UserAppServiceReadiness {
            service_id: service.service_id.clone(),
            ready: false,
            status,
            reason_code: Some(reason),
        },
    }
}

/// 一次性 HTTP 探测：2xx=true、非 2xx=false、连接/请求错误=Err。
async fn http_probe(url: &str, budget: Duration) -> anyhow::Result<bool> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(budget)
        .timeout(budget)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client.get(url).send().await?;
    Ok(response.status().is_success())
}

/// 单项预算：总预算剩余量与单项上限取小（零 = 总预算已耗尽）。
fn item_budget(deadline: tokio::time::Instant) -> Duration {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    remaining.min(PROBE_ITEM_TIMEOUT)
}

/// 静态托管根入口可服务性（live root 的 index 入口存在且可读；
/// reconcile 进行中返回 true = 跳过补强，不误判未就绪）。
async fn static_entry_available(port: u16) -> bool {
    match crate::static_hosting::hosted_root(port).await {
        Some(root) => root.join("index.html").is_file(),
        None => true,
    }
}

/// Pingap admin 只读快照（未注册端点/不可达返回 None——推导层归类）。
async fn verify_publication(
    deadline: tokio::time::Instant,
    physical: PhysicalVerifier,
) -> (bool, Option<String>) {
    if crate::proxy::compiler::publication_uncertain() {
        return (false, None);
    }
    let Some((outcome, expected)) = crate::proxy::compiler::confirmed_publication() else {
        return (false, None);
    };
    let Some(confirmation) = crate::proxy::compiler::official_confirmation() else {
        return (false, None);
    };
    if !confirmation.confirmed
        || confirmation.proxy_instance_id.as_deref() != Some(expected.instance_id.as_str())
        || confirmation.config_digest.as_deref() != Some(outcome.config_digest.as_str())
        || !matches!(confirmation.phase, shared_types::ProxyPhase::Serving)
    {
        return (false, None);
    }
    let Some(endpoint) = admin_probe::admin_endpoint() else {
        return (false, None);
    };
    if physical(&expected, deadline).await.is_err() {
        return (false, None);
    }
    match admin_probe::wait_for_official_publication(
        endpoint,
        &outcome,
        expected.process_id,
        &expected.instance_id,
        item_budget(deadline),
    )
    .await
    {
        Ok(actual) => {
            let verified = actual.process_id == expected.process_id
                && actual.instance_id == expected.instance_id
                && physical(&expected, deadline).await.is_ok()
                && crate::proxy::compiler::official_confirmation().as_ref() == Some(&confirmation);
            (
                verified,
                if verified {
                    outcome.serving_release_id
                } else {
                    None
                },
            )
        }
        Err(error) => {
            tracing::debug!("business readiness: complete publication unconfirmed: {error:#}");
            (false, None)
        }
    }
}

fn proxy_transition_status(
    confirmation: Option<&shared_types::ProxyConfirmation>,
) -> Option<UserAppReadinessStatus> {
    let confirmation = confirmation?;
    use shared_types::ProxyPhase;
    match confirmation.phase {
        ProxyPhase::Preparing
        | ProxyPhase::Stopping
        | ProxyPhase::Starting
        | ProxyPhase::Verifying => Some(UserAppReadinessStatus::Starting),
        ProxyPhase::Failed => Some(UserAppReadinessStatus::Failed),
        ProxyPhase::Absent | ProxyPhase::RecoveryRequired => Some(UserAppReadinessStatus::Unknown),
        ProxyPhase::Serving | ProxyPhase::Standby => None,
    }
}

async fn fetch_admin_snapshot(
    deadline: tokio::time::Instant,
) -> Option<admin_probe::AdminBasicSnapshot> {
    let endpoint = admin_probe::admin_endpoint()?;
    match tokio::time::timeout(
        item_budget(deadline),
        admin_probe::fetch_admin_basic(endpoint),
    )
    .await
    {
        Ok(Ok(snapshot)) => Some(snapshot),
        Ok(Err(error)) => {
            tracing::debug!("business readiness: pingap admin snapshot unavailable: {error:#}");
            None
        }
        Err(_) => {
            tracing::debug!("business readiness: pingap admin snapshot timed out");
            None
        }
    }
}

/// standby 摘流窗口的来源契约（停止短路路径专用）。
///
/// 证据链：[`crate::proxy::compiler::record_confirmed_publication`] 只在
/// admin 回报 hash 与 standby 期望一致、全部入口探针按 503 通过后写入
/// confirmed Standby 相位（同时 pin 实例与配置摘要）；停止受理/已停止
/// 短路不重发网络探测，直接消费该已核验记录。相位被后续发布事务覆盖
/// （Preparing/Starting/Serving）后本函数自然失效——那时的入口要么仍在
/// 旧 standby（短窗口，走全量探测路径由 `derive_proxy` 核验），要么已换代。
fn standby_origin_contract(
    confirmation: Option<&shared_types::ProxyConfirmation>,
) -> Option<String> {
    let confirmed_standby = confirmation.is_some_and(|confirmation| {
        confirmation.confirmed && matches!(confirmation.phase, shared_types::ProxyPhase::Standby)
    });
    confirmed_standby.then(|| shared_types::PINGAP_ETYPE_ORIGIN_CONTRACT.to_string())
}

/// 代理就绪推导：入口监听 + 生效 hash 匹配；来源契约仅在 hash 确认时透出。
#[allow(clippy::type_complexity)]
fn derive_proxy(
    admin: Option<&admin_probe::AdminBasicSnapshot>,
    entry_reachable: bool,
    phase: ServerPhase,
    publication_confirmed: bool,
) -> (
    bool,
    UserAppReadinessStatus,
    Option<UserAppReadinessReason>,
    Option<String>,
) {
    // orchestrating 早期 pingap 尚未启动属预期；只有运行后不可达才算异常。
    let expects_proxy = matches!(
        phase,
        ServerPhase::Running | ServerPhase::Deploying | ServerPhase::Orchestrating
    );
    if !entry_reachable {
        let status = if matches!(phase, ServerPhase::Running | ServerPhase::Deploying) {
            UserAppReadinessStatus::Degraded
        } else {
            UserAppReadinessStatus::Starting
        };
        return (
            false,
            status,
            Some(UserAppReadinessReason::ProxyNotStarted),
            None,
        );
    }
    let Some(admin) = admin else {
        return (
            false,
            if expects_proxy {
                UserAppReadinessStatus::Degraded
            } else {
                UserAppReadinessStatus::Starting
            },
            Some(UserAppReadinessReason::ProxyNotStarted),
            None,
        );
    };
    let expected = crate::proxy::compiler::expected_hash();
    let Some(expected) = expected else {
        // 本进程尚未记录期望 hash（理论仅出现在启动窗口）——观察不完整。
        return (
            false,
            UserAppReadinessStatus::Unknown,
            Some(UserAppReadinessReason::ObserveIncomplete),
            None,
        );
    };
    let Some(actual) = admin.config_hash.clone() else {
        return (
            false,
            UserAppReadinessStatus::Unknown,
            Some(UserAppReadinessReason::ObserveIncomplete),
            None,
        );
    };
    if !admin_probe::hashes_match(&expected, &actual) {
        return (
            false,
            UserAppReadinessStatus::Degraded,
            Some(UserAppReadinessReason::ProxyConfigMismatch),
            None,
        );
    }
    // standby 摘流窗口：入口 mock 直出全部请求（无 upstream 代理），响应来源
    // 全部为 Pingap 自产——来源契约成立，透出给故障页管线做友好替换；业务
    // 状态细分（starting/stopped/failed）由顶层推导与各服务观察给出。
    if crate::proxy::compiler::official_confirmation().is_some_and(|confirmation| {
        confirmation.confirmed && matches!(confirmation.phase, shared_types::ProxyPhase::Standby)
    }) {
        return (
            false,
            UserAppReadinessStatus::Unknown,
            Some(UserAppReadinessReason::ProxyStandby),
            Some(shared_types::PINGAP_ETYPE_ORIGIN_CONTRACT.to_string()),
        );
    }
    if !publication_confirmed {
        return (
            false,
            UserAppReadinessStatus::Unknown,
            Some(UserAppReadinessReason::ObserveIncomplete),
            None,
        );
    }
    (
        true,
        UserAppReadinessStatus::Ready,
        None,
        Some(shared_types::PINGAP_ETYPE_ORIGIN_CONTRACT.to_string()),
    )
}

/// 顶层状态推导（Plan §4.3 判定顺序的纯函数）。
fn derive_top_level(
    phase: ServerPhase,
    services: &[UserAppServiceReadiness],
    proxy_ready: bool,
    proxy_status: UserAppReadinessStatus,
) -> UserAppReadinessStatus {
    let all_ready =
        !services.is_empty() && services.iter().all(|service| service.ready) && proxy_ready;
    // 服务集合完整健康且代理可用 → ready（即使新版本在准备/最近部署失败，
    // 旧服务仍完整服务；操作结果与业务可用分开呈现）。
    if all_ready {
        return UserAppReadinessStatus::Ready;
    }
    match phase {
        ServerPhase::Failed(_) => {
            // 有明确失败证据；服务探测已确认没有可用的旧集合（all_ready 不成立）。
            UserAppReadinessStatus::Failed
        }
        ServerPhase::Running | ServerPhase::Deploying => {
            // 已进入服务态后检查不过：要么健康退化，要么（从未 ready 过的
            // 运行窗口）仍在启动。以服务探测结果区分。
            let any_started = services.iter().any(|service| {
                matches!(
                    service.reason_code,
                    Some(UserAppReadinessReason::HealthHttpFailure)
                        | Some(UserAppReadinessReason::UpstreamUnhealthy)
                        | Some(UserAppReadinessReason::StaticEntryUnavailable)
                ) || service.ready
            });
            if any_started || proxy_status == UserAppReadinessStatus::Degraded {
                UserAppReadinessStatus::Degraded
            } else {
                UserAppReadinessStatus::Starting
            }
        }
        ServerPhase::Orchestrating => UserAppReadinessStatus::Starting,
        ServerPhase::Idle => {
            // Idle + 有 release 且非确认停止组合（停止收束瞬间的过渡窗）。
            UserAppReadinessStatus::Stopped
        }
    }
}

/// An orchestration failure is authoritative even if services never started.
/// Preserve those service observations below, not as a contradictory top reason.
fn top_level_reason(
    status: UserAppReadinessStatus,
    services: &[UserAppServiceReadiness],
) -> Option<UserAppReadinessReason> {
    let fallback = match status {
        UserAppReadinessStatus::Ready => return None,
        UserAppReadinessStatus::Failed => return Some(UserAppReadinessReason::OrchestrationFailed),
        UserAppReadinessStatus::Degraded => Some(UserAppReadinessReason::HealthDegraded),
        UserAppReadinessStatus::NotDeployed
        | UserAppReadinessStatus::Provisioning
        | UserAppReadinessStatus::Starting
        | UserAppReadinessStatus::Stopping
        | UserAppReadinessStatus::Stopped
        | UserAppReadinessStatus::Unknown
        | UserAppReadinessStatus::Unsupported => None,
    };
    services
        .iter()
        .find(|service| !service.ready)
        .and_then(|service| service.reason_code)
        .or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// standby 摘流确认后：hash 匹配即透出来源契约（故障页管线据此信任
    /// `X-Pingap-EType: rcoder-standby`），代理不宣称 ready，原因标 PROXY_STANDBY。
    #[test]
    fn standby_confirmation_exposes_error_origin_contract() {
        let publication_id = "8204e27d-a7af-42f9-aa92-0b69964b8df2";
        let expected_hash = "STANDBYHASH01".to_string();
        let outcome = crate::proxy::compiler::CompileOutcome {
            serving_release_id: None,
            operation_id: Some("standby-operation".into()),
            config_path: "unused-standby-candidate.toml".into(),
            expected_hash: expected_hash.clone(),
            publication_id: publication_id.into(),
            config_digest: "b".repeat(64),
            entry_probes: vec![crate::proxy::compiler::ProxyProbeTarget {
                address: "127.0.0.1:9080".parse().unwrap(),
                expected_status: 503,
            }],
            business_probes: vec![],
        };
        let confirmed = crate::proxy::apply_status::ConfirmedPublication {
            process_id: std::process::id(),
            instance_id: "instance-standby".into(),
            publication_id: publication_id.into(),
            config_hash: expected_hash.clone(),
            config_digest: outcome.config_digest.clone(),
            attempt_id: 1,
            operation_id: Some("standby-operation".into()),
        };
        crate::proxy::compiler::record_confirmed_publication(&outcome, &confirmed).unwrap();
        assert!(matches!(
            crate::proxy::compiler::official_confirmation()
                .expect("standby confirmation recorded")
                .phase,
            shared_types::ProxyPhase::Standby
        ));

        let admin = admin_probe::AdminBasicSnapshot {
            process_id: Some(std::process::id()),
            config_hash: Some(expected_hash),
            upstreams: std::collections::HashMap::new(),
        };
        let (ready, status, reason, contract) =
            derive_proxy(Some(&admin), true, ServerPhase::Running, false);
        assert!(!ready, "standby must not claim proxy readiness");
        assert_eq!(status, UserAppReadinessStatus::Unknown);
        assert_eq!(reason, Some(UserAppReadinessReason::ProxyStandby));
        assert_eq!(
            contract.as_deref(),
            Some(shared_types::PINGAP_ETYPE_ORIGIN_CONTRACT),
            "standby window responses are entirely proxy-self-produced"
        );
    }

    /// 非 standby 的未确认发布（serving 确认缺失）不透出来源契约——原行为不变。
    #[test]
    fn unconfirmed_serving_publication_still_hides_origin_contract() {
        let publication_id = "0b69964b-8df2-42f9-aa92-8204e27da7af";
        let expected_hash = "SERVINGHASH01".to_string();
        let outcome = crate::proxy::compiler::CompileOutcome {
            serving_release_id: Some("rel-serving".into()),
            operation_id: None,
            config_path: "unused-serving-candidate.toml".into(),
            expected_hash: expected_hash.clone(),
            publication_id: publication_id.into(),
            config_digest: "c".repeat(64),
            entry_probes: vec![crate::proxy::compiler::ProxyProbeTarget {
                address: "127.0.0.1:9080".parse().unwrap(),
                expected_status: 200,
            }],
            business_probes: vec![],
        };
        let confirmed = crate::proxy::apply_status::ConfirmedPublication {
            process_id: std::process::id(),
            instance_id: "instance-serving".into(),
            publication_id: publication_id.into(),
            config_hash: expected_hash.clone(),
            config_digest: outcome.config_digest.clone(),
            attempt_id: 1,
            operation_id: None,
        };
        crate::proxy::compiler::record_confirmed_publication(&outcome, &confirmed).unwrap();

        let admin = admin_probe::AdminBasicSnapshot {
            process_id: Some(std::process::id()),
            config_hash: Some(expected_hash),
            upstreams: std::collections::HashMap::new(),
        };
        let (ready, status, reason, contract) =
            derive_proxy(Some(&admin), true, ServerPhase::Running, false);
        assert!(!ready);
        assert_eq!(status, UserAppReadinessStatus::Unknown);
        assert_eq!(reason, Some(UserAppReadinessReason::ObserveIncomplete));
        assert_eq!(contract, None);
    }

    /// 真实 observer 回归（Codex 复现）：已确认 Standby 后停止收束（Idle +
    /// !ready），快照必须保留 Stopped 判定并附加来源契约——停止/已停止是
    /// standby 摘流的预期业务路径，不能因短路丢契约导致代理不替换正文。
    #[tokio::test]
    async fn stopped_snapshot_keeps_standby_origin_contract() {
        let publication_id = "60fdbd11-ad41-458e-9557-04a0cfba7fb3";
        let expected_hash = "STOPSTANDBY01".to_string();
        let outcome = crate::proxy::compiler::CompileOutcome {
            serving_release_id: None,
            operation_id: Some("stop-operation".into()),
            config_path: "unused-stop-standby-candidate.toml".into(),
            expected_hash: expected_hash.clone(),
            publication_id: publication_id.into(),
            config_digest: "d".repeat(64),
            entry_probes: vec![crate::proxy::compiler::ProxyProbeTarget {
                address: "127.0.0.1:9080".parse().unwrap(),
                expected_status: 503,
            }],
            business_probes: vec![],
        };
        let confirmed = crate::proxy::apply_status::ConfirmedPublication {
            process_id: std::process::id(),
            instance_id: "instance-stop-standby".into(),
            publication_id: publication_id.into(),
            config_hash: expected_hash,
            config_digest: outcome.config_digest.clone(),
            attempt_id: 1,
            operation_id: Some("stop-operation".into()),
        };
        crate::proxy::compiler::record_confirmed_publication(&outcome, &confirmed).unwrap();

        // 已停止形态：Idle + runtime 未 ready（无需服务/网络探测的短路路径）。
        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_release(combo_lock(0));
        state.set_phase(ServerPhase::Idle);
        let observer = BusinessReadinessObserver::new(state.clone());
        let observed = observer.observe().await;

        assert_eq!(observed.status, UserAppReadinessStatus::Stopped);
        assert_eq!(
            observed.proxy.error_origin_contract.as_deref(),
            Some(shared_types::PINGAP_ETYPE_ORIGIN_CONTRACT),
            "stopped window entry still serves the confirmed standby mock"
        );
    }

    /// 无 Standby 确认记录的已停止快照不得透出来源契约（无证据不虚报）。
    #[tokio::test]
    async fn stopped_snapshot_without_standby_record_hides_origin_contract() {
        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_release(combo_lock(0));
        state.set_phase(ServerPhase::Idle);
        let observer = BusinessReadinessObserver::new(state.clone());
        let observed = observer.observe().await;

        assert_eq!(observed.status, UserAppReadinessStatus::Stopped);
        assert_eq!(observed.proxy.error_origin_contract, None);
    }

    /// 来源契约只跟随已确认的 Standby 相位：未确认、其他相位、无记录均不透出。
    #[test]
    fn standby_origin_contract_follows_confirmed_standby_phase() {
        let confirmation =
            |confirmed: bool, phase: shared_types::ProxyPhase| shared_types::ProxyConfirmation {
                contract: shared_types::ProxyConfirmationContract::OfficialRestartHttpV1,
                confirmed,
                phase,
                operation_id: "op".into(),
                proxy_instance_id: None,
                config_digest: None,
            };
        assert_eq!(
            standby_origin_contract(Some(&confirmation(true, shared_types::ProxyPhase::Standby)))
                .as_deref(),
            Some(shared_types::PINGAP_ETYPE_ORIGIN_CONTRACT)
        );
        assert_eq!(
            standby_origin_contract(Some(&confirmation(
                false,
                shared_types::ProxyPhase::Standby
            ))),
            None,
            "unconfirmed standby must not prove the entry origin"
        );
        assert_eq!(
            standby_origin_contract(Some(&confirmation(true, shared_types::ProxyPhase::Serving))),
            None,
            "only the standby phase proves the mock-window origin"
        );
        assert_eq!(
            standby_origin_contract(Some(&confirmation(
                true,
                shared_types::ProxyPhase::Preparing
            ))),
            None,
            "transitional publication phases must not inherit the standby contract"
        );
        assert_eq!(standby_origin_contract(None), None);
    }

    #[tokio::test]
    async fn cache_keeps_the_observed_identity_when_state_changes_before_store() {
        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        let observer = BusinessReadinessObserver::new(state.clone());
        let (fingerprint, observed) = observer.probe_round().await;
        assert_eq!(observed.status, UserAppReadinessStatus::NotDeployed);
        state.set_phase(ServerPhase::Deploying);
        observer.store_cache(fingerprint, Arc::new(observed));
        assert!(
            observer.fresh_cached().is_none(),
            "old observation must not be relabeled with the new state during cache insertion"
        );
    }

    #[tokio::test]
    async fn unsupported_observation_does_not_claim_a_desired_release_is_serving() {
        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_phase(ServerPhase::Running);
        let mut no_services = combo_lock(19491);
        no_services.services.clear();
        no_services.release_id = "desired-without-proxy".into();
        state.set_release(no_services);
        let observer = BusinessReadinessObserver::new(state.clone());
        let observed = observer.observe().await;
        assert_eq!(observed.status, UserAppReadinessStatus::Unsupported);
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::NoProxiedWebServices)
        );
        assert!(observed.serving_release_id.is_none());
        let mut custom = combo_lock(19491);
        custom.release_id = "desired-custom".into();
        custom.pingap.mode = workspace_manifest::PingapMode::Custom;
        state.set_release(custom);
        let observed = observer.observe().await;
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::CustomRouteUnverified)
        );
        assert!(observed.serving_release_id.is_none());
    }

    #[test]
    fn official_replacement_phases_never_borrow_old_ready() {
        let mut confirmation = shared_types::ProxyConfirmation {
            contract: shared_types::ProxyConfirmationContract::OfficialRestartHttpV1,
            confirmed: false,
            phase: shared_types::ProxyPhase::Preparing,
            operation_id: "reload".into(),
            proxy_instance_id: None,
            config_digest: None,
        };
        for phase in [
            shared_types::ProxyPhase::Preparing,
            shared_types::ProxyPhase::Stopping,
            shared_types::ProxyPhase::Starting,
            shared_types::ProxyPhase::Verifying,
        ] {
            confirmation.phase = phase;
            assert_eq!(
                proxy_transition_status(Some(&confirmation)),
                Some(UserAppReadinessStatus::Starting)
            );
        }
        confirmation.phase = shared_types::ProxyPhase::Failed;
        assert_eq!(
            proxy_transition_status(Some(&confirmation)),
            Some(UserAppReadinessStatus::Failed)
        );
        confirmation.phase = shared_types::ProxyPhase::RecoveryRequired;
        assert_eq!(
            proxy_transition_status(Some(&confirmation)),
            Some(UserAppReadinessStatus::Unknown)
        );
        confirmation.phase = shared_types::ProxyPhase::Serving;
        assert_eq!(proxy_transition_status(Some(&confirmation)), None);
    }

    /// 构造带一条已受理未收束 Stop 操作的内核（harness 对齐
    /// server::kernel_startup_tests 的 kernel()/request() 形态）。
    async fn kernel_with_admitted_stop(
        root: &std::path::Path,
        operation_id: &str,
    ) -> Arc<crate::runtime_kernel::RuntimeKernel> {
        use crate::runtime_kernel::{RuntimeKernel, RuntimeStore};
        let source = root.join("workspace");
        std::fs::create_dir_all(&source).unwrap();
        let store = RuntimeStore::open_with_root(root.join("control"), &source).unwrap();
        let identity = store
            .load_or_init_identity(
                "readiness-app".into(),
                "userapp-dev".into(),
                "workspace".into(),
                source.to_string_lossy().into_owned(),
                "readiness-generation".into(),
            )
            .unwrap();
        let kernel = Arc::new(RuntimeKernel::new(store, identity, Box::new(|_| {})));
        let request = shared_types::RuntimeOperationRequest {
            operation_id: operation_id.into(),
            expected_runtime_instance_id: kernel.identity().runtime_instance_id.clone(),
            expected_revision: 0,
            workspace_id: "workspace".into(),
            kind: shared_types::RuntimeOperationKind::Stop,
            profile: shared_types::RunProfileInput::Source {
                workspace_id: "workspace".into(),
            },
            run_config: None,
            request_context: None,
        };
        match kernel.admit(request).await {
            Ok(crate::runtime_kernel::AdmissionOutcome::Accepted(_)) => kernel,
            other => panic!("stop admission must be accepted, got {other:?}"),
        }
    }

    /// 反例（修复前失败）：Stop 已受理未收束的整个窗口（含 standby 发布的
    /// 代理过渡期）顶层必须是 stopping/STOP_ACCEPTED，收束后回到 stopped。
    #[tokio::test]
    async fn admitted_stop_reports_stopping_until_settled() {
        let root = tempfile::tempdir().unwrap();
        let kernel = kernel_with_admitted_stop(root.path(), "stop-window").await;
        assert_eq!(
            kernel.outstanding_stop_operations().unwrap(),
            vec!["stop-window"]
        );

        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_release(combo_lock(19493));
        state.set_phase(ServerPhase::Running);
        assert!(state.set_runtime_kernel(kernel.clone()));

        let observer = BusinessReadinessObserver::new(state.clone());
        let observed = await_status(&observer, UserAppReadinessStatus::Stopping).await;
        assert!(!observed.ready);
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::StopAccepted)
        );
        assert_eq!(observed.operation_id.as_deref(), Some("stop-window"));
        assert!(observed.serving_release_id.is_none());
        assert!(observed.target_release_id.is_none());
        assert!(observed.services.is_empty());
        assert_eq!(observed.proxy.status, UserAppReadinessStatus::Stopping);
        assert_eq!(
            observed.proxy.reason_code,
            Some(UserAppReadinessReason::StopAccepted)
        );
        assert!(observed.proxy.confirmation.is_none());
        assert_eq!(
            observed.runtime_instance_id.as_deref(),
            Some(kernel.identity().runtime_instance_id.as_str())
        );

        // 只读纪律：观察既不推进也不收束内核状态。
        assert_eq!(
            kernel.outstanding_stop_operations().unwrap(),
            vec!["stop-window"]
        );
        assert_eq!(
            kernel.status().await.unwrap().desired,
            shared_types::DesiredState::Stopped
        );

        // 按 run_loop 收束顺序：先终态持久化，再回 Idle。
        state
            .finish_runtime_operation_by_id(
                "stop-window",
                shared_types::RuntimeOperationState::Succeeded,
                None,
            )
            .await
            .unwrap();
        state.set_phase(ServerPhase::Idle);

        let settled = await_status(&observer, UserAppReadinessStatus::Stopped).await;
        assert_eq!(settled.reason_code, None);
        assert_eq!(settled.operation_id, None);
        assert!(kernel.outstanding_stop_operations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn idle_phase_does_not_confirm_an_unsettled_stop() {
        let root = tempfile::tempdir().unwrap();
        let kernel = kernel_with_admitted_stop(root.path(), "idle-stop").await;
        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_release(combo_lock(19495));
        state.set_phase(ServerPhase::Idle);
        assert!(state.set_runtime_kernel(kernel.clone()));

        let observed = BusinessReadinessObserver::new(state.clone())
            .observe()
            .await;
        assert_eq!(observed.status, UserAppReadinessStatus::Stopping);
        assert_eq!(observed.operation_id.as_deref(), Some("idle-stop"));

        // A parked recovery driver can be Idle while cleanup remains unknown.
        // RecoveryRequired is deliberately nonterminal; phase alone proves no exit.
        state
            .finish_runtime_operation_by_id(
                "idle-stop",
                shared_types::RuntimeOperationState::RecoveryRequired,
                Some((
                    shared_types::ERR_BACKEND_ERROR.into(),
                    "cleanup unknown".into(),
                )),
            )
            .await
            .unwrap();
        let observed = BusinessReadinessObserver::new(state.clone())
            .observe()
            .await;
        assert_eq!(observed.status, UserAppReadinessStatus::Stopping);
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::StopAccepted)
        );
        assert!(!observed.ready);
        assert_eq!(
            kernel.outstanding_stop_operations().unwrap(),
            vec!["idle-stop"]
        );
    }

    #[tokio::test]
    async fn unreadable_stop_records_do_not_confirm_stopped() {
        let root = tempfile::tempdir().unwrap();
        let kernel = kernel_with_admitted_stop(root.path(), "unreadable-stop").await;
        let record = root.path().join("control/operations/unreadable-stop.json");
        let original = std::fs::read(&record).unwrap();
        std::fs::write(&record, b"incomplete record").unwrap();
        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_release(combo_lock(19496));
        assert!(state.set_runtime_kernel(kernel));

        for phase in [ServerPhase::Idle, ServerPhase::Running] {
            state.set_phase(phase);
            let observed = BusinessReadinessObserver::new(state.clone())
                .observe()
                .await;
            assert_eq!(observed.status, UserAppReadinessStatus::Unknown);
            assert_eq!(
                observed.reason_code,
                Some(UserAppReadinessReason::ObserveIncomplete)
            );
            assert!(!observed.ready);
            assert!(observed.serving_release_id.is_none());
            assert_eq!(observed.proxy.status, UserAppReadinessStatus::Unknown);
        }
        // Observation neither repairs nor discards durable evidence. A later
        // successful read must recover naturally through the same observer path.
        assert_eq!(std::fs::read(&record).unwrap(), b"incomplete record");
        std::fs::write(&record, original).unwrap();
        let observed = BusinessReadinessObserver::new(state).observe().await;
        assert_eq!(observed.status, UserAppReadinessStatus::Stopping);
        assert_eq!(observed.operation_id.as_deref(), Some("unreadable-stop"));
    }

    /// 停止意图优先于代理过渡短路；无停止意图时过渡窗口（restart 语义）
    /// 保持 starting 不回归。
    #[tokio::test]
    async fn stop_intent_outranks_proxy_transition_but_restart_window_keeps_starting() {
        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_release(combo_lock(19494));
        state.set_phase(ServerPhase::Running);

        // Part A：无停止意图 → 在途代理过渡（Stopping 相位）仍报 starting。
        let outcome = crate::proxy::compiler::CompileOutcome {
            serving_release_id: Some("rel-transition".into()),
            operation_id: Some("fs-restart-window".into()),
            config_path: "unused-transition-candidate.toml".into(),
            expected_hash: "TRANSITIONHASH1".into(),
            publication_id: uuid::Uuid::new_v4().to_string(),
            config_digest: "b".repeat(64),
            entry_probes: vec![],
            business_probes: vec![],
        };
        crate::proxy::compiler::record_official_phase(
            &outcome,
            shared_types::ProxyPhase::Stopping,
            None,
        )
        .unwrap();
        let observer = BusinessReadinessObserver::new(state.clone());
        let observed = observer.observe().await;
        assert_eq!(observed.status, UserAppReadinessStatus::Starting);
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::ServiceStarting)
        );

        // Part B：同一 state 注入已受理未收束的 Stop 内核 → stopping 覆盖过渡。
        let root = tempfile::tempdir().unwrap();
        let kernel = kernel_with_admitted_stop(root.path(), "stop-outrank").await;
        assert!(state.set_runtime_kernel(kernel.clone()));
        let observed = await_status(&observer, UserAppReadinessStatus::Stopping).await;
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::StopAccepted)
        );
        assert_eq!(observed.operation_id.as_deref(), Some("stop-outrank"));
        assert_ne!(observed.operation_id.as_deref(), Some("fs-restart-window"));
        let confirmation = observed
            .proxy
            .confirmation
            .expect("transition confirmation stays visible");
        assert_eq!(confirmation.phase, shared_types::ProxyPhase::Stopping);
    }

    fn service(
        id: &str,
        ready: bool,
        reason: Option<UserAppReadinessReason>,
    ) -> UserAppServiceReadiness {
        UserAppServiceReadiness {
            service_id: id.into(),
            ready,
            status: if ready {
                UserAppReadinessStatus::Ready
            } else {
                UserAppReadinessStatus::Degraded
            },
            reason_code: reason,
        }
    }

    /// ready = 全部服务通过 AND proxy（Plan §4.3）。
    #[test]
    fn top_level_requires_all_services_and_proxy() {
        let services = vec![service("a", true, None), service("b", true, None)];
        assert_eq!(
            derive_top_level(
                ServerPhase::Running,
                &services,
                true,
                UserAppReadinessStatus::Ready
            ),
            UserAppReadinessStatus::Ready
        );
        // proxy 不过 → 不 ready
        assert_ne!(
            derive_top_level(
                ServerPhase::Running,
                &services,
                false,
                UserAppReadinessStatus::Degraded
            ),
            UserAppReadinessStatus::Ready
        );
        // 一个服务退化 → degraded（已进入服务态）
        let degraded = vec![
            service("a", true, None),
            service("b", false, Some(UserAppReadinessReason::HealthHttpFailure)),
        ];
        assert_eq!(
            derive_top_level(
                ServerPhase::Running,
                &degraded,
                true,
                UserAppReadinessStatus::Ready
            ),
            UserAppReadinessStatus::Degraded
        );
        // 空集合永远不 ready
        assert_eq!(
            derive_top_level(
                ServerPhase::Running,
                &[],
                true,
                UserAppReadinessStatus::Ready
            ),
            UserAppReadinessStatus::Starting
        );
    }

    /// Failed 相位 + 无可用服务集合 → failed；但服务全过 → ready（旧服务仍在）。
    #[test]
    fn failed_phase_keeps_ready_when_old_set_serves() {
        let healthy = vec![service("a", true, None)];
        assert_eq!(
            derive_top_level(
                ServerPhase::Failed("deploy failed".into()),
                &healthy,
                true,
                UserAppReadinessStatus::Ready
            ),
            UserAppReadinessStatus::Ready
        );
        let dead = vec![service(
            "a",
            false,
            Some(UserAppReadinessReason::ServiceStarting),
        )];
        assert_eq!(
            derive_top_level(
                ServerPhase::Failed("orchestration failed".into()),
                &dead,
                false,
                UserAppReadinessStatus::Starting
            ),
            UserAppReadinessStatus::Failed
        );
        assert_eq!(
            top_level_reason(UserAppReadinessStatus::Failed, &dead),
            Some(UserAppReadinessReason::OrchestrationFailed),
            "PG/bootstrap failure must not tell callers that startup is still in progress"
        );
    }

    /// 启动窗口（从未有服务起过）→ starting 而非 degraded。
    #[test]
    fn never_started_maps_to_starting() {
        let starting = vec![service(
            "a",
            false,
            Some(UserAppReadinessReason::ServiceStarting),
        )];
        assert_eq!(
            derive_top_level(
                ServerPhase::Running,
                &starting,
                false,
                UserAppReadinessStatus::Starting
            ),
            UserAppReadinessStatus::Starting
        );
        // HTTP 返回了非 2xx（服务已起但坏）→ degraded
        let http_bad = vec![service(
            "a",
            false,
            Some(UserAppReadinessReason::HealthHttpFailure),
        )];
        assert_eq!(
            derive_top_level(
                ServerPhase::Running,
                &http_bad,
                false,
                UserAppReadinessStatus::Starting
            ),
            UserAppReadinessStatus::Degraded
        );
    }

    #[test]
    fn reason_prefers_first_unready_service_evidence() {
        let services = vec![
            service("a", true, None),
            service("b", false, Some(UserAppReadinessReason::UpstreamUnhealthy)),
        ];
        assert_eq!(
            top_level_reason(UserAppReadinessStatus::Degraded, &services),
            Some(UserAppReadinessReason::UpstreamUnhealthy)
        );
        assert_eq!(
            top_level_reason(UserAppReadinessStatus::Ready, &services),
            None
        );
    }

    // ── 真实探测链组合场景（T6 契约：starting → ready → 退化 → 恢复）──────
    //
    // 进程内最小拓扑：一个 proxied web 服务（tiny HTTP）+ 假 Pingap 入口
    // （9080 accept-all）+ 假 admin（/api/basic 返回匹配 hash 与 upstream 健康）。
    // 覆盖真实 HTTP 探测、admin 快照、hash 门、来源契约透出与只读纪律。

    // ── 夹具承载方式（2026-09-27 归因加固）──────────────────────────────
    //
    // 三个假服务跑在独立 OS 线程（阻塞 IO），不挂 tokio runtime：全量并发下
    // 机器 CPU 饥饿曾把进程内假 admin 的应答拖过观察轮 3s 总预算，全部轮次
    // ObserveIncomplete（2026-09-26 失败窗口同批 6 个 deploy 用例 16s 级耗
    // 时，见 native-supervision verification）。OS 线程服务端不依赖被测
    // runtime 的调度就能备好应答；断言语义不变，只消除夹具自饥饿。

    /// 极简 HTTP/1.1 服务：`mode` 决定每次响应（200 健康契约 / 503 退化）。
    fn spawn_tiny_http(
        listener: std::net::TcpListener,
        mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) {
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { continue };
                let mode = mode.clone();
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    let mut buf = [0u8; 2048];
                    if socket.read(&mut buf).is_err() {
                        return;
                    }
                    let ok = mode.load(std::sync::atomic::Ordering::Relaxed) == 0;
                    let body = if ok { "ok" } else { "unhealthy" };
                    let status = if ok {
                        "200 OK"
                    } else {
                        "503 Service Unavailable"
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes());
                });
            }
        });
    }

    /// 假 admin `/api/basic`：返回与 `expected_hash` 匹配的 hash + upstream 健康。
    fn spawn_fake_admin(listener: std::net::TcpListener, expected_hash: String, process_id: u32) {
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { continue };
                let expected_hash = expected_hash.clone();
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    let mut buf = [0u8; 4096];
                    if socket.read(&mut buf).is_err() {
                        return;
                    }
                    #[derive(serde::Serialize)]
                    struct Basic<'a> {
                        config_hash: &'a str,
                        pid: String,
                        upstream_healthy_status: std::collections::BTreeMap<&'static str, Health>,
                    }
                    #[derive(serde::Serialize)]
                    struct Health {
                        healthy: u32,
                        total: u32,
                    }
                    let body = serde_json::to_string(&Basic {
                        config_hash: &expected_hash,
                        pid: process_id.to_string(),
                        upstream_healthy_status: std::collections::BTreeMap::from([(
                            "web",
                            Health {
                                healthy: 1,
                                total: 1,
                            },
                        )]),
                    })
                    .unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes());
                });
            }
        });
    }

    fn combo_lock(service_port: u16) -> workspace_manifest::ReleaseLock {
        let toml = format!(
            r#"
schema_version = 1
release_id = "rel-combo"
workspace_name = "combo"
minimum_app_cli_version = "0.0.1"
runtime_image_digest = "test"

[pingap]
mode = "managed"
version = "0.14.3"
commit = "test"

[[services]]
service_id = "web"
name = "web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = {service_port}
logs = []

[services.env]

[services.run]
command = ["true"]

[services.health]
readiness_path = "/health"

[services.proxy]
path = "/"
"#
        );
        toml::from_str(&toml).expect("combo release lock TOML")
    }

    /// 有界等待观察到达期望状态（每轮跨过 1s 观察缓存 TTL；全量并发下的
    /// 调度噪声不再把合法时序误判为失败——断言语义不变，仍要求最终到达）。
    async fn await_status(
        observer: &super::BusinessReadinessObserver,
        expect: shared_types::UserAppReadinessStatus,
    ) -> super::UserAppBusinessReadiness {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let observed = observer.observe().await;
            if observed.status == expect {
                return (*observed).clone();
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("observer did not reach {expect:?} in time; last: {observed:?}");
            }
            tokio::time::sleep(CACHE_TTL + std::time::Duration::from_millis(200)).await;
        }
    }

    #[tokio::test]
    async fn observer_combo_starting_ready_degraded_recovery() {
        use std::sync::atomic::{AtomicU8, Ordering};

        // 固定端口：入口 9080（accept-all）与 admin 3018（进程内假 admin）。
        let entry =
            std::net::TcpListener::bind("127.0.0.1:9080").expect("bind fake pingap entry 9080");
        let publication_id = uuid::Uuid::new_v4().to_string();
        let process_instance_id = uuid::Uuid::new_v4().to_string();
        let marker = publication_id.clone();
        std::thread::spawn(move || {
            for socket in entry.incoming() {
                let Ok(mut socket) = socket else {
                    continue;
                };
                use std::io::{Read, Write};
                let mut request = [0u8; 2048];
                if socket.read(&mut request).is_err() {
                    continue;
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nX-Rcoder-Publication: {marker}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{marker}",
                    marker.len()
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        let admin = std::net::TcpListener::bind("127.0.0.1:3018").expect("bind fake admin 3018");
        let expected_hash = "COMBOHASH01".to_string();
        let outcome = crate::proxy::compiler::CompileOutcome {
            serving_release_id: Some("rel-combo".into()),
            operation_id: None,
            config_path: "unused-component-candidate.toml".into(),
            expected_hash: expected_hash.clone(),
            publication_id: publication_id.clone(),
            config_digest: "a".repeat(64),
            entry_probes: vec![crate::proxy::compiler::ProxyProbeTarget {
                address: "127.0.0.1:9080".parse().unwrap(),
                expected_status: 200,
            }],
            business_probes: vec![],
        };
        let confirmed = crate::proxy::apply_status::ConfirmedPublication {
            process_id: std::process::id(),
            instance_id: process_instance_id,
            publication_id,
            config_hash: expected_hash.clone(),
            config_digest: outcome.config_digest.clone(),
            attempt_id: 1,
            operation_id: None,
        };
        crate::proxy::compiler::record_confirmed_publication(&outcome, &confirmed).unwrap();
        spawn_fake_admin(admin, expected_hash, confirmed.process_id);
        crate::proxy::admin_probe::register_admin_endpoint(
            "127.0.0.1:3018".into(),
            "combo".into(),
            "combo".into(),
        );

        // 业务服务：先保留端口不监听（connect refused = starting）。
        let service_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("reserve service port");
        let service_port = service_listener.local_addr().expect("service addr").port();
        drop(service_listener);

        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_release(combo_lock(service_port));
        state.set_phase(crate::server::ServerPhase::Running);
        let mut observer = super::BusinessReadinessObserver::new(state.clone());
        // This component fixture tests aggregate HTTP observations only. Its
        // explicit proof adapter is not production authority or real ownership
        // acceptance; native owner suites exercise retained Child/engine proof.
        fn controlled_proof(
            _receipt: &crate::proxy::apply_status::ConfirmedPublication,
            _deadline: tokio::time::Instant,
        ) -> futures::future::BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        observer.physical_verifier = controlled_proof;

        // 1) 服务未监听 → starting（connect refused 不算退化）。
        let observed = await_status(&observer, UserAppReadinessStatus::Starting).await;
        assert!(!observed.ready);
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::ServiceStarting)
        );

        // 2) 服务 200 + admin hash 匹配 + 入口可达 → ready，来源契约透出。
        let mode = Arc::new(AtomicU8::new(0));
        let service_listener =
            std::net::TcpListener::bind(("127.0.0.1", service_port)).expect("bind service port");
        spawn_tiny_http(service_listener, mode.clone());
        let observed = await_status(&observer, UserAppReadinessStatus::Ready).await;
        assert!(observed.ready, "expected ready, got {observed:?}");
        assert!(observed.proxy.ready);
        assert_eq!(
            observed.proxy.error_origin_contract.as_deref(),
            Some("pingap_etype_v1")
        );
        assert_eq!(observed.services.len(), 1);
        assert!(observed.services[0].ready);
        assert_eq!(observed.serving_release_id.as_deref(), Some("rel-combo"));
        state
            .try_accept_deploy_with_id(
                crate::server::DeployRequest {
                    runtime_operation_id: None,
                    url: "https://fixture.invalid/new.zip".into(),
                    release_id: "rel-desired".into(),
                    sha256: None,
                    local_path: None,
                    execution_target: None,
                    run_pg: None,
                },
                "desired-operation".into(),
            )
            .unwrap();
        let mut desired = combo_lock(service_port);
        desired.release_id = "rel-desired".into();
        state.set_release(desired);
        state.set_phase(ServerPhase::Orchestrating);
        let observed = observer.observe().await;
        assert!(observed.ready);
        assert_eq!(
            observed.serving_release_id.as_deref(),
            Some("rel-combo"),
            "old verified HTTP must not be labeled with the newly desired release"
        );
        assert_eq!(observed.target_release_id.as_deref(), Some("rel-desired"));
        state.set_release(combo_lock(service_port));
        state.set_phase(ServerPhase::Running);
        fn rejected_proof(
            _receipt: &crate::proxy::apply_status::ConfirmedPublication,
            _deadline: tokio::time::Instant,
        ) -> futures::future::BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { anyhow::bail!("controlled foreign process identity") })
        }
        observer.physical_verifier = rejected_proof;
        observer.cache.lock().unwrap().take();
        let unowned = observer.observe().await;
        assert!(
            !unowned.ready && !unowned.proxy.ready,
            "matching HTTP, PID and hash cannot replace physical owner proof"
        );
        assert!(unowned.serving_release_id.is_none());
        assert!(!unowned.proxy.confirmation.as_ref().unwrap().confirmed);
        observer.physical_verifier = controlled_proof;
        observer.cache.lock().unwrap().take();
        let probe_before = state.runtime_status().is_ready();

        // 3) 服务转 503 → degraded（HTTP 契约失败）；代理入口仍健康（局部状态保留）。
        mode.store(1, Ordering::Relaxed);
        let observed = await_status(&observer, UserAppReadinessStatus::Degraded).await;
        assert!(!observed.ready);
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::HealthHttpFailure)
        );
        assert!(observed.proxy.ready, "proxy entry stays healthy");

        // 4) 恢复 200 → ready again（不只记住一次成功）。
        mode.store(0, Ordering::Relaxed);
        tokio::time::sleep(CACHE_TTL + std::time::Duration::from_millis(100)).await;
        let observed = observer.observe().await;
        assert!(
            observed.ready,
            "expected recovery to ready, got {observed:?}"
        );

        // 只读纪律：整轮查询不回写编排探针 bool、不改相位。
        assert_eq!(state.runtime_status().is_ready(), probe_before);
        assert!(matches!(state.phase(), crate::server::ServerPhase::Running));
    }
}
