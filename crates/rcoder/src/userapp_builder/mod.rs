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
use tracing::{info, warn};

use crate::router::AppState;

/// UserappBuilder per-app PVC 默认大小(后续可提到 config.yml 的 user-app-builder.service 段)。
const DEFAULT_BUILDER_STORAGE_SIZE: &str = "100Gi";

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
/// （3s 超时），失败视为注册脏值（容器被外部删除）→ 清注册重建。
///
/// 供低频管理面调用（pod ensure/keepalive）：**先探活再返回**，防"注册表命中
/// 死容器"幻报就绪；热路径（转发/chat）不适用——它们有自己的节流
/// 探活（forward 30s 正缓存）或按需自愈语义。
///
/// 返回 `(info, created)`——created 由本函数判定（探活失败重建/miss 创建=true，
/// 复用=false），调用方无需再读注册表推断。
pub(crate) async fn ensure_userapp_builder_probed(
    state: &AppState,
    app_id: &str,
    explicit_user_id: Option<&str>,
) -> Result<(ContainerBasicInfo, bool)> {
    if let Some(info) = registered_builder(state, app_id) {
        let addr = dev_file_server_addr(state, &info);
        if probe_file_server(&addr).await {
            return Ok((info, false));
        }
        tracing::warn!(
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
        let info = create_builder_and_register(state, app_id, explicit_user_id).await?;
        return Ok((info, true));
    }
    let info = create_builder_and_register(state, app_id, explicit_user_id).await?;
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
pub(crate) fn registered_builder(state: &AppState, project_id: &str) -> Option<ContainerBasicInfo> {
    state
        .projects
        .get(project_id)
        .and_then(|p| p.container_info())
}

/// 创建 UserappBuilder(幂等)并注册进 state.projects,返回容器信息。
///
/// 直接调 `runtime.create_container`(UserappBuilder → `create_agent_container`),
/// **不走 ComputerContainerManager**(避免 ComputerAgentRunner 专属的 lazy_migrate)。
async fn create_builder_and_register(
    state: &AppState,
    project_id: &str,
    explicit_user_id: Option<&str>,
) -> Result<ContainerBasicInfo> {
    // owner 解析三档：显式传（请求入参）> userapp_metadata.owner（create-workspace/
    // start 注册落库）> fail-fast 报错。绝不兜底 app_id 兼任——旧兜底会把宿主树
    // 挂成 dev/{app_id}/{app_id} 孤儿目录（数据落错树不可回收，且对调用方不可见）。
    let metadata_owner = state.app_service.get_app_owner(project_id).await;
    let owner_user_id =
        resolve_owner(explicit_user_id, metadata_owner.as_deref()).with_context(|| {
            format!("cannot resolve owner user_id for app {project_id}; pass user_id explicitly")
        })?;
    // UserappBuilder identifier = project_id(app_id 兼任);挂载由 mounts/k8s_agent_create
    // auto-inject 统一组装（dev 四目录压平）。
    let params = ContainerCreateParams::builder()
        .project_id(project_id.to_string())
        .user_id(owner_user_id)
        .service_type(ServiceType::UserappBuilder)
        .storage_size(DEFAULT_BUILDER_STORAGE_SIZE)
        .build();

    let container_info = state
        .runtime()
        .create_container(params)
        .await
        .context("ensure UserappBuilder failed")?;

    // 注册到 state.projects(后续转发/部署据 project_id 查 container_name/ip)。
    let project_info = if let Some(existing) = state.get_project(project_id) {
        let mut info = (*existing).clone();
        info.set_container(Some(container_info.clone()));
        info
    } else {
        let mut info = ProjectAndContainerInfo::new(project_id.to_string());
        info.set_service_type(Some(ServiceType::UserappBuilder));
        info.set_container(Some(container_info.clone()));
        info
    };
    state
        .insert_project(project_id.to_string(), Arc::new(project_info))
        .context("register UserappBuilder to projects failed")?;

    info!(
        "[USERAPP_BUILDER] UserappBuilder ensured: app_id={}, container={}, ip={}",
        project_id, container_info.container_name, container_info.container_ip
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
