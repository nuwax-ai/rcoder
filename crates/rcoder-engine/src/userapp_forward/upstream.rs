//! userApp 透传层的上游定位与转发内核。
//!
//! - 定位契约：dev=注册表 + 探活自愈（30s 正缓存；脏值清 container 字段重建，
//!   不 remove_project 保 PG 会话映射）；prod=存在性检查 + 唤醒（stopped/starting
//!   等待，503+Retry-After）+ 确定性命名/容器 IPv4 + 发送请求体前确认端口可连
//! - 转发内核：method/path/query/headers/body 全量流式（multipart/SSE 天然
//!   支持），hop-by-hop 头按 RFC 9110 剥离（静态表 ∪ Connection 动态列举）
//! - 容器定位按 `X-App-Id` header（白名单校验）；容器不在线 502（dev）/
//!   503+Retry-After（prod 唤醒失败）

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};

use shared_types::APP_ID_HEADER;

/// 旧用户定位 header（Java 不再发送；意外携带时按名称移除——spec §2.1）。
pub(super) const LEGACY_USER_ID_HEADER: &str = "x-user-id";

use crate::app_state::AppState;
use crate::userapp_builder::{dev_file_server_addr, ensure_userapp_builder_until};

use super::semantics::HttpResultError;

/// userApp 透传面 API 前缀（运行时路径分派与透传清单共用词根）。
pub(super) const USERAPP_API_PREFIX: &str = "/api/v1/userapp";
/// tasks 族路径前缀（query app_id 定位 + 短路语义识别共用）。
pub(super) const TASKS_PATH_PREFIX: &str = "/api/v1/userapp/tasks/";
/// static 族路径前缀（构建链制品下载，path 段 app_id 定位）。
pub(super) const STATIC_PATH_PREFIX: &str = "/api/v1/userapp/static/";

/// 逐跳头静态表：转发前剥离（reqwest/上游自行生成；host 逐跳重写）。
const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "host",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-authenticate",
    "proxy-authorization",
];

/// 判定请求/响应头是否逐跳剥离：静态表 ∪ `Connection` 头动态列出的头
/// （RFC 9110 §7.6.1：`Connection: X-Foo` 则 X-Foo 亦是逐跳——静态表无法穷尽）。
fn is_hop_by_hop(name: &str, connection_listed: &[&str]) -> bool {
    HOP_BY_HOP.contains(&name.to_ascii_lowercase().as_str())
        || connection_listed
            .iter()
            .any(|listed| listed.eq_ignore_ascii_case(name))
}

/// 从 headers 提取 Connection 头动态声明的逐跳头名列表（小写化）。
fn connection_listed_tokens(headers: &axum::http::HeaderMap) -> Vec<String> {
    headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| t.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

/// 解析并校验 app_id header（None = 缺失/空/非法，调用方返回 400 HttpResult）。
///
/// identifier 白名单必做：`computer_intercept` 挂在无鉴权的 file-server 路由面
/// （与 TS 一致性设计），app_id 原样进入容器标识与 Docker bind 宿主路径拼接
/// （`host_root.join(app_id)`），含 `/` 即逃逸开发卷根把宿主任意目录挂进容器。
pub(super) fn require_app_id(req: &Request) -> Option<String> {
    app_id_from_headers(req.headers())
}

/// [`require_app_id`] 的 HeaderMap 版本（body 读取重组后 parts.headers 复用）。
pub(super) fn app_id_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers
        .get(APP_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    shared_types::validate_identifier(raw, "app_id").ok()?;
    Some(raw.to_owned())
}

pub(super) fn missing_app_id_response() -> Response {
    HttpResultError::bad_request(format!(
        "missing required header `{APP_ID_HEADER}` for userApp forwarding"
    ))
    .into_response()
}

/// 定位（miss 幂等 ensure）开发容器 file-server addr。
///
/// 注册表脏值自愈：容器被外部删除（docker rm / 回收）后 state.projects 残留死 IP，
/// 且 ensure 被注册表命中挡住不会重建——转发前轻量探活（GET /api/version，3s 超时）。
/// 探活失败**不直接判死**：先经 `crate::userapp_builder::remediate_stale_registry`
/// 以容器运行时真实状态裁决——Running 保容器（高负载超时/启动窗口抖动），
/// 真死才清注册重建。
/// 交互请求（file-list/git status 等）的整个定位阶段最多等待
/// `interactive_ensure_wait_seconds`（默认 30s，仍受 ensure 总预算约束）；
/// 已知冲突保留操作身份，否则返回等待超时，调用方可重试。tasks 查询不触发
/// ensure。部署制品拉取（`/api/v1/userapp/static/*`）保留完整配置预算
/// （默认 90s）。调度、拉镜像或 drain 可能超出任一预算；HTTP 等待结束
/// 不取消创建工作者。
async fn resolve_dev_addr(
    state: &AppState,
    app_id: &str,
    artifact_download: bool,
) -> Result<String, Box<Response>> {
    let configured =
        std::time::Duration::from_secs(state.config.userapp_storage.ensure_timeout_seconds);
    let budget = if artifact_download {
        configured
    } else {
        configured.min(std::time::Duration::from_secs(
            state.config.userapp_storage.interactive_ensure_wait_seconds,
        ))
    };
    let deadline = tokio::time::Instant::now() + budget;
    tokio::time::timeout_at(deadline, resolve_dev_addr_inner(state, app_id, deadline))
        .await
        .unwrap_or_else(|_| {
            Err(builder_control_response(
                &shared_types::UserAppWaitTimeout { operation_id: None }.into(),
            ))
        })
}

/// 转发层"可等待"的 builder 冲突判定。两种形态是同一个 builder 控制窗口
/// 的两个观测面，在调用方 deadline 内等待并重新检查持久意图：
/// 1. 准入被在途 Dev 控制操作拒绝（`OperationInProgress`）；
/// 2. 本请求的 ensure 操作已被在途控制操作取消
///    （`BuilderEnsureSuperseded`：本次创建所有已发写均有返回）。
///
/// 只把"取消/被接管"子集纳入等待；其余失败（镜像拉取失败等）维持快速
/// 失败，deadline 同时是等待的兜底熔断。
fn waitable_builder_conflict(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<shared_types::UserAppStoreError>())
        .any(|store_error| {
            matches!(
                store_error,
                shared_types::UserAppStoreError::OperationInProgress(blocker)
                    if blocker.scope == shared_types::UserAppOperationScope::Dev
            )
        })
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<crate::userapp_builder::BuilderEnsureSuperseded>()
                .is_some()
        })
}

/// Both initial lookup and stale-registration repair use this waiter. Only
/// ensure is retried; the forwarded HTTP request and its body are sent once.
async fn retry_builder_ensure<T, F, Fut>(
    deadline: tokio::time::Instant,
    mut ensure: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    if tokio::time::Instant::now() >= deadline {
        return Err(shared_types::UserAppWaitTimeout { operation_id: None }.into());
    }
    loop {
        let error = match tokio::time::timeout_at(deadline, ensure())
            .await
            .map_err(|_| shared_types::UserAppWaitTimeout { operation_id: None })?
        {
            Ok(info) => return Ok(info),
            Err(error) => error,
        };
        if !waitable_builder_conflict(&error) || tokio::time::Instant::now() >= deadline {
            return Err(error);
        }
        let next_attempt =
            (tokio::time::Instant::now() + std::time::Duration::from_millis(500)).min(deadline);
        tokio::time::sleep_until(next_attempt).await;
        if tokio::time::Instant::now() >= deadline {
            // Return the last observed conflict before the outer address
            // timeout discards it. Never admit another ensure at the deadline.
            return Err(error);
        }
    }
}

async fn resolve_dev_addr_inner(
    state: &AppState,
    app_id: &str,
    deadline: tokio::time::Instant,
) -> Result<String, Box<Response>> {
    // A dev-scope control operation (Stop/Restart) fences new business ensure
    // attempts by admission design, and may also cancel an already-admitted
    // ensure mid-flight. Wait out either conflict within the caller's budget.
    // Each attempt rechecks durable intent: completed Stop remains stopped;
    // completed Restart can be reused. This never grants an explicit start.
    let mut info = retry_builder_ensure(deadline, || {
        ensure_userapp_builder_until(state, app_id, deadline)
    })
    .await
    .map_err(|error| {
        builder_control_response(&error.context(format!("ensure dev container: app_id={app_id}")))
    })?;
    let mut addr = dev_file_server_addr(state, &info)
        .map_err(|error| HttpResultError::bad_gateway(error.to_string()).into_boxed_response())?;
    // 探活正缓存(30s): 每次转发都探活会给高频文件操作(批量列表/读写)平添一个
    // RTT; 成功后窗口内免探。失败路径(自愈重建)不受缓存影响; 窗口内死容器漏检
    // 可接受——send 失败仍会 502, 下一请求自愈。
    // 键 = 注册信息携带的复合 identifier（`{user_id}-{app_id}`，实例粒度）。
    let cache = PROBE_OK.get_or_init(dashmap::DashMap::new);
    let probe_key = probe_cache_key(app_id, &info);
    // view 回调式读取：不产生 Ref guard（锁仅在闭包内持有），结构性杜绝
    // guard 跨 await 的自死锁可能——不依赖"临时值即时 drop"的写法纪律
    let probe_fresh = cache
        .view(&probe_key, |_, t| t.elapsed() < PROBE_TTL)
        .unwrap_or(false);
    let credentials = super::file_credentials::credentials(
        state,
        shared_types::UserappStage::Dev,
        app_id,
        deadline,
    )
    .await
    .map_err(|failure| {
        HttpResultError::from_app_error(failure.into_app_error()).into_boxed_response()
    })?;
    if !probe_fresh && !probe_dev_container(&addr, &credentials).await {
        warn!(
            "[USERAPP_FORWARD] dev container probe failed (stale registry entry?), verifying container state: app_id={app_id}, addr={addr}"
        );
        // 先验容器真实状态再决定处置：Running 则保容器（探活失败是超时/未就绪
        // 抖动，编译高负载/新容器启动窗口常见），只有真死才清注册重建——
        // 防误杀正在跑任务的容器
        match crate::userapp_builder::remediate_stale_registry(state, app_id, &probe_key)
            .await
            .map_err(|error| {
                HttpResultError::bad_gateway(format!("verify builder state: {error:#}"))
                    .into_boxed_response()
            })? {
            crate::userapp_builder::RegistryRemediation::Alive(info) => {
                info!(
                    "[USERAPP_FORWARD] dev container alive on inspect, keep without rebuild: app_id={app_id}"
                );
                // 写正缓存：容器经 inspect 确认在跑（探活失败只是负载抖动），30s
                // 窗口内不再重复付 3s 探活超时——否则编译高峰期每个请求都要
                // probe 超时+inspect 一次。窗口内容器真死漏检与既有语义一致
                // （send 失败 502，下一请求自愈）。
                cache.insert(probe_key, std::time::Instant::now());
                return dev_file_server_addr(state, &info).map_err(|error| {
                    HttpResultError::bad_gateway(error.to_string()).into_boxed_response()
                });
            }
            crate::userapp_builder::RegistryRemediation::Gone => {
                // Re-enter coordinated ensure, which rechecks the latest registry
                // and runtime under the application lock. The observation above
                // does not authorize clearing a newer registration or its streams.
                info = retry_builder_ensure(deadline, || {
                    ensure_userapp_builder_until(state, app_id, deadline)
                })
                .await
                .map_err(|error| {
                    builder_control_response(
                        &error.context(format!("re-ensure dev container: app_id={app_id}")),
                    )
                })?;
                addr = dev_file_server_addr(state, &info).map_err(|error| {
                    HttpResultError::bad_gateway(error.to_string()).into_boxed_response()
                })?;
                // 重建的新容器可能仍在启动(agent_runner+file-server+PG 全套)——不写探活
                // 缓存, 由本次 send 定成败; 下一请求重新探活
                return Ok(addr);
            }
        }
    }
    if !probe_fresh {
        // 探活过 ≠ 归属正确：跨族污染（生产 pod 被写入注册表）时生产容器的
        // file-server 同样在 60000 应答——借 30s 探活缓存 miss 窗口做归属交叉
        // 校验（成本：每 app 每 30s 一次 K8s get），污染即自愈刷回 builder 值
        if let Some(updated) =
            crate::userapp_builder::cross_verify_registration(state, app_id, &probe_key, &info)
                .await
                .map_err(|error| {
                    HttpResultError::bad_gateway(format!("verify builder identity: {error:#}"))
                        .into_boxed_response()
                })?
        {
            cache.insert(probe_key, std::time::Instant::now());
            return dev_file_server_addr(state, &updated).map_err(|error| {
                HttpResultError::bad_gateway(error.to_string()).into_boxed_response()
            });
        }
        return Err(
            HttpResultError::bad_gateway("Builder is no longer running").into_boxed_response()
        );
    }
    Ok(addr)
}

fn builder_control_response(error: &anyhow::Error) -> Box<Response> {
    // control_error already logs structured admission conflicts. Log other
    // outcomes here, once after retries end, rather than on every poll.
    if !matches!(
        error.downcast_ref::<shared_types::UserAppStoreError>(),
        Some(shared_types::UserAppStoreError::OperationInProgress(_))
    ) {
        warn!("[USERAPP_FORWARD] ensure dev container failed: {error:#}");
    }
    let mut response = crate::userapp_builder::control_error(error).into_response();
    // Legacy TS forwarding keeps its gateway status. Formal routes normalize
    // the same envelope to HTTP 200 without dropping code or operation identity.
    *response.status_mut() = axum::http::StatusCode::BAD_GATEWAY;
    Box::new(response)
}

/// 探活正缓存: 复合 identifier → 最近一次探活成功时刻(重建自愈后刷新)。
static PROBE_OK: std::sync::OnceLock<dashmap::DashMap<String, std::time::Instant>> =
    std::sync::OnceLock::new();
const PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// 探活缓存键：注册信息携带的 identifier（应用共享后 = 纯 app_id；
/// 存量残留复合注册沿用其原键，保探活节流不失效）。
fn probe_cache_key(app_id: &str, info: &shared_types::ContainerBasicInfo) -> String {
    if info.project_id.is_empty() {
        app_id.to_string()
    } else {
        info.project_id.clone()
    }
}

/// 摘除探活正缓存条目（app purge/容器重建后调用，防缓存残留已删实例的
/// 健康时刻）。键 = 复合 identifier（invalidate 侧自行派生时用 app 级
/// 兜底键——多数调用点只有纯 app_id 上下文，实例粒度失效由 rebuild 路径
/// 的注册刷新间接覆盖）。
pub(crate) fn invalidate_probe_cache(key: &str) {
    if let Some(cache) = PROBE_OK.get() {
        cache.remove(key);
    }
}

/// 开发容器 file-server 轻量探活（连接失败/非 2xx 均视为不可用）。
async fn probe_dev_container(
    addr: &str,
    credentials: &shared_types::FileServerRequestCredentials,
) -> bool {
    credentials
        .apply(
            crate::http_client::forward_client()
                .get(format!("{addr}/api/version"))
                .timeout(std::time::Duration::from_secs(3)),
        )
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

// Hotpath 埋点：userapp 反代完整往返（请求头透传 + 上游 send + 响应流组装；feature 关闭时 no-op）
#[hotpath::measure]
async fn forward_to_addr(
    target_label: &str,
    app_id: &str,
    addr: &str,
    credentials: &shared_types::FileServerRequestCredentials,
    req: Request,
) -> Response {
    let target = format!("{addr}{}", req.uri());

    let (mut parts, body) = req.into_parts();
    // 旧用户定位 header 在转发边界按名称移除（spec §2.1：不读取值、不因值拒收）
    parts.headers.remove(LEGACY_USER_ID_HEADER);
    // Control-plane and proxy-hop credentials belong to distinct peers.
    parts.headers.remove("x-api-key");
    parts.headers.remove("x-proxy-token");
    let listed = connection_listed_tokens(&parts.headers);
    // 循环外一次构造引用视图（原先每个 header 重建一次 Vec）
    let listed_refs: Vec<&str> = listed.iter().map(String::as_str).collect();
    let mut outbound = crate::http_client::forward_client().request(parts.method, &target);
    for (name, value) in &parts.headers {
        if is_hop_by_hop(name.as_str(), &listed_refs) {
            continue;
        }
        outbound = outbound.header(name, value);
    }
    let reqwest_body = reqwest::Body::wrap_stream(body.into_data_stream());
    outbound = credentials.apply(outbound.body(reqwest_body));

    let upstream = match outbound.send().await {
        Ok(resp) => resp,
        Err(e) => {
            // 完整原始错误（含目标地址）只进日志；对用户分类呈现——
            // 连接建立失败（请求未送达，无副作用）→ 地址未就绪 + Retry-After；
            // 其余（超时/中途断开）→ 净化后的通用文案，不泄露内部拓扑。
            warn!(
                "[USERAPP_FORWARD] upstream request failed: app_id={app_id}, target={target}: {e:?}"
            );
            // 分类仅限 dev 路径（dev 有重启窗口语义与专属文案；prod 的
            // 连接失败由唤醒层分类，这里保持通用 502）。
            let locale = shared_types::current_request_locale();
            if e.is_connect() && target_label == "dev" {
                return HttpResultError::unavailable_with_code(
                    shared_types::error_codes::ERR_CONTAINER_ADDRESS_NOT_READY,
                    shared_types::t("error.dev_container_unreachable", locale),
                    10,
                )
                .into_response();
            }
            return HttpResultError::bad_gateway(format!(
                "{target_label} container request failed before a response was received"
            ))
            .into_response();
        }
    };

    let status = upstream.status();
    let resp_listed = connection_listed_tokens(upstream.headers());
    let resp_listed_refs: Vec<&str> = resp_listed.iter().map(String::as_str).collect();
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if is_hop_by_hop(name.as_str(), &resp_listed_refs) {
            continue;
        }
        builder = builder.header(name, value);
    }
    match builder.body(Body::from_stream(upstream.bytes_stream())) {
        Ok(resp) => resp,
        Err(e) => HttpResultError::system(format!("build upstream response: {e}")).into_response(),
    }
}

async fn forward_to_configured_addr(
    state: &AppState,
    stage: shared_types::UserappStage,
    target_label: &str,
    app_id: &str,
    addr: &str,
    req: Request,
    deadline: tokio::time::Instant,
) -> Response {
    let credentials = match super::file_credentials::credentials(state, stage, app_id, deadline)
        .await
    {
        Ok(credentials) => credentials,
        Err(failure) => {
            return super::error_body::reject(req, failure.into_app_error().into_response()).await;
        }
    };
    forward_to_addr(target_label, app_id, addr, &credentials, req).await
}

/// 全量透传一个请求到该 app 开发容器的 file-server（同 path+query）。
pub(crate) async fn forward_to_dev(state: &AppState, app_id: &str, req: Request) -> Response {
    if !matches!(
        super::semantics::classify_dev_absent(req.uri().path()),
        super::semantics::DevAbsentAction::Ensure
    ) {
        return match super::semantics::existing_dev_addr(state, app_id).await {
            Ok(Some(addr)) => {
                if let Err(error) = wait_for_dev_service(state, app_id, &addr).await {
                    return super::error_body::reject(req, error.into_response()).await;
                }
                forward_to_configured_addr(
                    state,
                    shared_types::UserappStage::Dev,
                    "dev",
                    app_id,
                    &addr,
                    req,
                    tokio::time::Instant::now()
                        + std::time::Duration::from_secs(
                            state.config.userapp_storage.ensure_timeout_seconds,
                        ),
                )
                .await
            }
            Ok(None) => {
                super::error_body::reject(req, super::semantics::unavailable_response(app_id)).await
            }
            Err(error) => super::error_body::reject(req, error.into_response()).await,
        };
    }
    // 制品拉取保留完整配置预算；其余交互转发使用较短的响应预算。
    let artifact_download = req.uri().path().starts_with(STATIC_PATH_PREFIX);
    let addr = match resolve_dev_addr(state, app_id, artifact_download).await {
        Ok(addr) => addr,
        Err(resp) => return super::error_body::reject(req, *resp).await,
    };
    if let Err(error) = wait_for_dev_service(state, app_id, &addr).await {
        return super::error_body::reject(req, error.into_response()).await;
    }
    forward_to_configured_addr(
        state,
        shared_types::UserappStage::Dev,
        "dev",
        app_id,
        &addr,
        req,
        tokio::time::Instant::now()
            + std::time::Duration::from_secs(state.config.userapp_storage.ensure_timeout_seconds),
    )
    .await
}

/// dev 转发发送前的 TCP 连接预检（对齐 prod [`Self::wait_for_prod_service`]
/// 的语义与安全论证：请求 body 是单次流，连接失败不重放——在把 body 移入
/// reqwest 之前确认真实 Service 路径可连接）。覆盖 builder 原地容器重启
/// （svc 无就绪 endpoint 的 ~12s 窗口）与全量 pod 重建窗口；等待期间读取
/// 在途 Dev 操作作为失败分级证据：
/// - 预算内连上 → `Ok`（后续真实请求可能因残余竞态失败，由
///   [`forward_to_addr`] 的分类出口兜底）；
/// - 预算耗尽 + 有在途 Dev 操作 → `ERR_OPERATION_IN_PROGRESS`（带
///   operation_id——语义"重启/停止进行中"，与 keepalive ensure 同构）；
/// - 预算耗尽 + 无在途 → `ERR_CONTAINER_ADDRESS_NOT_READY` + Retry-After
///   （本地化文案，不再裸抛 reqwest 连接错误）。
async fn wait_for_dev_service(
    state: &AppState,
    app_id: &str,
    addr: &str,
) -> Result<(), HttpResultError> {
    const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
    let budget = std::time::Duration::from_secs(
        state
            .config
            .userapp_storage
            .dev_forward_connect_wait_seconds
            .max(1),
    );
    let deadline = tokio::time::Instant::now() + budget;
    let uri = addr.parse::<reqwest::Url>().map_err(|error| {
        HttpResultError::bad_gateway(format!("Invalid dev container address: {error}"))
    })?;
    let host = uri
        .host_str()
        .ok_or_else(|| HttpResultError::bad_gateway("Dev container address has no host"))?;
    let port = uri
        .port_or_known_default()
        .ok_or_else(|| HttpResultError::bad_gateway("Dev container address has no port"))?;
    let mut inflight: Option<shared_types::UserAppOperationView> = None;
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        if tcp_connect_once_within(host, port, deadline).await {
            return Ok(());
        }
        if inflight.is_none() {
            // 在途证据读取同样受预算钳制（不因慢查询越过观察边界）。
            if let Ok(operations) =
                tokio::time::timeout_at(deadline, state.app_service.get_current_operations(app_id))
                    .await
            {
                inflight = operations.ok().and_then(|operations| {
                    operations.into_iter().find(|operation| {
                        operation.scope == shared_types::UserAppOperationScope::Dev
                            && !operation.state.is_terminal()
                    })
                });
            }
        }
        tokio::time::sleep(
            CONNECT_RETRY_DELAY
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
    let locale = shared_types::current_request_locale();
    if let Some(operation) = inflight {
        warn!(
            app_id,
            operation_id = %operation.operation_id,
            kind = ?operation.kind,
            "dev container unreachable while a Dev-scope operation is in flight"
        );
        return Err(HttpResultError::from_app_error(
            shared_types::AppError::with_message(
                shared_types::error_codes::ERR_OPERATION_IN_PROGRESS,
                shared_types::get_error_message(
                    shared_types::error_codes::ERR_OPERATION_IN_PROGRESS,
                    locale,
                ),
            )
            .with_operation_id(operation.operation_id),
        ));
    }
    warn!(
        app_id,
        "dev container service path unreachable within the connect-wait budget"
    );
    Err(HttpResultError::unavailable_with_code(
        shared_types::error_codes::ERR_CONTAINER_ADDRESS_NOT_READY,
        shared_types::t("error.dev_container_unreachable", locale),
        10,
    ))
}

/// 单次 TCP 连接尝试（2s 单次上限，钳制到剩余 deadline）——连接失败/
/// 超时一律 false，由调用方决定重试节奏（与 prod 预检同款常量）。
async fn tcp_connect_once_within(host: &str, port: u16, deadline: tokio::time::Instant) -> bool {
    const CONNECT_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let attempt_deadline = (tokio::time::Instant::now() + CONNECT_ATTEMPT_TIMEOUT).min(deadline);
    matches!(
        tokio::time::timeout_at(
            attempt_deadline,
            tokio::net::TcpStream::connect((host, port))
        )
        .await,
        Ok(Ok(_))
    )
}

/// 全量透传一个请求到该 app 生产运行容器的 file-server-proxy（同 path+query）。
///
/// 定位语义（与 pod ensure prod 分支同款）：
/// 1. 存在性检查（`get_app`）——读取应用详情并区分不存在与查询失败；
///    后续唤醒还会在协调器内独立验证生命周期与物理资源身份；
/// 2. 唤醒——闲置回收（scale 0）的 app 自动拉起（用户拍板：文件操作前容器没启动
///    要自动启动）；Timeout/Failed → 503 + Retry-After（对齐 proxy_http 流量唤醒）；
/// 3. 地址——K8s 确定性命名 FQDN（Service 换 Pod DNS 自愈）；Docker 直查容器 IPv4
///    （容器名 DNS 可能返回 AAAA 而容器内 file-server 只 bind IPv4）。
///
/// Starting 由生命周期协调器等待到 Running。该观察不证明实际 Service
/// 路径已可连接；转发层还需处理连接建立前的短暂不可达窗口。
pub(crate) async fn forward_to_prod(state: &AppState, app_id: &str, req: Request) -> Response {
    use shared_types::AppWakeControl;

    let deadline = tokio::time::Instant::now() + state.activity.wake_timeout();
    let addr = match resolve_prod_addr(state, app_id).await {
        Ok(addr) => addr,
        Err(resp) => return super::error_body::reject(req, *resp).await,
    };
    if let Err(error) = wait_for_prod_service(state, app_id, &addr, deadline).await {
        return super::error_body::reject(req, error.into_response()).await;
    }
    forward_to_configured_addr(
        state,
        shared_types::UserappStage::Prod,
        "prod runtime",
        app_id,
        &addr,
        req,
        deadline,
    )
    .await
}

/// The request body is a single-use stream. Check the real Service path before
/// moving it into reqwest, so a connection failure never triggers body replay.
async fn wait_for_prod_service(
    state: &AppState,
    app_id: &str,
    addr: &str,
    deadline: tokio::time::Instant,
) -> Result<(), HttpResultError> {
    use shared_types::AppWakeControl;

    const CONNECT_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
    let uri = addr.parse::<reqwest::Url>().map_err(|error| {
        HttpResultError::bad_gateway(format!("Invalid production runtime address: {error}"))
    })?;
    let host = uri
        .host_str()
        .ok_or_else(|| HttpResultError::bad_gateway("Production runtime address has no host"))?;
    let port = uri
        .port_or_known_default()
        .ok_or_else(|| HttpResultError::bad_gateway("Production runtime address has no port"))?;
    let mut checked_runtime = false;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(HttpResultError::service_unavailable(
                format!("app {app_id} service connection timed out; retry later"),
                WAKE_503_RETRY_AFTER_SECS,
            ));
        }
        let attempt_deadline = (now + CONNECT_ATTEMPT_TIMEOUT).min(deadline);
        if matches!(
            tokio::time::timeout_at(
                attempt_deadline,
                tokio::net::TcpStream::connect((host, port))
            )
            .await,
            Ok(Ok(_))
        ) {
            return Ok(());
        }
        if !checked_runtime {
            checked_runtime = true;
            match tokio::time::timeout_at(deadline, state.activity.remote_wake_state_fresh(app_id))
                .await
            {
                Ok(shared_types::RemoteWakeState::WakePending) => {
                    match tokio::time::timeout_at(deadline, state.activity.ensure_running(app_id))
                        .await
                    {
                        Ok(
                            shared_types::WakeOutcome::Ready
                            | shared_types::WakeOutcome::AlreadyRunning,
                        ) => {}
                        Ok(
                            shared_types::WakeOutcome::Failed(failure)
                            | shared_types::WakeOutcome::Timeout(failure),
                        ) => {
                            return Err(HttpResultError::from_app_error(failure.into_app_error()));
                        }
                        Ok(shared_types::WakeOutcome::Blocked { message, blocker }) => {
                            return Err(HttpResultError::from_app_error(
                                shared_types::AppError::conflict(&message).with_blocker(blocker),
                            ));
                        }
                        Err(_) => {
                            return Err(HttpResultError::from_app_error(
                                shared_types::WakeFailure::timeout("wake_wait", None, false)
                                    .into_app_error(),
                            ));
                        }
                    }
                }
                Ok(shared_types::RemoteWakeState::Running) => {}
                Ok(shared_types::RemoteWakeState::Unavailable) => {
                    return Err(HttpResultError::bad_gateway(format!(
                        "app {app_id} runtime is unavailable"
                    )));
                }
                Err(_) => {
                    return Err(HttpResultError::service_unavailable(
                        format!("app {app_id} runtime check timed out; retry later"),
                        WAKE_503_RETRY_AFTER_SECS,
                    ));
                }
            }
        }
        tokio::time::sleep(
            CONNECT_RETRY_DELAY
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}

/// 定位（含唤醒）生产运行容器 file-server addr（`http://{host}:60000`）。
async fn resolve_prod_addr(state: &AppState, app_id: &str) -> Result<String, Box<Response>> {
    if let Err(e) = state.app_service.get_app(app_id).await {
        info!("[USERAPP_FORWARD] prod forward target check failed: app_id={app_id}: {e}");
        return Err(Box::new(shared_types::AppError::from(e).into_response()));
    }
    // 唤醒（stopped 或 starting 时触发——Running 高频文件操作零开销）。
    // is_stopped 为内存视图，远端状态查询兜底多副本/重启后的漂移。
    use shared_types::AppWakeControl;
    if state.activity.is_stopped(app_id) || state.activity.remote_wake_pending(app_id).await {
        match state.activity.ensure_running(app_id).await {
            shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
            shared_types::WakeOutcome::Timeout(failure)
            | shared_types::WakeOutcome::Failed(failure) => {
                warn!(%app_id, code = %failure.code, stage = %failure.stage, "Production wake failed");
                return Err(Box::new(failure.into_app_error().into_response()));
            }
            shared_types::WakeOutcome::Blocked { message, blocker } => {
                return Err(Box::new(
                    shared_types::AppError::conflict(&message)
                        .with_blocker(blocker)
                        .into_response(),
                ));
            }
        }
    }
    // 地址解析
    let host = if shared_types::is_kubernetes_runtime() {
        use shared_types::ContainerLookup;
        state.projects.find_app_runtime_addr(app_id)
    } else {
        // Docker：直查容器 IPv4（同 pod restart 的 Userapp 定位模式）
        state
            .runtime()
            .get_container_info_by_identifier(app_id, &shared_types::ServiceType::Userapp)
            .await
            .map_err(|error| {
                Box::new(
                    container_runtime_api::runtime_app_error(
                        &error,
                        "production_address_lookup",
                        container_runtime_api::RuntimeErrorContext::ReadOnly,
                    )
                    .into_response(),
                )
            })?
            .map(|info| info.container_ip)
            .filter(|ip| !ip.is_empty())
    };
    match host.filter(|h| !h.is_empty()) {
        Some(host) => Ok(format!(
            "http://{host}:{}",
            shared_types::AGENT_FILE_SERVER_PORT
        )),
        None => {
            // 走到这里 = get_app 成功但容器定位失败（回收过渡态等）
            warn!("[USERAPP_FORWARD] prod runtime addr unavailable: app_id={app_id}");
            Err(Box::new(
                shared_types::AppError::with_message(
                    shared_types::ERR_CONTAINER_ADDRESS_NOT_READY,
                    format!("Runtime address for app {app_id} is not ready"),
                )
                .with_error_detail(
                    shared_types::ErrorDetail::new(
                        shared_types::ERR_CONTAINER_ADDRESS_NOT_READY,
                        "production_address_lookup",
                        "Container exists but no routable address is available",
                    )
                    .with_retryable(true),
                )
                .into_response(),
            ))
        }
    }
}

/// 唤醒 503 的 Retry-After 秒数（对齐 proxy_http 流量唤醒面）。
const WAKE_503_RETRY_AFTER_SECS: u32 = 15;

#[cfg(test)]
mod control_response_tests {
    use super::*;

    #[tokio::test]
    async fn legacy_builder_errors_preserve_operation_identity_and_business_code() {
        let cases = [
            (
                anyhow::Error::from(shared_types::UserAppWaitTimeout {
                    operation_id: Some("accepted-builder".into()),
                }),
                shared_types::error_codes::ERR_USERAPP_WAIT_TIMEOUT,
                Some("accepted-builder"),
            ),
            (
                anyhow::Error::new(crate::userapp_builder::BuilderEnsureSuperseded {
                    operation_id: "interrupted-ensure".into(),
                }),
                shared_types::error_codes::ERR_CONFLICT,
                Some("interrupted-ensure"),
            ),
            (
                anyhow::Error::from(shared_types::UserAppStoreError::OperationInProgress(
                    shared_types::UserAppOperationBlocker {
                        scope: shared_types::UserAppOperationScope::Prod,
                        operation_id: "conflicting-operation".into(),
                        kind: shared_types::UserAppOperationKind::Start,
                        state: shared_types::UserAppOperationState::Running,
                        step: "claimed".into(),
                    },
                )),
                shared_types::error_codes::ERR_OPERATION_IN_PROGRESS,
                None,
            ),
        ];
        for (error, code, operation_id) in cases {
            let response = *builder_control_response(&error.context("Forward builder lookup"));
            assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
            let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
                .await
                .expect("error envelope");
            let envelope: serde_json::Value = serde_json::from_slice(&body).expect("JSON envelope");
            assert_eq!(envelope["code"], code);
            if let Some(operation_id) = operation_id {
                assert_eq!(envelope["operation_id"], operation_id);
            } else {
                assert!(envelope.get("operation_id").is_none());
                assert_eq!(
                    envelope["data"]["holder_operation_id"],
                    "conflicting-operation"
                );
                assert_eq!(envelope["blocker"]["operation_id"], "conflicting-operation");
                assert_eq!(envelope["data"]["retryable"], false);
            }
            assert_eq!(envelope["success"], false);
        }
    }
}

#[cfg(test)]
#[path = "upstream_wait_tests.rs"]
mod waitable_conflict_tests;

#[cfg(test)]
#[path = "upstream_diagnostics_tests.rs"]
mod diagnostics_forward_tests;

#[cfg(test)]
mod implementation_file_boundary_baseline_tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    #[tokio::test]
    async fn file_forward_does_not_leak_rcoder_control_key_to_file_peer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/api/v1/userapp/files",
            get(|headers: HeaderMap| async move {
                if headers.contains_key("x-api-key") {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::OK
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let request = Request::builder()
            .uri("/api/v1/userapp/files")
            .header("x-api-key", "configured-primary-control-key")
            .body(Body::empty())
            .unwrap();
        let response = forward_to_addr(
            "file peer",
            "fixtureapp",
            &format!("http://{address}"),
            &shared_types::FileServerRequestCredentials::default(),
            request,
        )
        .await;
        server.abort();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the file peer must not receive the primary RCoder control key"
        );
    }

    /// 事故反例回归（2026-10-08 app 221）：builder 重启窗口内连接失败——
    /// 不得把裸 reqwest 错误（含集群内部 svc 地址）直通用户；连接类失败
    /// 分类为 ERR_CONTAINER_ADDRESS_NOT_READY + Retry-After + 本地化文案。
    #[tokio::test]
    async fn dev_forward_connect_failure_is_classified_not_raw() {
        // 绑定后立即释放：拿到一个几乎必然拒绝连接的端口。
        let address = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap()
        };
        let request = Request::builder()
            .uri("/api/v1/userapp/dev/restart")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let response = forward_to_addr(
            "dev",
            "fixture-221",
            &format!("http://{address}"),
            &shared_types::FileServerRequestCredentials::default(),
            request,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("10")
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["code"],
            shared_types::error_codes::ERR_CONTAINER_ADDRESS_NOT_READY
        );
        let message = payload["message"].as_str().unwrap_or_default();
        assert!(
            !message.contains("error sending request")
                && !message.contains("http://")
                && !message.is_empty(),
            "连接失败文案必须分类净化（非空、无 reqwest 原文/内部地址）: {message}"
        );
    }

    /// 预检核：端口可连立即 true；不可连在 deadline 内 false（供
    /// wait_for_dev_service 的循环复用语义）。
    #[tokio::test]
    async fn tcp_connect_once_within_bounds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            // 保持监听存活即可；连接由对端建立后立即结束测试。
            while let Ok((_socket, _)) = listener.accept().await {}
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        assert!(tcp_connect_once_within("127.0.0.1", address.port(), deadline).await);

        let refused = {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            probe.local_addr().unwrap()
        };
        let tight_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
        assert!(
            !tcp_connect_once_within("127.0.0.1", refused.port(), tight_deadline).await,
            "拒绝连接的端口在紧预算内应返回 false"
        );
    }
}
