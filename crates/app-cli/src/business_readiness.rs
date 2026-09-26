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

/// 进程内状态指纹：任何应使旧观察失效的变化都会改变指纹。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservationFingerprint {
    phase: String,
    release_id: Option<String>,
    business_ready_flag: bool,
    initializing: bool,
    expected_proxy_hash: Option<String>,
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
}

impl BusinessReadinessObserver {
    pub fn new(server: Arc<ServerState>) -> Self {
        Self {
            server,
            probe_gate: tokio::sync::Mutex::new(()),
            cache: std::sync::Mutex::new(None),
            revision: std::sync::atomic::AtomicU64::new(0),
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
                self.revision.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1
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
        let result = Arc::new(self.probe_round().await);
        self.store_cache(result.clone());
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

    fn store_cache(&self, result: Arc<UserAppBusinessReadiness>) {
        let fingerprint = ObservationFingerprint::capture(&self.server);
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let revision = match guard.as_ref() {
            Some(cached) if cached.fingerprint == fingerprint => cached.revision,
            _ => self.revision.load(std::sync::atomic::Ordering::Acquire),
        };
        *guard = Some(CachedReadiness {
            fingerprint,
            revision,
            at: std::time::Instant::now(),
            result,
        });
    }

    /// 无网络 I/O 的探测轮：真实探测 + 前后指纹复核。
    async fn probe_round(&self) -> UserAppBusinessReadiness {
        let fingerprint_before = ObservationFingerprint::capture(&self.server);
        let revision = self.current_revision(&fingerprint_before);
        let mut result = Self::probe_once(&self.server).await;
        let fingerprint_after = ObservationFingerprint::capture(&self.server);
        if fingerprint_after != fingerprint_before {
            // 换代窗口：预算内快速重试一次，仍不稳定则按 INSTANCE_CHANGED 丢弃。
            let retry = Self::probe_once(&self.server).await;
            let fingerprint_retry = ObservationFingerprint::capture(&self.server);
            if fingerprint_retry == fingerprint_after {
                result = retry;
            } else {
                tracing::warn!(
                    "business readiness observation discarded: instance transition during probe"
                );
                return UserAppBusinessReadiness {
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
                    },
                    services: Vec::new(),
                };
            }
        }
        result.observation_revision = revision;
        result
    }

    /// 一次完整观察：状态机短路 → 并发服务探测 → Pingap admin 快照 → 集中推导。
    async fn probe_once(server: &Arc<ServerState>) -> UserAppBusinessReadiness {
        let checked_at = now_rfc3339();
        let instance_id = runtime_instance_id(server);
        let operation_id = server.current_runtime_operation();
        let phase = server.phase();
        let release = server.release();
        let initializing = server.initializing();

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
                },
                services: Vec::new(),
            };
        };

        // 确认停止：相位回 Idle 且业务 ready 标志已撤销（StopBusiness 收束路径
        // 恒经此组合）。stopped 不需要任何网络探测。
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
                    error_origin_contract: None,
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
                serving_release_id: Some(release.release_id.clone()),
                target_release_id: None,
                operation_id,
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status: UserAppReadinessStatus::Unsupported,
                    reason_code: Some(UserAppReadinessReason::NoProxiedWebServices),
                    error_origin_contract: None,
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
                serving_release_id: Some(release.release_id.clone()),
                target_release_id: None,
                operation_id,
                observation_revision: 0,
                proxy: UserAppProxyReadiness {
                    ready: false,
                    status: UserAppReadinessStatus::Unsupported,
                    reason_code: Some(UserAppReadinessReason::CustomRouteUnverified),
                    error_origin_contract: None,
                },
                services: Vec::new(),
            };
        }

        // ── 真实探测：服务 HTTP（并发 ≤8）+ Pingap admin 快照 + 入口 TCP ──
        // 总预算 3s：各项探测受剩余预算约束，预算耗尽的未完成项记 unknown
        //（不折算成失败，也不从汇总集合消失）。
        let deadline = tokio::time::Instant::now() + OBSERVE_BUDGET;
        let admin_snapshot = fetch_admin_snapshot(deadline).await;
        let entry_reachable = tokio::time::timeout(
            item_budget(deadline),
            tokio::net::TcpStream::connect(("127.0.0.1", crate::proxy::pingap::PINGAP_PORT)),
        )
        .await
        .map(|connected| connected.is_ok())
        .unwrap_or(false);

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
        let (proxy_ready, proxy_status, proxy_reason, origin_contract) =
            derive_proxy(admin_snapshot.as_ref(), entry_reachable, phase.clone());

        let target_release_id = target_release_hint(server, &release.release_id);

        let readiness = derive_top_level(phase.clone(), &services, proxy_ready, proxy_status);
        UserAppBusinessReadiness {
            ready: readiness.is_ready(),
            status: readiness,
            reason_code: top_level_reason(readiness, &services),
            checked_at,
            runtime_instance_id: instance_id,
            serving_release_id: readiness.is_ready().then(|| release.release_id.clone()),
            target_release_id,
            operation_id,
            observation_revision: 0,
            proxy: UserAppProxyReadiness {
                ready: proxy_ready,
                status: proxy_status,
                reason_code: proxy_reason,
                error_origin_contract: origin_contract,
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
    let request = server
        .deploy_request_release_id()
        .filter(|value| !value.trim().is_empty() && value != current);
    match server.phase() {
        ServerPhase::Deploying | ServerPhase::Orchestrating => request,
        _ => None,
    }
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

/// 代理就绪推导：入口监听 + 生效 hash 匹配；来源契约仅在 hash 确认时透出。
#[allow(clippy::type_complexity)]
fn derive_proxy(
    admin: Option<&admin_probe::AdminBasicSnapshot>,
    entry_reachable: bool,
    phase: ServerPhase,
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

/// 顶层原因码：取代表性的第一个服务级证据。
fn top_level_reason(
    status: UserAppReadinessStatus,
    services: &[UserAppServiceReadiness],
) -> Option<UserAppReadinessReason> {
    if status.is_ready() {
        return None;
    }
    services
        .iter()
        .find(|service| !service.ready)
        .and_then(|service| service.reason_code)
        .or(match status {
            UserAppReadinessStatus::Failed => Some(UserAppReadinessReason::OrchestrationFailed),
            UserAppReadinessStatus::Degraded => Some(UserAppReadinessReason::HealthDegraded),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 极简 HTTP/1.1 服务：`mode` 决定每次响应（200 健康契约 / 503 退化）。
    async fn spawn_tiny_http(
        listener: tokio::net::TcpListener,
        mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mode = mode.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 2048];
                let Ok(read) = socket.read(&mut buf).await else {
                    return;
                };
                let _ = read;
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
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    }

    /// 假 admin `/api/basic`：返回与 `expected_hash` 匹配的 hash + upstream 健康。
    async fn spawn_fake_admin(listener: tokio::net::TcpListener, expected_hash: String) {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let expected_hash = expected_hash.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let Ok(read) = socket.read(&mut buf).await else {
                    return;
                };
                let _ = read;
                let body = format!(
                    r#"{{"config_hash":"{expected_hash}","upstream_healthy_status":{{"web":{{"healthy":1,"total":1}}}}}}"#
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
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
        let entry = tokio::net::TcpListener::bind("127.0.0.1:9080")
            .await
            .expect("bind fake pingap entry 9080");
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = entry.accept().await else {
                    return;
                };
                drop(socket);
            }
        });
        let admin = tokio::net::TcpListener::bind("127.0.0.1:3018")
            .await
            .expect("bind fake admin 3018");
        let expected_hash = "COMBOHASH01".to_string();
        tokio::spawn(spawn_fake_admin(admin, expected_hash.clone()));
        crate::proxy::admin_probe::register_admin_endpoint(
            "127.0.0.1:3018".into(),
            "combo".into(),
            "combo".into(),
        );
        crate::proxy::compiler::record_expected_hash(&expected_hash);

        // 业务服务：先保留端口不监听（connect refused = starting）。
        let service_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve service port");
        let service_port = service_listener.local_addr().expect("service addr").port();
        drop(service_listener);

        let state = Arc::new(ServerState::new(
            crate::runtime_status::RuntimeStatusService::default(),
        ));
        state.mark_initialized();
        state.set_release(combo_lock(service_port));
        state.set_phase(crate::server::ServerPhase::Running);
        let observer = super::BusinessReadinessObserver::new(state.clone());

        // 1) 服务未监听 → starting（connect refused 不算退化）。
        let observed = await_status(&observer, UserAppReadinessStatus::Starting).await;
        assert!(!observed.ready);
        assert_eq!(
            observed.reason_code,
            Some(UserAppReadinessReason::ServiceStarting)
        );

        // 2) 服务 200 + admin hash 匹配 + 入口可达 → ready，来源契约透出。
        let mode = Arc::new(AtomicU8::new(0));
        let service_listener = tokio::net::TcpListener::bind(("127.0.0.1", service_port))
            .await
            .expect("bind service port");
        tokio::spawn(spawn_tiny_http(service_listener, mode.clone()));
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
