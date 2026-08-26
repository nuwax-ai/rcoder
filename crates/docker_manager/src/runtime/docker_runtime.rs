//! Docker runtime implementation
//!
//! This module provides `DockerRuntime` that wraps the existing `DockerManager`
//! and implements the `ContainerRuntime` trait.

use async_trait::async_trait;
use container_runtime_api::{
    AgentContainerRuntime, AppPortSpec, AppPortStatus, ContainerCreateParams,
    ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus, ExposeType,
    RemovedContainerInfo, RuntimeContainerInfo, WorkspaceRuntime,
};
use moka::future::Cache;
use shared_types::{ContainerBasicInfo, ServiceType};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use crate::DockerManager;

/// Docker 内存态回收策略（Docker 无 K8s 注解；字段 None = 未设/沿用默认）。
#[derive(Clone, Copy, Default)]
pub(super) struct RecyclePolicy {
    pub(super) recycle_enabled: Option<bool>,
    pub(super) idle_timeout_seconds: Option<u64>,
}

impl RecyclePolicy {
    /// merge：参数为 None 则保留旧值（self），Some 则覆盖。返回新策略。
    pub(super) fn merge(
        self,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> Self {
        Self {
            recycle_enabled: recycle_enabled.or(self.recycle_enabled),
            idle_timeout_seconds: idle_timeout_seconds.or(self.idle_timeout_seconds),
        }
    }
}

/// Docker runtime implementation wrapping DockerManager
pub struct DockerRuntime {
    pub(super) inner: Arc<DockerManager>,
    /// TTL cache for list_containers result (15 seconds)
    list_cache: Cache<(), Vec<RuntimeContainerInfo>>,
    /// UserApp 闲置回收策略（Docker 无 K8s 注解，改用内存态；dev 模式可接受重启丢失，
    /// 与 pingora_ports 同架构）。app_id → RecyclePolicy，merge 语义。
    pub(super) recycle_policy: DashMap<String, RecyclePolicy>,
}

impl DockerRuntime {
    /// Create a new DockerRuntime wrapping the given DockerManager
    pub fn new(inner: Arc<DockerManager>) -> Self {
        Self {
            inner,
            list_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(15))
                .build(),
            recycle_policy: DashMap::new(),
        }
    }

    /// 读 app 的回收策略（内存态）。未设置 → 默认（recyclable）。
    pub(super) fn recycle_policy_of(&self, app_id: &str) -> RecyclePolicy {
        self.recycle_policy
            .get(app_id)
            .map(|e| *e.value())
            .unwrap_or_default()
    }
}

#[async_trait]
impl AgentContainerRuntime for DockerRuntime {
    async fn create_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // start_agent_container 被标记为 deprecated 是因为返回的 container_id 可能过期，
        // 但 ContainerRuntime trait 的调用方应通过 find_container 获取最新信息，
        // 因此在 runtime 适配层使用是安全的。
        #[allow(deprecated)]
        self.inner
            .start_agent_container(params)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerCreationError(e.to_string()))
    }

    async fn get_container_info(
        &self,
        project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.inner
            .get_agent_info(project_id)
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        match service_type {
            ServiceType::WebAgentRunner => self
                .inner
                .get_agent_info(identifier)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string())),
            // 使用 find_container 实时查询 Docker API 获取 IP，
            // 避免 get_user_container_info → get_agent_info → get_container_info 只查缓存
            // 导致服务重启后缓存丢失返回 None。
            // UserAppBuilder 复用 agent-runner 镜像(有 gRPC),同样走实时查询。
            ServiceType::ComputerAgentRunner | ServiceType::UserAppBuilder => {
                let result = self.find_container(identifier, service_type).await?;
                Ok(result.map(|pod| ContainerBasicInfo {
                    container_id: pod.container_id,
                    container_name: pod.container_name,
                    container_ip: pod.container_ip.clone(),
                    internal_port: shared_types::GRPC_DEFAULT_PORT,
                    external_port: 0,
                    project_id: identifier.to_string(),
                    status: String::from(pod.status),
                    created_at: pod.created_at,
                    service_url: format!(
                        "http://{}:{}",
                        pod.container_ip,
                        shared_types::GRPC_DEFAULT_PORT
                    ),
                }))
            }
            // UserApp 兜底：UserApp 通常走 create_deployment/get_deployment_status，
            // 此处仅为 trait 穷尽性，端口不固定故 internal_port=0
            ServiceType::UserApp => {
                let result = self.find_container(identifier, service_type).await?;
                Ok(result.map(|pod| ContainerBasicInfo {
                    container_id: pod.container_id,
                    container_name: pod.container_name,
                    container_ip: pod.container_ip.clone(),
                    internal_port: 0,
                    external_port: 0,
                    project_id: identifier.to_string(),
                    status: String::from(pod.status),
                    created_at: pod.created_at,
                    // 唯一消费方（DockerRuntimeIpResolver）只读 container_ip；
                    // service_url 是 v1 agent 容器遗留字段，此处填空避免每请求死分配
                    service_url: String::new(),
                }))
            }
        }
    }

    async fn find_container(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        let result = self
            .inner
            .find_project_container(project_id, service_type)
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?;

        Ok(result.map(|r| RuntimeContainerInfo {
            container_id: r.container_id,
            container_name: r.container_name,
            container_ip: r.container_ip,
            status: map_container_status(&r.status),
            created_at: r.created_at,
            env_vars: None, // 不填充环境变量（用于快速查找）
        }))
    }

    /// Docker 基础诊断：find_container → docker inspect → 读 State(OOMKilled/exit_code/running)。
    /// 用于 gRPC 连接失败时的根因识别（OOM / 容器不在），生成精准友好错误，而非裸 transport error。
    /// inspect 失败(容器重建中等)→ 返回默认诊断(无根因)，调用方按"保留原文"处理，不误判。
    async fn diagnose_agent_pod(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentPodDiagnostic> {
        use bollard::query_parameters::InspectContainerOptions;

        // 1. 按 identifier 找容器；找不到本身就是根因（exists=false）。
        let Some(info) = self.find_container(identifier, service_type).await? else {
            return Ok(container_runtime_api::AgentPodDiagnostic {
                exists: false,
                ..Default::default()
            });
        };

        // 2. docker inspect 读 State。
        let inspect = match self
            .inner
            .get_docker_client()
            .inspect_container(&info.container_id, None::<InspectContainerOptions>)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "[DIAGNOSE] docker inspect failed for {}: {}",
                    info.container_id,
                    e
                );
                // inspect 失败(容器可能在重建/重命名)→ 不确定，返回默认诊断，不误判。
                return Ok(container_runtime_api::AgentPodDiagnostic::default());
            }
        };

        // 3. State → AgentPodDiagnostic。OOMKilled 是 Docker 侧最关键的根因信号。
        // state 借用(不 move inspect.state),以便同时读 inspect.restart_count(在顶层)。
        let state = inspect.state.as_ref();
        let oom_killed = state.and_then(|s| s.oom_killed).unwrap_or(false);
        let exit_code = state.and_then(|s| s.exit_code);
        let running = state.and_then(|s| s.running).unwrap_or(false);
        let restart_count = inspect.restart_count.unwrap_or(0) as u32;
        let status_str = state.and_then(|s| s.status.as_ref().map(|st| format!("{st:?}")));

        Ok(container_runtime_api::AgentPodDiagnostic {
            exists: true,
            ready: running,
            restart_count,
            last_terminate_reason: oom_killed.then(|| "OOMKilled".to_string()),
            last_exit_code: exit_code.map(|c| c as i32),
            waiting_reason: None, // Docker 无 CrashLoopBackOff 概念
            detail: status_str.filter(|s| !s.is_empty()),
        })
    }

    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
        self.inner
            .stop_container(project_id)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))
    }

    async fn stop_container_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        match service_type {
            // UserApp/UserAppBuilder 的 identifier=app_id/project_id，复用 WebAgentRunner 的 stop_container 路径
            ServiceType::WebAgentRunner | ServiceType::UserApp | ServiceType::UserAppBuilder => {
                self.inner
                    .stop_container(identifier)
                    .await
                    .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))
            }
            ServiceType::ComputerAgentRunner => {
                if let Some(container) = self
                    .inner
                    .find_user_container(identifier, service_type)
                    .await
                    .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?
                {
                    self.inner
                        .stop_container_by_id(&container.container_id)
                        .await
                        .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?;
                }
                Ok(())
            }
        }
    }

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        if let Some(info) = self.get_container_info(project_id).await? {
            // 统一走 ContainerStatus 枚举比较（大小写不敏感），不直接比字符串
            Ok(crate::types::ContainerStatus::from(info.status).is_running())
        } else {
            Ok(false)
        }
    }

    async fn is_container_running_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(identifier, service_type)
            .await?
            .map(|c| c.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        // 尝试从缓存获取
        if let Some(cached) = self.list_cache.get(&()).await {
            return Ok(cached);
        }

        // 缓存未命中或过期，fetch 并写入缓存
        let result = self.fetch_containers().await?;
        self.list_cache.insert((), result.clone()).await;
        Ok(result)
    }

    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        self.inner
            .sync_all_container_states()
            .await
            .map_err(|e| ContainerRuntimeError::DockerError(e.to_string()))
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.inner
            .cleanup_all_containers()
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        self.inner.get_docker_client().ping().await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("Docker ping failed: {}", e))
        })?;
        Ok(())
    }
}

/// Docker 不实现 WorkspaceRuntime 的 `resolve_*` / `list_workspace_identifiers` / `ensure_workspace`
/// (file-server 经 trait upcast 拿到 DockerRuntime 时这些方法命中 trait 默认 Ok(None)/Ok(vec![])/Ok(()),
/// 走 LocalWorkspaceResolver 降级 —— 符合 Docker 模式设计)。
/// **仅 `destroy_app_pvc` 重写** (Docker 模式 destroy = 删 app workspace 目录, 对应 K8s 删 PVC+subvolume).
#[async_trait]
impl WorkspaceRuntime for DockerRuntime {
    async fn workspace_volume_name(
        &self,
        app_id: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        // Docker 持久卷 = userapp-workspace prod 树四目录（uid 维度经通配定位，
        // 无单一物理路径）——返回展示标识串（与 K8s 返回 PVC 名对称：标识而非
        // 可 stat 路径；存在性判定由调用方 storage 层用元数据 uid 精确定位）。
        if app_id.is_empty() || app_id.contains('/') || app_id.contains('\\') {
            return Err(ContainerRuntimeError::DockerError(format!(
                "workspace_volume_name: invalid app_id {app_id:?}"
            )));
        }
        Ok(format!(
            "{}/prod/*/{}",
            shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
            app_id
        ))
    }

    async fn destroy_app_pvc(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        // Docker 无 PVC 概念；destroy = 删 userapp-workspace prod 树该 app 的四目录
        // （workspace + data/logs/agent-store，对应 K8s 删单卷 PVC）+ 兜底删旧
        // RCODER_WORKSPACE_ROOT/{app_id} 制品目录（四目录化前的旧布局孤儿）。
        // uid 不在本层（无元数据视图）→ 通配扫 `prod/*/` 一层按
        // userapp_prod_subpaths(uid, app_id) 精确匹配四段——与 dev cleanup
        // （dev_cleanup.rs 通配 dev/*/）完全同款模式，顺带覆盖 uid 兜底不一致的目录。
        // 幂等：目录不存在返回 Ok（对应 K8s PVC 404→Ok）。app_id 经 service 层
        // validate_app_id 校验（DNS-1123，无 .. / 路径穿越），join 安全。
        if app_id.is_empty() || app_id.contains('/') || app_id.contains('\\') {
            return Err(ContainerRuntimeError::DockerError(format!(
                "destroy_app_pvc: invalid app_id {app_id:?}"
            )));
        }
        let prod_root =
            std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT).join("prod");
        if prod_root.is_dir() {
            let uid_entries = std::fs::read_dir(&prod_root)
                .map_err(|e| {
                    ContainerRuntimeError::DockerError(format!(
                        "destroy_app_pvc: read_dir {}: {e}",
                        prod_root.display()
                    ))
                })?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>();
            for uid in uid_entries {
                for sub in shared_types::paths::userapp_prod_subpaths(&uid, app_id) {
                    let dir =
                        std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
                            .join(&sub);
                    if dir.exists() {
                        tokio::fs::remove_dir_all(&dir).await.map_err(|e| {
                            ContainerRuntimeError::DockerError(format!(
                                "destroy_app_pvc: remove {}: {}",
                                dir.display(),
                                e
                            ))
                        })?;
                        tracing::info!("[Docker] app prod dir destroyed: {}", dir.display());
                    }
                }
            }
        }
        // 旧布局兜底：RCODER_WORKSPACE_ROOT/{app_id}（默认 /app/project_workspace/apps，
        // 与 AppManagerConfig::get_workspace_root 同源）——存量升级后制品目录孤儿。
        let ws_root = std::env::var("RCODER_WORKSPACE_ROOT")
            .unwrap_or_else(|_| "/app/project_workspace/apps".to_string());
        let legacy_dir = std::path::Path::new(&ws_root).join(app_id);
        if legacy_dir.exists() {
            tokio::fs::remove_dir_all(&legacy_dir).await.map_err(|e| {
                ContainerRuntimeError::DockerError(format!(
                    "destroy_app_pvc: remove legacy {}: {}",
                    legacy_dir.display(),
                    e
                ))
            })?;
            tracing::info!(
                "[Docker] legacy app workspace destroyed: {}",
                legacy_dir.display()
            );
        }
        Ok(())
    }
}

// UserAppDeploymentRuntime 完整实现拆至 docker_app_runtime.rs（与 K8s 侧
// k8s_app_*.rs 文件群对称——Docker 语义映射的 app 域自成一档）。
impl DockerRuntime {
    /// Fetch containers from Docker API (used as cache loader)
    async fn fetch_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let containers = self.inner.list_containers().await;
        let mut result = Vec::with_capacity(containers.len());
        for c in containers {
            let container_ip = self
                .inner
                .get_container_connection_info(&c)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?
                .unwrap_or_default();

            // 构建环境变量映射（包含 project_id 和 service_type）
            let mut env_vars = HashMap::new();
            env_vars.insert("PROJECT_ID".to_string(), c.project_id.clone());
            if let Some(ref user_id) = c.user_id {
                env_vars.insert("USER_ID".to_string(), user_id.clone());
            }
            if let Some(ref service_type) = c.service_type {
                env_vars.insert("SERVICE_TYPE".to_string(), service_type.to_string());
            }

            result.push(RuntimeContainerInfo {
                container_id: c.container_id,
                container_name: c.container_name,
                container_ip,
                status: map_container_status(&c.status),
                created_at: c.created_at,
                env_vars: Some(env_vars),
            });
        }
        Ok(result)
    }
}

/// UserApp 容器/Deployment 命名（单一来源，与 K8s 侧 `KubernetesRuntime::app_deployment_name` 对称）。
///
/// 前缀取自 `ServiceType::UserApp::container_prefix()`，避免散落硬编码；改前缀只需改一处。
pub(super) fn app_deployment_name(app_id: &str) -> String {
    format!("{}-{app_id}", ServiceType::UserApp.container_prefix())
}

/// 容器 ports 元数据 label（update live 回退数据源；编码 "8080:http,5432:tcp"，
/// 与 K8s `rcoder.io/port-expose` 注解同构——Docker 侧 Http/Tcp 均无完整运行时
/// 落地可反推，见 create_deployment 内注释）。
pub(super) const APP_PORTS_LABEL: &str = "rcoder.io/app-ports";
/// 容器 command 元数据 label（JSON 数组；create 时用户显式设置才写入）。
pub(super) const APP_COMMAND_LABEL: &str = "rcoder.io/app-command";

/// ports → label 值（按端口排序编码，顺序无关 → 字符串稳定，避免无谓容器 diff）。
pub(super) fn encode_ports_label(ports: &[AppPortSpec]) -> String {
    let mut entries: Vec<(u16, &ExposeType)> =
        ports.iter().map(|p| (p.port, &p.expose_type)).collect();
    entries.sort_by_key(|(port, _)| *port);
    entries
        .iter()
        .map(|(port, et)| format!("{port}:{}", expose_type_str(et)))
        .collect::<Vec<_>>()
        .join(",")
}

/// label 值 → ports（容错：非法条目跳过；name 空串/strip_prefix None——Docker 单机
/// 模式这两项无运行时语义）。
pub(super) fn parse_ports_label(raw: &str) -> Vec<AppPortSpec> {
    raw.split(',')
        .filter_map(|entry| {
            let mut it = entry.split(':');
            let port: u16 = it.next()?.trim().parse().ok()?;
            let et = match it.next()?.trim() {
                "tcp" => ExposeType::Tcp,
                _ => ExposeType::Http,
            };
            Some(AppPortSpec {
                name: String::new(),
                port,
                expose_type: et,
                strip_prefix: None,
            })
        })
        .collect()
}

pub(super) fn expose_type_str(e: &ExposeType) -> &'static str {
    match e {
        ExposeType::Http => "http",
        ExposeType::Tcp => "tcp",
    }
}

/// Docker `NanoCpus`（1 核 = 1e9）→ K8s Quantity 核数字符串（"1"/"0.5"）。
/// update 回退用：读回的值将再次下发为 K8s/Docker 资源限制。
pub(super) fn docker_cpus_to_quantity(nano_cpus: i64) -> String {
    let cores = nano_cpus as f64 / 1e9;
    if cores.fract() == 0.0 {
        format!("{}", cores as i64)
    } else {
        format!("{cores}")
    }
}

/// Docker 字节内存限制 → K8s Quantity 字符串（无损换算：优先整 Gi/Mi/Ki 档，非整档
/// 用更细档位精确表示——1.5Gi=1536Mi 而非缩水成 1Gi；非 Ki 整数倍的罕见值直接输出
/// 字节数，K8s Quantity 合法且无损）。
pub(super) fn docker_memory_to_quantity(bytes: i64) -> String {
    const KI: i64 = 1024;
    const MI: i64 = 1024 * 1024;
    const GI: i64 = 1024 * 1024 * 1024;
    if bytes >= GI && bytes % GI == 0 {
        format!("{}Gi", bytes / GI)
    } else if bytes >= MI && bytes % MI == 0 {
        format!("{}Mi", bytes / MI)
    } else if bytes >= KI && bytes % KI == 0 {
        format!("{}Ki", bytes / KI)
    } else {
        format!("{bytes}")
    }
}

/// 将内部 `ContainerStatus` 映射为运行时 `ContainerRuntimeStatus`
fn map_container_status(status: &crate::types::ContainerStatus) -> ContainerRuntimeStatus {
    match status {
        crate::types::ContainerStatus::Running => ContainerRuntimeStatus::Running,
        crate::types::ContainerStatus::Stopped => ContainerRuntimeStatus::Failed,
        crate::types::ContainerStatus::Creating => ContainerRuntimeStatus::Pending,
        crate::types::ContainerStatus::Restarting => ContainerRuntimeStatus::Pending,
        crate::types::ContainerStatus::Paused => {
            ContainerRuntimeStatus::Unknown("paused".to_string())
        }
        crate::types::ContainerStatus::Dead => ContainerRuntimeStatus::Failed,
        crate::types::ContainerStatus::Removing => ContainerRuntimeStatus::Failed,
        crate::types::ContainerStatus::Exited => ContainerRuntimeStatus::Failed,
        crate::types::ContainerStatus::Unknown(s) => ContainerRuntimeStatus::Unknown(s.clone()),
    }
}

/// 从容器 inspect 结果提取 IP：优先取 `preferred_network` 网卡，回退任意网卡。
///
/// Docker 容器可能同时连接多个网络（主网络 + 自定义），`networks.values().next()`
/// 会非确定性地取一个。优先按主网络名定位，确保拿到 Pingora backend 应指向的 IP。
pub(super) fn extract_container_ip(
    inspect: &bollard::models::ContainerInspectResponse,
    preferred_network: Option<&str>,
) -> String {
    let Some(nets) = inspect
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
    else {
        return String::new();
    };
    if let Some(net) = preferred_network
        && let Some(entry) = nets.get(net)
        && let Some(ip) = entry.ip_address.as_ref()
        && !ip.is_empty()
    {
        return ip.clone();
    }
    nets.values()
        .next()
        .and_then(|e| e.ip_address.clone())
        .filter(|ip| !ip.is_empty())
        .unwrap_or_default()
}

/// 从容器 inspect 提取 TCP 端口状态（Docker port_bindings → host_port）。
///
/// Docker 仅对 TCP 端口做 port_bindings（create_deployment 时），HTTP 端口走 Pingora
/// 不做 binding，故此处只还原 TCP；name 用 `tcp-{port}`（Docker 无端口名概念，调用方
/// 按 port 而非 name 匹配 external_port）。
pub(super) fn extract_container_ports(
    inspect: &bollard::models::ContainerInspectResponse,
) -> Vec<AppPortStatus> {
    let Some(ports_map) = inspect
        .network_settings
        .as_ref()
        .and_then(|n| n.ports.as_ref())
    else {
        return vec![];
    };
    ports_map
        .iter()
        .filter_map(|(key, bindings)| {
            // key 形如 "80/tcp"
            let port: u16 = key.trim_end_matches("/tcp").parse().ok()?;
            let host_port = bindings
                .as_ref()
                .and_then(|b| b.first())
                .and_then(|pb| pb.host_port.as_deref())
                .and_then(|s| s.parse::<u16>().ok())?;
            Some(AppPortStatus {
                name: format!("tcp-{port}"),
                port,
                expose_type: ExposeType::Tcp,
                external_port: Some(host_port),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ports label 编解码往返：Http/Tcp 类型精确保留、编码按端口排序稳定。
    #[test]
    fn ports_label_roundtrip_preserves_expose_types() {
        let ports = vec![
            AppPortSpec {
                name: "http".into(),
                port: 8080,
                expose_type: ExposeType::Http,
                strip_prefix: Some(true),
            },
            AppPortSpec {
                name: "db".into(),
                port: 5432,
                expose_type: ExposeType::Tcp,
                strip_prefix: None,
            },
        ];
        let encoded = encode_ports_label(&ports);
        assert_eq!(encoded, "5432:tcp,8080:http");
        let parsed = parse_ports_label(&encoded);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].port, 5432);
        assert!(matches!(parsed[0].expose_type, ExposeType::Tcp));
        assert_eq!(parsed[1].port, 8080);
        assert!(matches!(parsed[1].expose_type, ExposeType::Http));
    }

    /// 解码容错：非法条目（非数字端口/缺类型）跳过，其余保留。
    #[test]
    fn ports_label_parse_skips_invalid_entries() {
        let parsed = parse_ports_label("8080:http,abc:tcp,:http,9090:tcp");
        assert_eq!(parsed.len(), 2);
        assert!(parsed.iter().all(|p| p.port == 8080 || p.port == 9090));
    }
}
