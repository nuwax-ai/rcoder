//! UserappBuilder 开发容器 ensure 与定位。
//!
//! 跨域公共入口：文件转发层（`userapp_forward`）、chat 开发对话、create-workspace、
//! start/restart 部署链共用——注册表命中复用，miss 创建注册。
//!
//! 构建任务本体在 agent-runner 容器内 file-server（`/api/v1/userapp/build` + tasks 查询），
//! rcoder 不再做发布任务编排（旧 publish 任务体系已随 `/api/v1/userapp/publish` 接口族删除）。

mod dev_cleanup;
mod dev_locator;

pub use dev_cleanup::UserappDevResourcesCleanup;
pub use dev_locator::UserappDevLocator;

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
    /// 容器 Stopped/不存在（或 runtime 查询失败）：走既有清注册重建路径。
    Gone,
}

/// 探活失败自愈裁决（[`RegistryRemediation`] 的判定体）。
///
/// `find_container` 的 Docker 实现为实时 inspect（404 不缓存）、K8s 为 pod
/// get/label list——两后端 Running 语义一致，天然覆盖。
pub(crate) async fn remediate_stale_registry(
    state: &AppState,
    app_id: &str,
) -> RegistryRemediation {
    let Ok(Some(rc)) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await
    else {
        // 查询失败与不存在同判 Gone：重建路径的同名清理已实时化（只删真实
        // 存在的容器），双层防护下 runtime 瞬时故障不会误删活容器
        return RegistryRemediation::Gone;
    };
    if rc.status != container_runtime_api::ContainerRuntimeStatus::Running {
        return RegistryRemediation::Gone;
    }
    // Running：以 inspect 真实值刷新注册。注册缺失走重建（防御——调用方
    // 语义上只在"注册命中但探活失败"时进入本函数）。
    let Some(mut project) = state.get_project(app_id).map(|p| (*p).clone()) else {
        return RegistryRemediation::Gone;
    };
    let Some(existing) = project.container_info() else {
        return RegistryRemediation::Gone;
    };
    if let Some(updated) = refreshed_registration(&existing, &rc) {
        project.set_container(Some(updated.clone()));
        if let Err(e) = state.insert_project(app_id.to_string(), Arc::new(project)) {
            tracing::warn!(
                "[USERAPP_BUILDER] refresh registry from inspect failed: app_id={app_id}: {e:#}"
            );
        } else {
            info!(
                "[USERAPP_BUILDER] registry refreshed from inspect (container alive, probe failure was transient): app_id={app_id}, ip={}",
                updated.container_ip
            );
        }
        RegistryRemediation::Alive(updated)
    } else {
        // 与注册一致（探活失败是纯抖动，注册本来就没脏）：零写直接复用
        RegistryRemediation::Alive(existing.clone())
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
    // 长度 Fail Fast（仅新建路径需要——注册命中说明历史上已建成，不受限）：
    // K8s 下 STS pod 的 controller-revision-hash label =
    // `rcoder-app-builder-{app_id}-{10位hash}` 受 63 字节限，超长必然
    // FailedCreate 且表象含糊（ensure 500/连接超时，真因只在 kubectl
    // events）。入口明确拒绝（229 全链 e2e 实测抓出）。
    if app_id.len() > shared_types::USERAPP_APP_ID_MAX_LEN
        && registered_builder(state, app_id).is_none()
    {
        return Err(anyhow!(
            "app_id length {} exceeds {} (K8s StatefulSet label 63-byte limit; \
             see USERAPP_APP_ID_MAX_LEN)",
            app_id.len(),
            shared_types::USERAPP_APP_ID_MAX_LEN
        ));
    }
    match registered_builder(state, app_id) {
        Some(info) => Ok(info),
        None => create_builder_and_register(state, app_id, explicit_user_id).await,
    }
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
    if let Some(info) = registered_builder(state, app_id) {
        let addr = dev_file_server_addr(state, &info);
        if probe_file_server(&addr).await {
            // 探活过 ≠ 归属正确：跨族污染形态下生产容器的 file-server 同样在
            // 60000 应答（探活恒过、remediation 永不触发）——追加归属交叉校验
            if let Some(updated) = cross_verify_registration(state, app_id, &info).await {
                return Ok((updated, false));
            }
            return Ok((info, false));
        }
        tracing::warn!(
            "[USERAPP_ENSURE] dev container probe failed (stale registry?), verifying container state: app_id={app_id}, addr={addr}"
        );
        // 先验容器真实状态再决定处置：Running 则保容器（探活失败是超时/未就绪
        // 抖动），只有真死才清注册重建——防误杀正在跑任务的容器
        match remediate_stale_registry(state, app_id).await {
            RegistryRemediation::Alive(info) => {
                tracing::info!(
                    "[USERAPP_ENSURE] dev container alive on inspect, keep without rebuild: app_id={app_id}"
                );
                return Ok((info, false));
            }
            RegistryRemediation::Gone => {
                // 就地清 container 字段而非 remove_project（保 PG project 行与会话映射）
                state.clear_project_container_field(app_id);
                let info = create_builder_and_register(state, app_id, explicit_user_id).await?;
                return Ok((info, true));
            }
        }
    }
    let info = create_builder_and_register(state, app_id, explicit_user_id).await?;
    Ok((info, true))
}

/// 探活通过后的**跨族污染交叉校验**（补"探活失败才自愈"的触发缺口）。
///
/// 背景：生产 UserApp 容器与 builder 一样在 60000 跑 file-server——注册表被
/// 跨族污染时（如生产 pod 被写入 builder 注册项），探活恒过、remediation
/// 永不触发，dev 流量持续打向生产容器（vnc/ttyd 6080/7681 拒绝显形为 502，
/// 文件族 60000 则是"错容器成功"更隐蔽）。此处以带类型分流的
/// `find_container` 真实值与注册值比对：不一致即以 inspect 值刷新注册
/// （复用 [`refreshed_registration`]），把自愈触发从"探活失败"扩展到
/// "归属不符"。find 失败/非 Running 时不推翻注册（探活已过的条目维持现状）。
///
/// 成本：一次 pods().get（K8s 单 get，毫秒级）。调用方为低频管理面
/// （ensure_probed）与热路径的 30s 探活缓存 miss 分支，频率受控。
pub(crate) async fn cross_verify_registration(
    state: &AppState,
    app_id: &str,
    registered: &ContainerBasicInfo,
) -> Option<ContainerBasicInfo> {
    let Ok(Some(rc)) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await
    else {
        return None;
    };
    if rc.status != container_runtime_api::ContainerRuntimeStatus::Running {
        return None;
    }
    let updated = refreshed_registration(registered, &rc)?;
    if let Some(mut project) = state.get_project(app_id).map(|p| (*p).clone()) {
        project.set_container(Some(updated.clone()));
        if let Err(e) = state.insert_project(app_id.to_string(), Arc::new(project)) {
            tracing::warn!(
                "[USERAPP_BUILDER] refresh contaminated registry failed: app_id={app_id}: {e}"
            );
            // 写回失败也返回新值——本次请求路由正确比注册表持久一致更紧要，
            // 下一次校验会再试写
            return Some(updated);
        }
    }
    tracing::warn!(
        "[USERAPP_BUILDER] cross-family registry contamination self-healed: app_id={app_id}, registered={} -> actual={}",
        registered.container_name,
        updated.container_name
    );
    Some(updated)
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
/// 容器注册在否，不 ensure 不自愈）。
pub(crate) fn registered_builder(state: &AppState, app_id: &str) -> Option<ContainerBasicInfo> {
    state.projects.get(app_id).and_then(|p| p.container_info())
}

/// 创建 UserappBuilder(幂等)并注册进 state.projects,返回容器信息。
///
/// 直接调 `runtime.create_container`(UserappBuilder → `create_agent_container`),
/// **不走 ComputerContainerManager**(避免 ComputerAgentRunner 专属的 lazy_migrate)。
async fn create_builder_and_register(
    state: &AppState,
    app_id: &str,
    explicit_user_id: Option<&str>,
) -> Result<ContainerBasicInfo> {
    // owner 解析三档：显式传（请求入参）> userapp_metadata.owner（create-workspace/
    // start 注册落库）> fail-fast 报错。绝不兜底 app_id 兼任——旧兜底会把宿主树
    // 挂成 dev/{app_id}/{app_id} 孤儿目录（数据落错树不可回收，且对调用方不可见）。
    let metadata_owner = state.app_service.get_app_owner(app_id).await;
    let owner_user_id =
        resolve_owner(explicit_user_id, metadata_owner.as_deref()).with_context(|| {
            format!("cannot resolve owner user_id for app {app_id}; pass user_id explicitly")
        })?;
    // UserappBuilder identifier = app_id（值经 project_id 槽位进容器基建——
    // state.projects/ContainerCreateParams 共用 project 键空间）；挂载由
    // mounts/k8s_agent_create auto-inject 统一组装（dev 四目录压平）。
    let params = ContainerCreateParams::builder()
        .project_id(app_id.to_string())
        .user_id(owner_user_id)
        .service_type(ServiceType::UserappBuilder)
        .storage_size(DEFAULT_BUILDER_STORAGE_SIZE)
        .build();

    let container_info = state
        .runtime()
        .create_container(params)
        .await
        .context("ensure UserappBuilder failed")?;

    // 注册到 state.projects(后续转发/部署据 app_id 查 container_name/ip)。
    let project_info = if let Some(existing) = state.get_project(app_id) {
        let mut info = (*existing).clone();
        info.set_container(Some(container_info.clone()));
        info
    } else {
        let mut info = ProjectAndContainerInfo::new(app_id.to_string());
        info.set_service_type(Some(ServiceType::UserappBuilder));
        info.set_container(Some(container_info.clone()));
        info
    };
    state
        .insert_project(app_id.to_string(), Arc::new(project_info))
        .context("register UserappBuilder to projects failed")?;

    info!(
        "[USERAPP_BUILDER] UserappBuilder ensured: app_id={}, container={}, ip={}",
        app_id, container_info.container_name, container_info.container_ip
    );
    Ok(container_info)
}

/// dev 宿主树 owner 解析（纯函数）：显式传 > metadata 注册值 > 报错。
///
/// 空白字符串视为未传（pod 分派层 body 字段可能携空串）。
/// cache/clean 的 userApp 分派共用（owner 三档同源）。
pub(crate) fn resolve_owner(explicit: Option<&str>, metadata: Option<&str>) -> Result<String> {
    if let Some(uid) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(uid.to_string());
    }
    if let Some(uid) = metadata.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(uid.to_string());
    }
    Err(anyhow!("missing user_id"))
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
    #[test]
    fn resolve_owner_prefers_explicit_then_metadata_then_fails() {
        // 显式传优先（与 metadata 冲突时显式赢）
        assert_eq!(
            resolve_owner(Some("u-explicit"), Some("u-meta")).unwrap(),
            "u-explicit"
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
            project_id: "app-1".to_string(),
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
            app_id: Some("app-1".to_string()),
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
