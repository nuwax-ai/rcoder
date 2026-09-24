//! UserappBuilder 开发容器 ensure 与定位。
//!
//! 跨域公共入口：文件转发层（`userapp_forward`）、chat 开发对话、create-workspace、
//! start/restart 部署链共用——注册表命中复用，miss 创建注册。
//!
//! 构建任务本体在 agent-runner 容器内 file-server（`/api/v1/userapp/build` + tasks 查询），
//! rcoder 不再做发布任务编排（旧 publish 任务体系已随 `/api/v1/userapp/publish` 接口族删除）。

pub mod adoption;
pub mod app_adoption;
pub(crate) mod auto_repair;
pub mod compute_control;
pub mod control;
mod creation;
mod dev_cleanup;
mod dev_locator;
mod lifecycle;
mod recovery;
pub(crate) mod retry;
pub mod shutdown_gate;

pub use dev_cleanup::UserappDevResourcesCleanup;
pub use dev_locator::UserappDevLocator;
pub(crate) use recovery::start_recovery;

use std::sync::{Arc, Weak};

use anyhow::{Context, Result, anyhow};
use container_runtime_api::ContainerCreateParams;
// 存储契约 trait：state.projects（ProjectStoreBackend 枚举）上的方法经此解析
use shared_types::ProjectStore as _;
use shared_types::{
    AGENT_FILE_SERVER_PORT, ContainerBasicInfo, ProjectAndContainerInfo, ServiceType,
};
use tracing::info;

use crate::app_state::AppState;

/// UserappBuilder per-app PVC 默认大小(后续可提到 config.yml 的 user-app-builder.service 段)。
const DEFAULT_BUILDER_STORAGE_SIZE: &str = "100Gi";

/// 探活失败后的注册表自愈裁决：以容器运行时**真实状态**（实时 inspect）为准，
/// 不再凭单次探活失败即判死——编译高负载探活超时、新容器启动窗口未就绪等
/// 抖动场景下，容器实际 Running，杀重建会把正在跑的任务连同容器一起蒸发。
pub(crate) enum RegistryRemediation {
    /// 容器真实 Running：探活失败是超时/未就绪抖动。注册已按 inspect 真实值
    /// 刷新（治"注册表死 IP"本源），调用方继续使用返回的 info，**不杀不重建**
    /// （透传由本次请求定成败）。
    Alive(ContainerBasicInfo),
    /// Only an authoritative absent/stopped result permits the rebuild path.
    Gone,
}

/// 探活失败自愈裁决（[`RegistryRemediation`] 的判定体）。
///
/// `find_container` 的 Docker 实现为实时 inspect（404 不缓存）、K8s 为 pod
/// get/label list——两后端 Running 语义一致，天然覆盖。
///
/// `instance`：复合 identifier（`{user_id}-{app_id}`，自描述——实例归属经
/// [`shared_types::parse_builder_instance_id`] 派生）。
pub(crate) async fn remediate_stale_registry(
    state: &AppState,
    app_id: &str,
    instance: &str,
) -> Result<RegistryRemediation> {
    let existing = registered_builder(state, instance)
        .ok_or_else(|| anyhow!("Builder registration changed during inspection: {instance}"))?;
    match cross_verify_registration(state, app_id, instance, &existing).await? {
        Some(info) => Ok(RegistryRemediation::Alive(info)),
        None => Ok(RegistryRemediation::Gone),
    }
}

/// 注册刷新构造（纯函数）：以 inspect 真实值覆盖注册的标识/地址/状态字段
/// （service_url 随 IP 同步重算），端口等其余字段沿用注册旧值；与注册完全
/// 一致时返回 None（零写）。
fn refreshed_registration(
    existing: &ContainerBasicInfo,
    rc: &container_runtime_api::RuntimeContainerInfo,
) -> Option<ContainerBasicInfo> {
    let updated = ContainerBasicInfo {
        container_id: rc.container_id.clone(),
        container_name: rc.container_name.clone(),
        container_ip: rc.container_ip.clone(),
        status: String::from(rc.status.clone()),
        created_at: rc.created_at,
        service_url: format!("http://{}:{}", rc.container_ip, existing.internal_port),
        workload_uid: rc.workload_uid.clone(),
        ..existing.clone()
    };
    (&updated != existing).then_some(updated)
}

/// 确保 UserappBuilder 开发容器存在（幂等）并返回容器信息。
///
/// 应用共享：按 app_id 定位唯一 dev 容器（无用户维度），miss 创建注册。
pub async fn ensure_userapp_builder(state: &AppState, app_id: &str) -> Result<ContainerBasicInfo> {
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(state.config.userapp_storage.ensure_timeout_seconds);
    within_builder_deadline(
        deadline,
        ensure_builder_target_until(state, app_id, deadline, true),
    )
    .await
}

pub(crate) async fn ensure_userapp_builder_until(
    state: &AppState,
    app_id: &str,
    deadline: tokio::time::Instant,
) -> Result<ContainerBasicInfo> {
    // deadline 包裹由调用方负责（ensure_userapp_builder 包装/upstream 自带
    // timeout_at）——此处只透传，保持既有错误形态。
    ensure_builder_target_until(state, app_id, deadline, false).await
}

/// ensure 体：应用共享模型——builder identifier == 纯 app_id，恒走
/// lifecycle 受理（admit/恢复/围栏）。
async fn ensure_builder_target_until(
    state: &AppState,
    app_id: &str,
    deadline: tokio::time::Instant,
    explicit_start: bool,
) -> Result<ContainerBasicInfo> {
    let recovered = adoption::discover_missing_identity(state, app_id).await?;
    state
        .app_service
        .verify_recovered_storage(app_id, shared_types::UserAppOperationScope::Dev)
        .await?;
    state
        .userapp_store
        .check_compute_access(
            app_id,
            shared_types::UserAppOperationScope::Dev,
            explicit_start,
        )
        .await?;
    let instance = app_id;
    let _lifecycle = lifecycle::acquire(instance).await;
    // app_id 长度 Fail Fast（仅新建路径——注册命中说明历史上已建成，不受限）：
    // STS pod 的 controller-revision-hash label =
    // `rcoder-app-builder-{app_id}-{10位hash}` 受 63 字节限，超长必然 FailedCreate
    // 且表象含糊（ensure 500/连接超时，真因只在 kubectl events）。入口明确拒绝
    //（229 全链 e2e 实测抓出的既有纪律，共享模型沿用）。
    if instance.chars().count() > shared_types::USERAPP_APP_ID_MAX_LEN
        && registered_builder(state, instance).is_none()
    {
        return Err(anyhow!(
            "builder app_id length {} exceeds {} (K8s StatefulSet label 63-byte limit; \
             see USERAPP_APP_ID_MAX_LEN)",
            instance.chars().count(),
            shared_types::USERAPP_APP_ID_MAX_LEN
        ));
    }
    let identity = match recovered {
        Some(identity) => identity,
        None => state.userapp_store.ensure_identity(app_id).await?,
    };
    if let Some(repair) = auto_repair::repair_if_needed(state, app_id, None).await? {
        return Err(anyhow!(
            "Published builder address unavailable; automatic repair operation={} state={:?}",
            repair.operation_id,
            repair.state
        ));
    }
    if !builder_fenced_by_current_operation(state, &identity).await?
        && let Some(info) = registered_or_discovered_builder(state, instance).await?
        && let Some(verified) = cross_verify_registration(state, app_id, instance, &info).await?
    {
        return Ok(verified);
    }
    creation::ensure(state, app_id, instance, _lifecycle, deadline).await
}

/// 当前操作是否围栏 builder：dev 槽（Ensure/Adopt/Stop/Restart/DestroyDev/
/// ClearDevStorage）或 application 槽（DeleteApplication/PurgeResources）在途
/// 即围栏——生产部署等 prod 槽操作进行中，已验证的注册继续服务（部署期
/// app-cli 经 rcoder static 转发下载制品依赖此路径，否则新受理会与在途部署
/// 操作自冲突）。槽位即语义：无需再按 kind 查询判定。
async fn builder_fenced_by_current_operation(
    _state: &AppState,
    identity: &shared_types::UserAppLifecycleRecord,
) -> Result<bool> {
    Ok(
        identity.active_operations.dev.is_some()
            || identity.active_operations.application.is_some(),
    )
}

pub async fn ensure_userapp_builder_probed(
    state: &AppState,
    app_id: &str,
) -> Result<(ContainerBasicInfo, bool)> {
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(state.config.userapp_storage.ensure_timeout_seconds);
    within_builder_deadline(
        deadline,
        ensure_userapp_builder_probed_until(state, app_id, deadline),
    )
    .await
}

async fn ensure_userapp_builder_probed_until(
    state: &AppState,
    app_id: &str,
    deadline: tokio::time::Instant,
) -> Result<(ContainerBasicInfo, bool)> {
    let recovered = adoption::discover_missing_identity(state, app_id).await?;
    state
        .app_service
        .verify_recovered_storage(app_id, shared_types::UserAppOperationScope::Dev)
        .await?;
    state
        .userapp_store
        .check_compute_access(app_id, shared_types::UserAppOperationScope::Dev, false)
        .await?;
    let instance = app_id;
    let _lifecycle = lifecycle::acquire(instance).await;
    if instance.chars().count() > shared_types::USERAPP_APP_ID_MAX_LEN
        && registered_builder(state, instance).is_none()
    {
        return Err(anyhow!(
            "builder app_id length {} exceeds {} (K8s StatefulSet label 63-byte limit; \
             see USERAPP_APP_ID_MAX_LEN)",
            instance.chars().count(),
            shared_types::USERAPP_APP_ID_MAX_LEN
        ));
    }
    let identity = match recovered {
        Some(identity) => identity,
        None => state.userapp_store.ensure_identity(app_id).await?,
    };
    if let Some(repair) = auto_repair::repair_if_needed(state, app_id, None).await? {
        return Err(anyhow!(
            "Published builder address unavailable; automatic repair operation={} state={:?}",
            repair.operation_id,
            repair.state
        ));
    }
    let fenced = builder_fenced_by_current_operation(state, &identity).await?;

    /// 就绪裁决后创建（lifecycle admit 路径）。`lease`：进程内互斥，
    /// 创建期间持有（移交 worker）。
    async fn rebuild(
        state: &AppState,
        app_id: &str,
        instance: &str,
        deadline: tokio::time::Instant,
        lease: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<ContainerBasicInfo> {
        creation::ensure(state, app_id, instance, lease, deadline).await
    }

    if !fenced && let Some(info) = registered_or_discovered_builder(state, instance).await? {
        let addr = dev_file_server_addr(state, &info)?;
        if probe_file_server(&addr).await {
            // 探活过 ≠ 归属正确：跨族污染形态下生产容器的 file-server 同样在
            // 60000 应答（探活恒过、remediation 永不触发）——追加归属交叉校验
            if let Some(updated) = cross_verify_registration(state, app_id, instance, &info).await?
            {
                return Ok((updated, false));
            }
            // Creation validates the runtime independently; do not erase a
            // registration that may have changed while probing.
            return Ok((
                rebuild(state, app_id, instance, deadline, _lifecycle).await?,
                true,
            ));
        }
        tracing::warn!(
            "[USERAPP_ENSURE] dev container probe failed (stale registry?), verifying container state: app_id={app_id}, addr={addr}"
        );
        // 先验容器真实状态再决定处置：Running 则保容器（探活失败是超时/未就绪
        // 抖动），只有真死才清注册重建——防误杀正在跑任务的容器
        match remediate_stale_registry(state, app_id, instance).await? {
            RegistryRemediation::Alive(info) => {
                tracing::info!(
                    "[USERAPP_ENSURE] dev container alive on inspect, keep without rebuild: app_id={app_id}"
                );
                return Ok((info, false));
            }
            RegistryRemediation::Gone => {
                // Publish a verified replacement only after durable creation.
                let info = rebuild(state, app_id, instance, deadline, _lifecycle).await?;
                return Ok((info, true));
            }
        }
    }
    let info = rebuild(state, app_id, instance, deadline, _lifecycle).await?;
    Ok((info, true))
}

/// Covers all waiter work, including metadata/runtime reads before admission
/// and physical identity verification after completion. Spawned creation owns
/// its lease independently and is not cancelled by dropping this waiter.
async fn within_builder_deadline<T>(
    deadline: tokio::time::Instant,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    if tokio::time::Instant::now() >= deadline {
        return Err(shared_types::UserAppWaitTimeout { operation_id: None }.into());
    }
    tokio::time::timeout_at(deadline, work)
        .await
        .map_err(|_| shared_types::UserAppWaitTimeout { operation_id: None })?
}

/// Preserve typed lifecycle failures through anyhow context into HTTP envelopes.
/// Never classify an operation by searching its formatted error text.
pub fn control_error(error: &anyhow::Error) -> shared_types::AppError {
    if let Some(control) = error.downcast_ref::<shared_types::BuilderControlError>() {
        return shared_types::AppError::with_message(
            shared_types::error_codes::ERR_BACKEND_ERROR,
            control.message.clone(),
        )
        .with_operation_id(control.operation_id.clone());
    }
    if let Some(timeout) = error.downcast_ref::<shared_types::UserAppWaitTimeout>() {
        let response = shared_types::AppError::with_message(
            shared_types::error_codes::ERR_USERAPP_WAIT_TIMEOUT,
            "Builder ensure deadline exceeded; the accepted operation may still be running",
        );
        return match &timeout.operation_id {
            Some(id) => response.with_operation_id(id.clone()),
            None => response,
        };
    }
    if let Some(shared_types::UserAppStoreError::OperationInProgress(blocker)) =
        error.downcast_ref::<shared_types::UserAppStoreError>()
    {
        // step-E 观测：Java 信封会丢弃 blocker 字段（blocker-envelope-java-handoff），
        // 这里保证阻塞者身份在 rcoder 日志可查（排障不再依赖响应体）。
        tracing::warn!(
            blocker_operation_id = %blocker.operation_id,
            blocker_kind = ?blocker.kind,
            blocker_state = ?blocker.state,
            blocker_step = %blocker.step,
            blocker_scope = ?blocker.scope,
            "Application operation conflict: admission blocked by an in-flight operation"
        );
        return shared_types::AppError::with_message(
            shared_types::error_codes::ERR_CONFLICT,
            "A conflicting application operation is in progress",
        )
        .with_operation_id(blocker.operation_id.clone())
        .with_blocker(blocker.clone());
    }
    shared_types::AppError::with_message(
        shared_types::error_codes::ERR_BACKEND_ERROR,
        format!("Ensure userApp dev container failed: {error:#}"),
    )
}

/// 探活通过后的**跨族污染交叉校验**（补"探活失败才自愈"的触发缺口）。
///
/// 背景：生产 UserApp 容器与 builder 一样在 60000 跑 file-server——注册表被
/// 跨族污染时（如生产 pod 被写入 builder 注册项），探活恒过、remediation
/// 永不触发，dev 流量持续打向生产容器（vnc/ttyd 6080/7681 拒绝显形为 502，
/// 文件族 60000 则是"错容器成功"更隐蔽）。此处以带类型分流的
/// `find_container` 真实值与注册值比对：不一致即以 inspect 值刷新注册
/// （复用 [`refreshed_registration`]），把自愈触发从"探活失败"扩展到
/// "归属不符"。查询失败返回错误；权威不存在/非 Running 返回 None，禁止复用旧地址。
///
/// 成本：一次 pods().get（K8s 单 get，毫秒级）。调用方为低频管理面
/// （ensure_probed）与热路径的 30s 探活缓存 miss 分支，频率受控。
///
/// `app_id`：纯 app_id（lifecycle 查询）；`instance`：复合 identifier（容器
/// 定位/身份校验/实例归属——`{user_id}-{app_id}` 自描述，parse 派生归属）。
pub async fn cross_verify_registration(
    state: &AppState,
    app_id: &str,
    instance: &str,
    registered: &ContainerBasicInfo,
) -> Result<Option<ContainerBasicInfo>> {
    verify_registration(state, app_id, instance, registered, true).await
}

async fn verify_registration(
    state: &AppState,
    app_id: &str,
    instance: &str,
    registered: &ContainerBasicInfo,
    publish: bool,
) -> Result<Option<ContainerBasicInfo>> {
    let Some(rc) = state
        .runtime()
        .find_container(instance, &ServiceType::UserappBuilder)
        .await
        .context("inspect builder registration")?
    else {
        return Ok(None);
    };
    validate_builder_identity(instance, &rc)?;
    // 非 Running（含已删除容器的注册缓存残影）先归 None——调用方按需重建；
    // 此时 verify 的 capture 查无物理负载，会误报"身份变更"。
    if rc.status != container_runtime_api::ContainerRuntimeStatus::Running {
        return Ok(None);
    }
    // 物理身份校验由 verify_live_builder 的运行时 capture 执行（应用共享，
    // 无归属注解可比对）。
    adoption::verify_live_builder(state, app_id, instance, &rc.container_id).await?;
    // A live Pod alone is insufficient when its management Service is absent.
    // Read-side discovery never repairs it; return to admitted creation/resume
    // so repair shares the same durable operation and runtime lease as Stop.
    if state
        .runtime()
        .get_container_info_by_identifier(instance, &ServiceType::UserappBuilder)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let Some(updated) = refreshed_registration(registered, &rc) else {
        return Ok(Some(registered.clone()));
    };
    if !publish {
        return Ok(Some(updated));
    }
    if let Some(mut project) = state.get_project(instance).map(|p| (*p).clone()) {
        project.set_service_type(Some(ServiceType::UserappBuilder));
        project.set_container(Some(updated.clone()));
        state
            .insert_project(instance.to_string(), Arc::new(project))
            .context("persist verified builder registration")?;
    }
    info!(app_id, instance, container_id = %updated.container_id, "Builder registration refreshed from authoritative runtime");
    Ok(Some(updated))
}

fn validate_builder_identity(
    instance: &str,
    actual: &container_runtime_api::RuntimeContainerInfo,
) -> Result<()> {
    if actual.service_type.as_ref() != Some(&ServiceType::UserappBuilder)
        || actual.identity_key() != Some(instance)
    {
        return Err(anyhow!("Builder runtime identity conflict: {instance}"));
    }
    Ok(())
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
pub fn dev_file_server_addr(state: &AppState, info: &ContainerBasicInfo) -> Result<String> {
    let addr = shared_types::build_container_port_addr(
        &info.container_name,
        &info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
        AGENT_FILE_SERVER_PORT,
    )?;
    Ok(format!("http://{addr}"))
}

/// 纯解析:只查 state.projects,无副作用（短路语义 peek 复用——只读判定
/// 容器注册在否，不 ensure 不自愈）。键 = 复合 identifier（`{user_id}-{app_id}`）。
pub(crate) fn registered_builder(state: &AppState, instance: &str) -> Option<ContainerBasicInfo> {
    state
        .projects
        .get(instance)
        .and_then(|p| p.container_info())
}

/// Physical state for `/computer/pod/ensure`. The management file server and
/// app-cli may still be starting or failed; they are not a reason to report a
/// running container as absent or to block an explicit compute restart.
pub enum BuilderComputeState {
    Running(ContainerBasicInfo),
    /// The controller/container still exists and can be started in place.
    StoppedRetained,
    /// Docker AutoRemove removed the container after a confirmed Stop.
    StoppedRemoved,
    Missing,
}

pub async fn inspect_builder_compute(
    state: &AppState,
    app_id: &str,
) -> Result<BuilderComputeState> {
    let app = adoption::discover_missing_identity(state, app_id).await?;
    let Some(app) = app else {
        return Ok(BuilderComputeState::Missing);
    };
    if let Some(actual) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await?
    {
        validate_builder_identity(app_id, &actual)?;
        if actual.status == container_runtime_api::ContainerRuntimeStatus::Running {
            adoption::verify_live_builder(state, app_id, app_id, &actual.container_id).await?;
            state
                .userapp_store
                .check_compute_access(app_id, shared_types::UserAppOperationScope::Dev, false)
                .await?;
            return Ok(BuilderComputeState::Running(ContainerBasicInfo {
                container_id: actual.container_id,
                container_name: actual.container_name,
                container_ip: actual.container_ip,
                internal_port: AGENT_FILE_SERVER_PORT,
                external_port: 0,
                project_id: app_id.into(),
                status: String::from(actual.status),
                created_at: actual.created_at,
                service_url: String::new(),
                workload_uid: actual.workload_uid,
            }));
        }
    }
    let context = shared_types::UserAppExecutionContext {
        app_id: app_id.to_owned(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: "physical-compute-observation".into(),
        executor_id: "reader".into(),
        request_fingerprint: "0".repeat(64),
    };
    let target = adoption::capture_bound_target(state, &context).await?;
    let stopped_intent = state
        .userapp_store
        .compute_desired_stopped(
            app_id,
            &app.lifecycle_id,
            shared_types::UserAppOperationScope::Dev,
        )
        .await?;
    Ok(if target.workload.is_some() {
        BuilderComputeState::StoppedRetained
    } else if stopped_intent {
        BuilderComputeState::StoppedRemoved
    } else {
        BuilderComputeState::Missing
    })
}

/// Registry misses perform an authoritative read before creating anything.
async fn registered_or_discovered_builder(
    state: &AppState,
    instance: &str,
) -> Result<Option<ContainerBasicInfo>> {
    if let Some(info) = registered_builder(state, instance) {
        if dev_file_server_addr(state, &info).is_ok() {
            return Ok(Some(info));
        }
        match state.runtime().refresh_container_reach(&info).await {
            Ok(()) => return Ok(Some(info)),
            Err(container_runtime_api::ContainerRuntimeError::ContainerNotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        // An absent cached physical ID needs ordinary discovery, not a repeated
        // refresh of a container which no longer exists.
    }
    let Some(actual) = state
        .runtime()
        .find_container(instance, &ServiceType::UserappBuilder)
        .await
        .context("discover existing builder")?
    else {
        return Ok(None);
    };
    validate_builder_identity(instance, &actual)?;
    // 物理身份绑定由下方 verify_live_builder 校验（应用共享，无归属注解）。
    if actual.status != container_runtime_api::ContainerRuntimeStatus::Running {
        return Ok(None);
    }
    let info = ContainerBasicInfo {
        container_id: actual.container_id,
        container_name: actual.container_name,
        container_ip: actual.container_ip.clone(),
        internal_port: AGENT_FILE_SERVER_PORT,
        external_port: 0,
        project_id: instance.into(),
        status: String::from(actual.status),
        created_at: actual.created_at,
        service_url: format!("http://{}:{}", actual.container_ip, AGENT_FILE_SERVER_PORT),
        workload_uid: None,
    };
    adoption::verify_live_builder(state, instance, instance, &info.container_id).await?;
    state.runtime().refresh_container_reach(&info).await?;
    register_builder(state, instance, &info)?;
    Ok(Some(info))
}

fn register_builder(state: &AppState, instance: &str, info: &ContainerBasicInfo) -> Result<()> {
    let mut project = state
        .get_project(instance)
        .map(|old| (*old).clone())
        .unwrap_or_else(|| ProjectAndContainerInfo::new(instance.to_owned()));
    project.set_service_type(Some(ServiceType::UserappBuilder));
    project.set_container(Some(info.clone()));
    state
        .insert_project(instance.into(), Arc::new(project))
        .context("register verified UserApp builder")
}

async fn confirm_builder_ready(
    state: &AppState,
    app_id: &str,
    instance: &str,
    info: ContainerBasicInfo,
    deadline: tokio::time::Instant,
) -> Result<ContainerBasicInfo> {
    confirm_builder_ready_inner(state, app_id, instance, info, None, deadline).await
}

async fn confirm_builder_ready_inner(
    state: &AppState,
    app_id: &str,
    instance: &str,
    info: ContainerBasicInfo,
    operation: Option<&shared_types::UserAppOperationRecord>,
    deadline: tokio::time::Instant,
) -> Result<ContainerBasicInfo> {
    tokio::time::timeout_at(deadline, async {
        loop {
            if let Some(operation) = operation
                && let Err(shared_types::UserAppStoreError::OperationInProgress(blocker)) = state
                    .userapp_store
                    .check_compute_access(app_id, shared_types::UserAppOperationScope::Dev, false)
                    .await
                && let Some(control) = state
                    .userapp_store
                    .get_compute_control(app_id, &blocker.operation_id)
                    .await?
                && control.lifecycle_id == operation.lifecycle_id
                && control
                    .interrupted_operations
                    .contains(&operation.operation_id)
            {
                // Creation already returned and released its runtime lease.
                // This loop performs only reads, so no mutation is abandoned.
                return Err(container_runtime_api::ContainerRuntimeError::CreationCancelled.into());
            }
            if let Some(actual) = verify_registration(state, app_id, instance, &info, false).await?
            {
                if actual.container_id != info.container_id {
                    return Err(anyhow!("Builder resource replaced before readiness"));
                }
                if probe_file_server(&dev_file_server_addr(state, &actual)?).await {
                    return Ok(actual);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .context("Builder readiness deadline exceeded")?
}

/// Create a builder without publishing its registration before the confirmation deadline.
///
/// 直接调 `runtime.create_container`(UserappBuilder → `create_agent_container`),
/// **不走 ComputerContainerManager**(避免 ComputerAgentRunner 专属的 lazy_migrate)。
/// `instance` = 纯 app_id（装 project_id 槽位——docker_manager 命名链单一来源，
/// STS/PVC/svc/label/锁名随之派生；应用共享后与 app_id 恒等）。
async fn create_builder_inner(
    state: &AppState,
    app_id: &str,
    instance: &str,
    execution_context: shared_types::UserAppExecutionContext,
) -> Result<ContainerBasicInfo> {
    let bound_target = adoption::capture_bound_target(state, &execution_context).await?;
    state
        .userapp_store
        .check_business_execution(&execution_context)
        .await?;
    let mut params = ContainerCreateParams::builder()
        .execution_context(execution_context.clone())
        .project_id(instance.to_string())
        // 容器内契约（env/profiler）的纯 app_id 显式直传——消费方不再从
        // project_id 复合槽右切（字段语义不重载）
        .builder_app_id(app_id.to_string())
        .service_type(ServiceType::UserappBuilder)
        .storage_size(DEFAULT_BUILDER_STORAGE_SIZE)
        .build();

    params.resource_binding = bound_target.resource_binding;

    let cancelled = params.creation_cancelled.clone();
    let creation = state.runtime().create_container(params);
    tokio::pin!(creation);
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
    let container_info = loop {
        tokio::select! {
            result = &mut creation => break result.context("ensure UserappBuilder failed")?,
            _ = poll.tick(), if !cancelled.load(std::sync::atomic::Ordering::Acquire) => {
                if let Err(shared_types::UserAppStoreError::OperationInProgress(blocker)) = state.userapp_store
                    .check_compute_access(app_id, shared_types::UserAppOperationScope::Dev, false).await
                    && let Ok(Some(control)) = state.userapp_store.get_compute_control(app_id, &blocker.operation_id).await
                    && control.lifecycle_id == execution_context.lifecycle_id
                    && control.interrupted_operations.contains(&execution_context.operation_id)
                {
                    cancelled.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        }
    };

    info!(
        "[USERAPP_BUILDER] UserappBuilder ensured: app_id={}, instance={}, container={}, ip={}",
        app_id, instance, container_info.container_name, container_info.container_ip
    );
    Ok(container_info)
}

/// 供 bin 装配（main.rs）构造 Pingora 代理的 dev 容器懒启动回调
/// （`UserappDevLocator` 实现 `UserappDevEnsure` 契约；`new` 为 crate 内可见）。
pub fn dev_ensure_for_proxy(state: Weak<AppState>) -> Arc<UserappDevLocator> {
    Arc::new(UserappDevLocator::new(state))
}

#[cfg(test)]
mod remediation_tests {
    use super::*;
    use chrono::Utc;
    use container_runtime_api::{ContainerRuntimeStatus, RuntimeContainerInfo};
    use shared_types::ContainerBasicInfo;

    fn registered(ip: &str, id: &str) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: id.to_string(),
            container_name: "rcoder-app-builder-app-1".to_string(),
            container_ip: ip.to_string(),
            internal_port: 60000,
            external_port: 0,
            project_id: "app1".to_string(),
            status: "Running".to_string(),
            created_at: Utc::now(),
            service_url: format!("http://{ip}:60000"),
            workload_uid: None,
        }
    }

    fn inspected(ip: &str, id: &str) -> RuntimeContainerInfo {
        RuntimeContainerInfo {
            container_id: id.to_string(),
            container_name: "rcoder-app-builder-app-1".to_string(),
            container_ip: ip.to_string(),
            status: ContainerRuntimeStatus::Running,
            created_at: Utc::now(),
            env_vars: None,
            service_type: Some(ServiceType::UserappBuilder),
            project_id: None,
            user_id: None,
            pod_id: None,
            app_id: Some("app1".to_string()),
            workload_uid: None,
        }
    }

    /// IP 漂移（重建后注册残留死 IP 的本源场景）：刷新为新值，service_url
    /// 同步重算，端口等其余字段沿用注册旧值。
    #[test]
    fn refreshed_registration_updates_ip_and_service_url() {
        let existing = registered("192.168.97.10", "id-old");
        let out = refreshed_registration(&existing, &inspected("192.168.97.8", "id-new"))
            .expect("ip drift must refresh");
        assert_eq!(out.container_ip, "192.168.97.8");
        assert_eq!(out.service_url, "http://192.168.97.8:60000");
        assert_eq!(out.container_id, "id-new");
        assert_eq!(out.internal_port, 60000, "port fields preserved");
    }

    /// 注册与 inspect 完全一致（探活失败是纯抖动）：零写（None）。
    #[test]
    fn refreshed_registration_no_write_when_identical() {
        let existing = registered("192.168.97.8", "id-x");
        let rc = inspected("192.168.97.8", "id-x");
        // created_at 取 rc 的值——对齐构造语义重造一份与输出一致的输入
        let existing = ContainerBasicInfo {
            created_at: rc.created_at,
            status: String::from(rc.status.clone()),
            workload_uid: None,
            ..existing
        };
        assert!(
            refreshed_registration(&existing, &rc).is_none(),
            "identical registration must be zero-write"
        );
    }

    /// 仅容器 id 变化（同名重建新实例）：也要刷新（id 是 rm/inspect 的键）。
    #[test]
    fn refreshed_registration_updates_on_id_change() {
        let existing = registered("192.168.97.8", "id-old");
        assert!(refreshed_registration(&existing, &inspected("192.168.97.8", "id-new")).is_some());
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::within_builder_deadline;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::time::{Duration, Instant};

    #[tokio::test]
    async fn expired_deadline_does_not_poll_new_work() {
        let executed = AtomicBool::new(false);
        let result = within_builder_deadline(Instant::now(), async {
            executed.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(result.is_err());
        assert!(!executed.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn nested_waits_share_one_deadline() {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(10);
        let result = within_builder_deadline(deadline, async {
            tokio::time::sleep(Duration::from_secs(7)).await;
            within_builder_deadline(deadline, async {
                tokio::time::sleep(Duration::from_secs(7)).await;
                Ok(())
            })
            .await
        })
        .await;
        assert!(result.is_err());
        assert_eq!(Instant::now() - started, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn waiter_timeout_does_not_cancel_an_accepted_worker() {
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = completed.clone();
        let worker = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            worker_completed.store(true, Ordering::SeqCst);
        });
        let result = within_builder_deadline(
            Instant::now() + Duration::from_secs(5),
            std::future::pending::<anyhow::Result<()>>(),
        )
        .await;
        assert!(result.is_err());
        assert!(!completed.load(Ordering::SeqCst));
        worker.await.expect("accepted worker completion");
        assert!(completed.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod control_error_tests {
    use super::control_error;
    use axum::{body::to_bytes, response::IntoResponse as _};

    #[tokio::test]
    async fn wrapped_timeout_preserves_code_and_operation_identity() {
        let error = anyhow::Error::new(shared_types::UserAppWaitTimeout {
            operation_id: Some("accepted-builder".into()),
        })
        .context("upstream lookup");
        let response = control_error(&error).into_response();
        assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let envelope: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(
            envelope["code"],
            shared_types::error_codes::ERR_USERAPP_WAIT_TIMEOUT
        );
        assert_eq!(envelope["operation_id"], "accepted-builder");
        assert_eq!(envelope["success"], false);
    }

    #[tokio::test]
    async fn conflict_classification_requires_a_typed_cause() {
        let blocker = shared_types::UserAppOperationBlocker {
            scope: shared_types::UserAppOperationScope::Dev,
            operation_id: "owner-operation".into(),
            kind: shared_types::UserAppOperationKind::RestartBuilder,
            state: shared_types::UserAppOperationState::RecoveryRequired,
            step: "claimed".into(),
        };
        let error = anyhow::Error::new(shared_types::UserAppStoreError::OperationInProgress(
            blocker,
        ))
        .context("admission");
        let response = control_error(&error).into_response();
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let envelope: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(envelope["code"], shared_types::error_codes::ERR_CONFLICT);
        assert_eq!(envelope["operation_id"], "owner-operation");
        let text_only = anyhow::anyhow!("Application operation in progress: owner-operation");
        let response = control_error(&text_only).into_response();
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let envelope: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(
            envelope["code"],
            shared_types::error_codes::ERR_BACKEND_ERROR
        );
        assert!(envelope.get("operation_id").is_none());
    }
}
