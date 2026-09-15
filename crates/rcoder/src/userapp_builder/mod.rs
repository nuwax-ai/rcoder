//! UserappBuilder 开发容器 ensure 与定位。
//!
//! 跨域公共入口：文件转发层（`userapp_forward`）、chat 开发对话、create-workspace、
//! start/restart 部署链共用——注册表命中复用，miss 创建注册。
//!
//! 构建任务本体在 agent-runner 容器内 file-server（`/api/v1/userapp/build` + tasks 查询），
//! rcoder 不再做发布任务编排（旧 publish 任务体系已随 `/api/v1/userapp/publish` 接口族删除）。

pub(crate) mod adoption;
pub(crate) mod control;
mod creation;
mod dev_cleanup;
mod dev_locator;
mod lifecycle;
mod recovery;
pub(crate) mod retry;

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
    build_backend_addr,
};
use tracing::info;

use crate::router::AppState;

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
        ..existing.clone()
    };
    (&updated != existing).then_some(updated)
}

/// 确保 UserappBuilder 开发容器存在（幂等）并返回容器信息。
///
/// `explicit_user_id`：请求入参显式携带的 owner（优先档；`None`/空白视为未传，
/// 走 metadata 注册值）。新建容器时用于组装宿主树 `dev/{user_id}/{app_id}`。
pub(crate) async fn ensure_userapp_builder(
    state: &AppState,
    app_id: &str,
    explicit_user_id: Option<&str>,
) -> Result<ContainerBasicInfo> {
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(state.config.userapp_storage.ensure_timeout_seconds);
    within_builder_deadline(
        deadline,
        ensure_userapp_builder_until(state, app_id, explicit_user_id, deadline),
    )
    .await
}

pub(crate) async fn ensure_userapp_builder_until(
    state: &AppState,
    app_id: &str,
    explicit_user_id: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<ContainerBasicInfo> {
    let target = resolve_dev_target(
        explicit_user_id,
        state.app_service.get_app_owner(app_id).await?.as_deref(),
    )
    .with_context(|| {
        format!("cannot resolve dev instance user for app {app_id}; pass user_id explicitly")
    })?;
    // deadline 包裹由调用方负责（ensure_userapp_builder 包装/upstream 自带
    // timeout_at）——此处只透传，保持既有错误形态。
    ensure_builder_target_until(state, app_id, &target, deadline).await
}

/// 双路径 ensure 体（`ensure_userapp_builder_until` /
/// `ensure_userapp_builder_probed_until` 共用）：owner 实例走 lifecycle 受理
/// （admit/恢复/围栏，语义不变）；非 owner 协作者实例走轻量路径（复合键
/// 进程内锁 + docker_manager 侧 K8s operation 锁 fence，不经 lifecycle
/// admit——app 级 operation 会让跨用户并发 ensure 误撞
/// OperationInProgress）。两路径的容器命名/注册表/探活键统一为复合
/// identifier。
async fn ensure_builder_target_until(
    state: &AppState,
    app_id: &str,
    target: &DevTarget,
    deadline: tokio::time::Instant,
) -> Result<ContainerBasicInfo> {
    let instance = target.instance_id(app_id)?;
    let _lifecycle = lifecycle::acquire(&instance).await;
    // 复合 identifier 长度 Fail Fast（仅新建路径——注册命中说明历史上已
    // 建成，不受限）：STS pod 的 controller-revision-hash label =
    // `rcoder-app-builder-{user_id}-{app_id}-{10位hash}` 受 63 字节限，超长
    // 必然 FailedCreate 且表象含糊（ensure 500/连接超时，真因只在 kubectl
    // events）。入口明确拒绝（229 全链 e2e 实测抓出的既有纪律，复合键化沿用）。
    if instance.chars().count() > shared_types::USERAPP_BUILDER_INSTANCE_ID_MAX_LEN
        && registered_builder(state, &instance).is_none()
    {
        return Err(anyhow!(
            "builder instance id length {} exceeds {} (K8s StatefulSet label 63-byte limit; \
             see USERAPP_BUILDER_INSTANCE_ID_MAX_LEN)",
            instance.chars().count(),
            shared_types::USERAPP_BUILDER_INSTANCE_ID_MAX_LEN
        ));
    }
    if target.is_owner_instance {
        let identity = state
            .userapp_store
            .ensure_identity(app_id, &target.instance_user)
            .await?;
        if !builder_fenced_by_current_operation(state, &identity).await?
            && let Some(info) = registered_or_discovered_builder(state, &instance).await?
            && let Some(verified) =
                cross_verify_registration(state, app_id, &instance, &info).await?
        {
            return Ok(verified);
        }
        creation::ensure(
            state,
            app_id,
            &instance,
            &target.instance_user,
            _lifecycle,
            deadline,
        )
        .await
    } else {
        if let Some(info) = registered_or_discovered_builder(state, &instance).await?
            && let Some(verified) =
                cross_verify_registration(state, app_id, &instance, &info).await?
        {
            return Ok(verified);
        }
        create_instance_builder(state, app_id, &instance, &target.instance_user, deadline).await
    }
}

/// 当前操作是否围栏 builder：仅 builder 变更族（Ensure/Adopt/Stop/Restart/
/// DestroyDevStorage）跳过注册快路径——生产部署等无关操作进行中，已验证的
/// 注册继续服务（部署期 app-cli 经 rcoder static 转发下载制品依赖此路径，
/// 否则新受理会与在途部署操作自冲突）。
async fn builder_fenced_by_current_operation(
    state: &AppState,
    identity: &shared_types::UserAppLifecycleRecord,
) -> Result<bool> {
    let Some(operation_id) = identity.current_operation_id.as_deref() else {
        return Ok(false);
    };
    let operation = state
        .userapp_store
        .get_operation(&identity.app_id, operation_id)
        .await?
        .ok_or_else(|| anyhow!("Current operation record is missing: {operation_id}"))?;
    Ok(operation.kind.affects_builder())
}

/// 探活自愈版 [`ensure_userapp_builder`]：注册命中后连容器 file-server 探活
/// （3s 超时）。探活失败**不直接判死**——先经 [`remediate_stale_registry`]
/// 以容器真实状态裁决：Running 保容器（负载抖动/启动窗口），真死才清注册重建。
///
/// 供低频管理面调用（pod ensure/keepalive）：**先探活再返回**，防"注册表命中
/// 死容器"幻报就绪；热路径（转发/chat）不适用——它们有自己的节流
/// 探活（forward 30s 正缓存）或按需自愈语义。
///
/// 返回 `(info, created)`——created 由本函数判定（真死重建/miss 创建=true，
/// 探活通过或经裁决保容器=false），调用方无需再读注册表推断。
pub(crate) async fn ensure_userapp_builder_probed(
    state: &AppState,
    app_id: &str,
    explicit_user_id: Option<&str>,
) -> Result<(ContainerBasicInfo, bool)> {
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(state.config.userapp_storage.ensure_timeout_seconds);
    within_builder_deadline(
        deadline,
        ensure_userapp_builder_probed_until(state, app_id, explicit_user_id, deadline),
    )
    .await
}

async fn ensure_userapp_builder_probed_until(
    state: &AppState,
    app_id: &str,
    explicit_user_id: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<(ContainerBasicInfo, bool)> {
    let target = resolve_dev_target(
        explicit_user_id,
        state.app_service.get_app_owner(app_id).await?.as_deref(),
    )
    .with_context(|| {
        format!("cannot resolve dev instance user for app {app_id}; pass user_id explicitly")
    })?;
    let instance = target.instance_id(app_id)?;
    let _lifecycle = lifecycle::acquire(&instance).await;
    if instance.chars().count() > shared_types::USERAPP_BUILDER_INSTANCE_ID_MAX_LEN
        && registered_builder(state, &instance).is_none()
    {
        return Err(anyhow!(
            "builder instance id length {} exceeds {} (K8s StatefulSet label 63-byte limit; \
             see USERAPP_BUILDER_INSTANCE_ID_MAX_LEN)",
            instance.chars().count(),
            shared_types::USERAPP_BUILDER_INSTANCE_ID_MAX_LEN
        ));
    }
    // owner 实例：lifecycle 受理在身（围栏/恢复）；非 owner 实例：轻量路径
    // （复合键锁 fence，无 lifecycle 操作）。探活/自愈两路径同构。
    // `_lifecycle`（复合键进程内互斥）在创建分支移交给 creation worker
    // （防同进程并发创建），探活/裁决分支由本函数持有至返回。
    let fenced = if target.is_owner_instance {
        let identity = state
            .userapp_store
            .ensure_identity(app_id, &target.instance_user)
            .await?;
        builder_fenced_by_current_operation(state, &identity).await?
    } else {
        false
    };

    /// 就绪裁决后按路径创建（owner=lifecycle admit，非 owner=轻量直建）。
    /// `lease`：复合键进程内互斥，创建期间持有（owner 路径移交 worker）。
    async fn rebuild(
        state: &AppState,
        app_id: &str,
        instance: &str,
        target: &DevTarget,
        deadline: tokio::time::Instant,
        lease: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<ContainerBasicInfo> {
        if target.is_owner_instance {
            creation::ensure(
                state,
                app_id,
                instance,
                &target.instance_user,
                lease,
                deadline,
            )
            .await
        } else {
            let _held = lease;
            create_instance_builder(state, app_id, instance, &target.instance_user, deadline).await
        }
    }

    if !fenced && let Some(info) = registered_or_discovered_builder(state, &instance).await? {
        let addr = dev_file_server_addr(state, &info);
        if probe_file_server(&addr).await {
            // 探活过 ≠ 归属正确：跨族污染形态下生产容器的 file-server 同样在
            // 60000 应答（探活恒过、remediation 永不触发）——追加归属交叉校验
            if let Some(updated) =
                cross_verify_registration(state, app_id, &instance, &info).await?
            {
                return Ok((updated, false));
            }
            // Creation validates the runtime independently; do not erase a
            // registration that may have changed while probing.
            return Ok((
                rebuild(state, app_id, &instance, &target, deadline, _lifecycle).await?,
                true,
            ));
        }
        tracing::warn!(
            "[USERAPP_ENSURE] dev container probe failed (stale registry?), verifying container state: app_id={app_id}, addr={addr}"
        );
        // 先验容器真实状态再决定处置：Running 则保容器（探活失败是超时/未就绪
        // 抖动），只有真死才清注册重建——防误杀正在跑任务的容器
        match remediate_stale_registry(state, app_id, &instance).await? {
            RegistryRemediation::Alive(info) => {
                tracing::info!(
                    "[USERAPP_ENSURE] dev container alive on inspect, keep without rebuild: app_id={app_id}"
                );
                return Ok((info, false));
            }
            RegistryRemediation::Gone => {
                // Publish a verified replacement only after durable creation.
                let info = rebuild(state, app_id, &instance, &target, deadline, _lifecycle).await?;
                return Ok((info, true));
            }
        }
    }
    let info = rebuild(state, app_id, &instance, &target, deadline, _lifecycle).await?;
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
pub(crate) fn control_error(error: &anyhow::Error) -> shared_types::AppError {
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
    if let Some(shared_types::UserAppStoreError::OperationInProgress(id)) =
        error.downcast_ref::<shared_types::UserAppStoreError>()
    {
        return shared_types::AppError::with_message(
            shared_types::error_codes::ERR_CONFLICT,
            "A conflicting application operation is in progress",
        )
        .with_operation_id(id.clone());
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
pub(crate) async fn cross_verify_registration(
    state: &AppState,
    app_id: &str,
    instance: &str,
    registered: &ContainerBasicInfo,
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
    // 实例归属校验统一由 verify_live_builder 的运行时 capture 以绑定注解
    // （rcoder.io/owner-id）执行；user_id 身份槽位对 userapp 族恒为空
    // （身份键是复合 identifier），不能与实例归属比较。
    let instance_user = instance_owner(instance)?;
    adoption::verify_live_builder(state, app_id, instance, &instance_user, &rc.container_id)
        .await?;
    let Some(updated) = refreshed_registration(registered, &rc) else {
        return Ok(Some(registered.clone()));
    };
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

/// 复合 identifier 的实例归属段（`{user_id}-{app_id}` 右切 user 段）。
/// 非复合形态（存量/畸形）显式报错——复合键是 builder 定位单一形态。
fn instance_owner(instance: &str) -> Result<String> {
    shared_types::parse_builder_instance_id(instance)
        .map(|(user, _)| user.to_string())
        .ok_or_else(|| anyhow!("builder instance id is not composite: {instance}"))
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
pub(crate) fn dev_file_server_addr(state: &AppState, info: &ContainerBasicInfo) -> String {
    let host = build_backend_addr(
        &info.container_name,
        &info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    );
    format!("http://{host}:{AGENT_FILE_SERVER_PORT}")
}

/// 纯解析:只查 state.projects,无副作用（短路语义 peek 复用——只读判定
/// 容器注册在否，不 ensure 不自愈）。键 = 复合 identifier（`{user_id}-{app_id}`）。
pub(crate) fn registered_builder(state: &AppState, instance: &str) -> Option<ContainerBasicInfo> {
    state
        .projects
        .get(instance)
        .and_then(|p| p.container_info())
}

/// Registry misses perform an authoritative read before creating anything.
async fn registered_or_discovered_builder(
    state: &AppState,
    instance: &str,
) -> Result<Option<ContainerBasicInfo>> {
    if let Some(info) = registered_builder(state, instance) {
        return Ok(Some(info));
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
    let instance_user = instance_owner(instance)?;
    // 实例归属绑定由下方 verify_live_builder 以 rcoder.io/owner-id 注解校验；
    // userapp 族身份槽位不含 user_id，不在此比较（注册登记需实例归属）。
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
    };
    let app_id = shared_types::parse_builder_instance_id(instance)
        .map(|(_, app_id)| app_id.to_string())
        .unwrap_or_else(|| instance.to_string());
    adoption::verify_live_builder(state, &app_id, instance, &instance_user, &info.container_id)
        .await?;
    register_builder(state, instance, &info)?;
    Ok(Some(info))
}

fn register_builder(state: &AppState, instance: &str, info: &ContainerBasicInfo) -> Result<()> {
    let instance_user = instance_owner(instance)?;
    let mut project = state
        .get_project(instance)
        .map(|old| (*old).clone())
        .unwrap_or_else(|| ProjectAndContainerInfo::new(instance.to_owned()));
    project.set_service_type(Some(ServiceType::UserappBuilder));
    project.set_user_id(Some(instance_user));
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
    tokio::time::timeout_at(deadline, async {
        loop {
            if let Some(actual) = cross_verify_registration(state, app_id, instance, &info).await? {
                if actual.container_id != info.container_id {
                    return Err(anyhow!("Builder resource replaced before readiness"));
                }
                if probe_file_server(&dev_file_server_addr(state, &actual)).await {
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
/// `instance` = 复合 identifier（装 project_id 槽位——docker_manager 命名链
/// 单一来源，STS/PVC/svc/label/锁名随之复合化）；`instance_user` = 实例归属
/// （owner 或协作者，注解 owner-id 的值）。
async fn create_builder_inner(
    state: &AppState,
    app_id: &str,
    instance: &str,
    instance_user: &str,
    execution_context: shared_types::UserAppExecutionContext,
) -> Result<ContainerBasicInfo> {
    let bound_target = adoption::capture_bound_target(state, &execution_context).await?;
    let mut params = ContainerCreateParams::builder()
        .execution_context(execution_context)
        .project_id(instance.to_string())
        // 容器内契约（env/profiler）的纯 app_id 显式直传——消费方不再从
        // project_id 复合槽右切（字段语义不重载）
        .builder_app_id(app_id.to_string())
        .user_id(instance_user.to_string())
        .service_type(ServiceType::UserappBuilder)
        .storage_size(DEFAULT_BUILDER_STORAGE_SIZE)
        .build();

    params.resource_binding = bound_target.resource_binding;

    let container_info = state
        .runtime()
        .create_container(params)
        .await
        .context("ensure UserappBuilder failed")?;

    info!(
        "[USERAPP_BUILDER] UserappBuilder ensured: app_id={}, instance={}, container={}, ip={}",
        app_id, instance, container_info.container_name, container_info.container_ip
    );
    Ok(container_info)
}

/// 非 owner 协作者实例创建（轻量路径，**不经 lifecycle admit**）：
/// 并发 fence = 复合键进程内互斥（调用方持有）+ docker_manager 侧 K8s
/// operation ConfigMap 锁（锁名随复合 identifier 自动复合化）。app 须已
/// 存在且 Active（identity 由 owner 链建立；此处只读校验，不动 lifecycle）。
/// 就绪确认与注册与 owner 路径同构（探活 + 跨族污染交叉校验）。
async fn create_instance_builder(
    state: &AppState,
    app_id: &str,
    instance: &str,
    instance_user: &str,
    deadline: tokio::time::Instant,
) -> Result<ContainerBasicInfo> {
    let app = state
        .userapp_store
        .get_application(app_id)
        .await?
        .ok_or_else(|| {
            anyhow!("Application identity missing for collaborator builder: app {app_id}")
        })?;
    if app.state != shared_types::UserAppLifecycleState::Active {
        return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
    }
    // 实例执行上下文：app_id 槽 = 复合 identifier（STS 名派生 + 注解校验）；
    // operation 维度合成实例级标识（无 lifecycle operation 可引用），
    // request_fingerprint 与 owner 路径同源算法（creation_fingerprint 的
    // instance 维度），保证同实例重创建指纹稳定。
    let context = shared_types::UserAppExecutionContext {
        app_id: instance.to_string(),
        user_id: instance_user.to_string(),
        lifecycle_id: app.lifecycle_id,
        operation_id: format!("instance-{}", uuid::Uuid::new_v4().simple()),
        executor_id: uuid::Uuid::new_v4().simple().to_string(),
        request_fingerprint: creation::instance_fingerprint(
            state,
            app_id,
            instance,
            instance_user,
        )?,
    };
    let info = create_builder_inner(state, app_id, instance, instance_user, context).await?;
    let verified = confirm_builder_ready(state, app_id, instance, info, deadline).await?;
    register_builder(state, instance, &verified)?;
    Ok(verified)
}

/// dev 宿主树 owner 解析（纯函数，**owner 锚定语义**——prod 空容器预创建等
/// 归属锚点链专用）：显式传 > metadata 注册值 > 报错；显式与 metadata 同时
/// 存在且不等 = 归属冲突 fail-fast。
///
/// dev builder 复合键时代的实例定位**不走此函数**（用 [`resolve_dev_target`]
/// ——显式 user_id 是实例定位者，不必等于 owner）。
///
/// 空白字符串视为未传（pod 分派层 body 字段可能携空串）。
/// cache/clean 的 userApp 分派共用（owner 三档同源）。
pub(crate) fn resolve_owner(explicit: Option<&str>, metadata: Option<&str>) -> Result<String> {
    let explicit = explicit.map(str::trim).filter(|s| !s.is_empty());
    let metadata = metadata.map(str::trim).filter(|s| !s.is_empty());
    if let (Some(requested), Some(owner)) = (explicit, metadata)
        && requested != owner
    {
        return Err(anyhow!("Application ownership conflict"));
    }
    let owner = metadata
        .or(explicit)
        .ok_or_else(|| anyhow!("missing user_id"))?;
    shared_types::validate_identifier(owner, "user_id").map_err(anyhow::Error::msg)?;
    Ok(owner.to_owned())
}

/// dev builder 实例定位目标（协作模型：同 app 多用户各自独立容器）。
pub(crate) struct DevTarget {
    /// 实例归属 user_id：该用户的独立 builder（容器/PVC/工作区）。
    pub instance_user: String,
    /// 是否 owner 实例（走 lifecycle 受理创建；非 owner 走轻量实例路径）。
    /// app 尚无 identity 时首个显式调用者即成为 owner（creator 语义不变）。
    pub is_owner_instance: bool,
}

impl DevTarget {
    /// 复合 identifier `{instance_user}-{app_id}`（builder 定位单一形态）。
    pub fn instance_id(&self, app_id: &str) -> Result<String> {
        shared_types::builder_instance_id(&self.instance_user, app_id).map_err(anyhow::Error::msg)
    }
}

/// dev 实例定位解析（纯函数）：显式 user_id = 实例定位者（**不必等于 owner**
/// ——共享 space 协作者各自建独立容器）；无显式回落 metadata owner（老调用
/// 兼容）；app 无 identity 时显式者首写为 owner；双缺 fail-fast（绝不兜底
/// app_id 兼任——孤儿目录树防线）。
pub(crate) fn resolve_dev_target(
    explicit: Option<&str>,
    metadata_owner: Option<&str>,
) -> Result<DevTarget> {
    let explicit = explicit.map(str::trim).filter(|s| !s.is_empty());
    let metadata = metadata_owner.map(str::trim).filter(|s| !s.is_empty());
    let (instance_user, is_owner_instance) = match (explicit, metadata) {
        (Some(user), Some(owner)) => (user.to_owned(), user == owner),
        // 首写者成为 owner（Java 侧 creator 先 create-workspace 的既有序）
        (Some(user), None) => (user.to_owned(), true),
        // 老调用兼容：无显式回落 owner 实例
        (None, Some(owner)) => (owner.to_owned(), true),
        (None, None) => return Err(anyhow!("missing user_id")),
    };
    shared_types::validate_identifier(&instance_user, "user_id").map_err(anyhow::Error::msg)?;
    Ok(DevTarget {
        instance_user,
        is_owner_instance,
    })
}

/// 供 bin 装配（main.rs）构造 Pingora 代理的 dev 容器懒启动回调
/// （`UserappDevLocator` 实现 `UserappDevEnsure` 契约；`new` 为 crate 内可见）。
pub fn dev_ensure_for_proxy(state: Weak<AppState>) -> Arc<UserappDevLocator> {
    Arc::new(UserappDevLocator::new(state))
}

#[cfg(test)]
mod tests {
    use super::resolve_owner;

    /// owner 三档：显式优先（含空白显式降级）> metadata > fail-fast。
    /// resolve_owner（owner 锚定语义——prod 空容器预创建等归属锚点链）：
    /// 冲突即拒 / 空白降级 / 双缺 fail-fast。
    #[test]
    fn resolve_owner_prefers_explicit_then_metadata_then_fails() {
        // 显式与 metadata 冲突 → 归属冲突拒绝（prod 归属锚点防线）
        assert!(resolve_owner(Some("u-explicit"), Some("u-meta")).is_err());
        assert_eq!(
            resolve_owner(Some("u-meta"), Some("u-meta")).unwrap(),
            "u-meta"
        );
        // 显式空白 → 降级 metadata
        assert_eq!(resolve_owner(Some("  "), Some("u-meta")).unwrap(), "u-meta");
        // 无显式 → metadata
        assert_eq!(resolve_owner(None, Some("u-meta")).unwrap(), "u-meta");
        // metadata 空白 → 视为未注册
        assert!(resolve_owner(None, Some(" ")).is_err());
        // 双缺 → fail-fast（绝不兜底 app_id 建孤儿目录树）
        assert!(resolve_owner(None, None).is_err());
    }

    /// resolve_dev_target（dev 实例定位语义——协作模型）：显式 = 实例定位者
    /// （不必等于 owner），无显式回落 owner，首写者成为 owner，双缺 fail-fast。
    #[test]
    fn resolve_dev_target_locates_instance_user_without_owner_conflict() {
        use super::resolve_dev_target;
        // 协作者：显式 ≠ metadata owner → 非冲突，独立实例
        let collaborator =
            resolve_dev_target(Some("1769678981"), Some("1754545591")).expect("collaborator");
        assert_eq!(collaborator.instance_user, "1769678981");
        assert!(!collaborator.is_owner_instance);
        assert_eq!(
            collaborator.instance_id("77").unwrap(),
            "1769678981-77",
            "复合 identifier 右切可还原归属"
        );
        // owner 本人：显式 == metadata
        let owner = resolve_dev_target(Some("1754545591"), Some("1754545591")).unwrap();
        assert!(owner.is_owner_instance);
        // 无显式 → 回落 owner 实例（老调用兼容）
        let fallback = resolve_dev_target(None, Some("1754545591")).unwrap();
        assert_eq!(fallback.instance_user, "1754545591");
        assert!(fallback.is_owner_instance);
        // 显式空白 → 同无显式
        let blank = resolve_dev_target(Some("  "), Some("1754545591")).unwrap();
        assert!(blank.is_owner_instance);
        // app 无 identity：显式者首写为 owner（creator 语义）
        let first_writer = resolve_dev_target(Some("u-new"), None).unwrap();
        assert_eq!(first_writer.instance_user, "u-new");
        assert!(first_writer.is_owner_instance);
        // 双缺 → fail-fast
        assert!(resolve_dev_target(None, None).is_err());
        // 复合串结构约束：app_id 含 '-' 拒绝（解析无歧义保证）
        assert!(
            resolve_dev_target(Some("u1"), Some("o1"))
                .unwrap()
                .instance_id("e2e-app")
                .is_err()
        );
    }
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
mod ownership_regressions {
    use super::*;
    #[test]
    fn resolving_owner_never_overwrites_a_different_registered_owner() {
        assert!(resolve_owner(Some("replacement-owner"), Some("original-owner")).is_err());
        assert!(resolve_owner(Some("../foreign"), None).is_err());
        assert_eq!(
            resolve_owner(Some(" original-owner "), Some("original-owner")).expect("same owner"),
            "original-owner"
        );
    }
    #[test]
    fn builder_validation_uses_official_identity_priority_and_service_family() {
        let mut actual = container_runtime_api::RuntimeContainerInfo {
            container_id: "id".into(),
            container_name: "builder".into(),
            container_ip: "127.0.0.1".into(),
            status: container_runtime_api::ContainerRuntimeStatus::Running,
            created_at: chrono::Utc::now(),
            env_vars: None,
            service_type: Some(ServiceType::UserappBuilder),
            project_id: None,
            user_id: Some("owner".into()),
            pod_id: None,
            app_id: Some("app".into()),
        };
        assert!(validate_builder_identity("app", &actual).is_ok());
        actual.pod_id = Some("different-pod".into());
        assert!(validate_builder_identity("app", &actual).is_err());
        actual.pod_id = None;
        actual.service_type = Some(ServiceType::Userapp);
        assert!(validate_builder_identity("app", &actual).is_err());
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
        let error = anyhow::Error::new(shared_types::UserAppStoreError::OperationInProgress(
            "owner-operation".into(),
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
