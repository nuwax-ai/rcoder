//! ContainerRuntime trait 定义
//!
//! 阶段3 ISP 拆分: 原 `ContainerRuntime` 32 方法拆为 3 个聚焦子 trait + 1 个聚合 super-trait:
//! - [`AgentContainerRuntime`] (Group A, 13): agent 容器生命周期 (create/stop/find/list/health).
//! - [`WorkspaceRuntime`]    (Group B, 5):  workspace/file-server PVC 解析 (resolve/ensure/destroy).
//! - [`UserAppDeploymentRuntime`] (Group C, 14): UserApp Deployment CRUD (create/patch/scale/logs/exec).
//! - [`ContainerRuntime`]:    聚合 super-trait = A + B + C; 旧调用点零改 (`Arc<dyn ContainerRuntime>`).
//! - [`UserAppRuntime`]:      B + C 视图; 供 app_manager 收紧 (不需 agent 能力).
//!
//! 默认实现逐字照搬原 trait; K8s/Docker 各自的 override 也保留原 method body.

use async_trait::async_trait;
use shared_types::{ContainerBasicInfo, ServiceType};

use super::container_params::ContainerCreateParams;
use super::types::{
    AppEventInfo, ContainerLogEntry, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, ContainerSpecSnapshot, DeploymentStatus, ExecResult,
    RemovedContainerInfo, ResourceUsage, RuntimeContainerInfo,
};

// mpsc 仍在 lib.rs re-export（`container_runtime_api::mpsc::Receiver` 被 docker_manager /
// app_manager 多处使用，trait 方法 stream_app_logs 用到）
pub use tokio::sync::mpsc;

// ============================================================================
// Group A — AgentContainerRuntime (13 方法)
// agent 容器生命周期: create / find / stop / list / health / sync / inplace-restart.
// 三个 *_by_identifier 方法保留行为默认实现 (委派 find / get_container_info / stop_container).
// ============================================================================

/// Agent 容器生命周期运行时 (Group A)。
///
/// 用于 rcoder 主服务对 agent pod/container (WebAgentRunner / ComputerAgentRunner) 的管理。
/// 不含 UserApp Deployment 或 workspace PVC 解析能力。
#[async_trait]
pub trait AgentContainerRuntime: Send + Sync {
    /// Create and start a container
    async fn create_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo>;

    /// Get container information by project_id
    async fn get_container_info(
        &self,
        project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>>;

    /// Get container information by identifier + service type.
    ///
    /// `identifier` means:
    /// - RCoder: `project_id`
    /// - ComputerAgentRunner: `user_id`
    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        if matches!(service_type, ServiceType::WebAgentRunner) {
            return self.get_container_info(identifier).await;
        }

        let info = self.find_container(identifier, service_type).await?;
        Ok(info.map(|pod| ContainerBasicInfo {
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

    /// Find container by project_id (returns None if not running)
    async fn find_container(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>>;

    /// Stop and remove container
    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()>;

    /// Stop and remove container by identifier + service type.
    ///
    /// `identifier` means:
    /// - RCoder: `project_id`
    /// - ComputerAgentRunner: `user_id`
    async fn stop_container_by_identifier(
        &self,
        identifier: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        self.stop_container(identifier).await
    }

    /// Get container status
    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool>;

    /// Get container status by identifier + service type.
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

    /// List all containers managed by this runtime
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>>;

    /// 同步缓存状态，清理失效的容器记录
    ///
    /// 对于 Docker：遍历 ContainerStateActor 缓存，通过 Docker API 验证容器是否仍存在
    /// 对于 K8s：遍历 pod_cache，通过 K8s API 验证 Pod 是否仍存在
    ///
    /// # Returns
    /// 返回元组 (已检查数量, 已移除容器信息列表)
    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        // 默认实现：不做任何事（向后兼容）
        Ok((0, Vec::new()))
    }

    /// Cleanup all containers (used on shutdown)
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()>;

    /// Health check - verify runtime is accessible
    async fn health_check(&self) -> ContainerRuntimeResult<()>;

    /// 原地重启 agent 容器（exec SIGTERM PID 1 → kubelet restartPolicy 重启容器，卷不 unstage → 快）。
    ///
    /// 用于 agent-runner pod 重启，避免 delete+recreate 触发 CephFS `NodeStageVolume` re-stage（~60s）。
    /// K8s：exec 进 agent 容器 `kill -TERM 1` → agent_runner SIGTERM handler 优雅退出 → kubelet
    /// `restartPolicy=Always` 原地重启容器（PID 1 = agent_runner，已实测确认）。
    /// 默认 NotImplemented；调用方（pod_restart）失败应回落 destroy+recreate。
    async fn restart_container_inplace(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let _ = (identifier, service_type);
        Err(ContainerRuntimeError::ConfigurationError(
            "restart_container_inplace not supported by this runtime".to_string(),
        ))
    }
}

// ============================================================================
// Group B — WorkspaceRuntime (5 方法)
// workspace / file-server PVC 解析与生命周期:
//   resolve_workspace_path / resolve_workspace_path_by_pvcname / list_workspace_identifiers /
//   ensure_workspace / destroy_app_pvc.
// 默认实现: resolve_*=Ok(None), list=Ok(vec![]), ensure/destroy=Ok(()) (Docker / 未实现).
// ============================================================================

/// Workspace (per-agent / per-app PVC) 解析与生命周期运行时 (Group B)。
///
/// 用于 file-server 经 rcoder 挂根聚合访问 agent 数据 (不启动 agent pod 也能服务),
/// 以及 app_manager 维护 per-app PVC (含孤儿检测 / destroy).
#[async_trait]
pub trait WorkspaceRuntime: Send + Sync {
    /// 解析 agent workspace 在 rcoder 主进程可访问的路径 (阶段2 挂根聚合)。
    ///
    /// K8s 模式: 返回 per-agent PVC 的 CephFS subvolume 聚合路径
    ///   `{RCODER_CEPHFS_ROOT}/{subvolumePath}` (rcoder 静态 PV 挂根, 访问 agent 数据;
    ///   file-server 经此读 tree/git/skills, 不启动 agent pod 也能服务)。
    /// Docker 模式: 不提供聚合视角, 用默认 None (file-server 走 LocalWorkspaceResolver)。
    async fn resolve_workspace_path(
        &self,
        _identifier: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<String>> {
        Ok(None)
    }

    /// 解析任意 PVC 名(per-agent 或共享)的 CephFS subvolume 聚合路径 (阶段3 lazy mv 用)。
    ///
    /// 与 `resolve_workspace_path` 同, 但直接接受 PVC 名 (共享 PVC 如 rcoder-workspace,
    /// 非按 identifier 生成)。供 rcoder 经挂根做 lazy mv 时定位共享 PVC 的数据根。
    /// Docker 模式默认 None。
    async fn resolve_workspace_path_by_pvcname(
        &self,
        _pvc_name: &str,
    ) -> ContainerRuntimeResult<Option<String>> {
        Ok(None)
    }

    /// 列出某 service_type 下所有 workspace（per-app PVC / 工作空间目录）对应的 identifier。
    ///
    /// 供 storage/query 枚举"有持久数据"的 app——含**已 delete 但 PVC/目录保留的孤儿**。
    /// 这是 orphan 检测的数据源：`list_deployments` 只能拿到运行中的 app，看不到已删的残留，
    /// 故 storage/query 需用它才能发现孤儿存储（v2 §5.4 / §9.2 的数据侧对账）。
    /// K8s 实现：枚举带 `service_type=<st>` label 的 PVC，从 PVC 名反解 identifier；
    /// Docker 默认空（dev 模式，孤儿检测非关键）。
    async fn list_workspace_identifiers(
        &self,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Vec<String>> {
        Ok(vec![])
    }

    /// 确保 per-agent workspace PVC 存在 (幂等: 已存在则复用, 不存在则创建)。
    ///
    /// K8s: 调 `ensure_workspace_pvc`; Docker: no-op。
    /// 供 file-server (经 `WorkspacePathResolver::ensure_and_resolve`) 在 resolve 时
    /// 主动确保 PVC 存在 (file-server 先于 rcoder create_container 被调)。
    async fn ensure_workspace(
        &self,
        _identifier: &str,
        _service_type: &ServiceType,
        _storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        Ok(()) // default no-op (Docker / 未实现)
    }

    /// 销毁 per-app PVC(UserApp 专用;Docker 默认 no-op)。
    ///
    /// K8s: 删 PVC 对象 → ceph-csi 回收 subvolume(释放配额)。调用方须保证 app 已 delete
    /// (PVC 无 Pod 引用 → pvc-protection finalizer 正常移除,不会卡)。幂等:PVC 不存在返 Ok。
    /// 见 `docs/application-management-service-v2-design.md` §5.4 destroy。
    async fn destroy_app_pvc(&self, _app_id: &str) -> ContainerRuntimeResult<()> {
        Ok(()) // default no-op (Docker / 未实现)
    }
}

// ============================================================================
// Group C — UserAppDeploymentRuntime (14 方法)
// UserApp Deployment CRUD: create/patch/scale/restart/delete + status/logs/events/exec.
// 默认实现: 核心操作返 ConfigurationError; 容器快照/events/resource/prerequisites Ok(default).
// ============================================================================

/// UserApp Deployment 运行时 (Group C)。
///
/// K8s 由 `KubernetesRuntime` 实现真实 Deployment 操作；
/// Docker 由 `DockerRuntime` 做等价语义映射（容器 create/stop/start）。
#[async_trait]
pub trait UserAppDeploymentRuntime: Send + Sync {
    /// 创建并启动一个 Deployment（K8s）或等价容器（Docker）
    async fn create_deployment(
        &self,
        _params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(ContainerRuntimeError::ConfigurationError(
            "create_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 更新一个已存在的 Deployment/容器（全量替换 desired state）。
    ///
    /// K8s：SSA re-apply 全部资源（幂等）+ 清理不再需要的端口/配置资源（orphan）。
    /// Docker：image/command/env 变化需重建容器（force-remove + create）。
    /// 返回新的 ContainerBasicInfo（Docker 含新 container_ip，供 service 层重注 pingora）。
    async fn patch_deployment(
        &self,
        _params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(ContainerRuntimeError::ConfigurationError(
            "patch_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 伸缩 Deployment 副本数（K8s scale；Docker: 0=stop, >=1=start）
    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        let _ = (app_id, replicas);
        Err(ContainerRuntimeError::ConfigurationError(
            "scale_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 触发滚动重启（K8s rollout annotation；Docker: stop+start）
    async fn restart_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let _ = app_id;
        Err(ContainerRuntimeError::ConfigurationError(
            "restart_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 修改闲置回收策略（只 patch Deployment `metadata.annotations`，不碰 pod template → 不触发 rollout）。
    /// 字段 `None` = 不改该键；至少一个 `Some` 由上层校验。K8s/Docker 均已 override；
    /// 未实现的 runtime 返回 Err（Fail Fast，避免策略被静默丢弃）。生效于下个扫描 tick。
    async fn patch_recycle_policy(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        let _ = (app_id, recycle_enabled, idle_timeout_seconds);
        Err(ContainerRuntimeError::ConfigurationError(
            "patch_recycle_policy not supported by this runtime".to_string(),
        ))
    }

    /// 持久化 scale-to-zero 的流量唤醒语义。默认 runtime 没有持久注解能力，允许 no-op。
    async fn patch_wake_on_traffic(
        &self,
        app_id: &str,
        enabled: bool,
    ) -> ContainerRuntimeResult<()> {
        let _ = (app_id, enabled);
        Ok(())
    }

    /// 删除 Deployment 及其关联资源（Service/HTTPRoute/ConfigMap/Secret 等）
    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let _ = app_id;
        Err(ContainerRuntimeError::ConfigurationError(
            "delete_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 实时查询 Deployment 运行时状态（供 app_manager 无状态化读路径）
    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let _ = app_id;
        Err(ContainerRuntimeError::ConfigurationError(
            "get_deployment_status not supported by this runtime".to_string(),
        ))
    }

    /// 读 app 当前容器的 `command`/`env` 快照（`update` 部分更新回退用，见
    /// [`ContainerSpecSnapshot`]）。
    ///
    /// 默认返回空快照（不支持时回退为空 = 保持旧行为）；K8s/Docker 重写读 live 容器。
    /// 仅在 `request.command`/`env` 为 None 时由 `app_manager::build_container_params_from_update`
    /// 用作回退，避免部分更新静默清空 → CrashLoop / 丢环境变量。
    async fn get_app_container_spec(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
        let _ = app_id;
        Ok(ContainerSpecSnapshot::default())
    }

    /// 列出当前 runtime 托管的所有 UserApp Deployment（供对账接口）
    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        Err(ContainerRuntimeError::ConfigurationError(
            "list_deployments not supported by this runtime".to_string(),
        ))
    }

    /// 拉取 app 容器的 stdout/stderr 日志（最近 `tail` 行）。
    ///
    /// K8s 经 Pod logs API（按 app-id label 定位 Pod）；Docker 经 `docker logs`。
    /// `timestamps=true` 时 K8s/Docker 在每行前缀 RFC3339 时间戳，由实现解析回 timestamp 字段。
    /// **`follow` 流式当前未实现**（返回 tail 快照），SSE/WebSocket 流式留待后续增强。
    async fn get_app_logs(
        &self,
        app_id: &str,
        tail: u32,
        timestamps: bool,
    ) -> ContainerRuntimeResult<Vec<ContainerLogEntry>> {
        let _ = (app_id, tail, timestamps);
        Err(ContainerRuntimeError::ConfigurationError(
            "get_app_logs not supported by this runtime".to_string(),
        ))
    }

    /// 启动日志**流**（follow），返回一个 mpsc::Receiver。
    ///
    /// runtime 内部 spawn 任务读取容器日志源（K8s `log_stream(follow)` / Docker `logs(follow)`），
    /// 逐行 send 到 channel。**receiver drop 即取消**：客户端断开 → handler 退出 → receiver 析构
    /// → runtime 任务的 send 出错 → 任务终止并释放日志源（服务端停止 follow）。
    /// `tail` 为起始历史行数（0 = 不取历史，仅 follow 新行）。
    async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> ContainerRuntimeResult<mpsc::Receiver<ContainerLogEntry>> {
        let _ = (app_id, tail);
        Err(ContainerRuntimeError::ConfigurationError(
            "stream_app_logs not supported by this runtime".to_string(),
        ))
    }

    /// 在 UserApp 容器内执行命令（exec）。
    ///
    /// 用于数据库管理等场景（reset-password / create-database：在 app 容器内跑 psql，
    /// 利用本地 trust 认证绕过当前密码）。默认不支持（返回 ConfigurationError），
    /// DockerRuntime 用 bollard exec、KubernetesRuntime 用 kube-rs exec 实现。
    /// `command` 是完整命令（如 `["sh","-c","psql -c ..."]`），容器内 sh 可展开镜像 ENV。
    async fn exec(
        &self,
        _app_id: &str,
        _command: Vec<String>,
    ) -> ContainerRuntimeResult<ExecResult> {
        Err(ContainerRuntimeError::ConfigurationError(
            "exec not supported by this runtime".to_string(),
        ))
    }

    /// 查询 app 相关的 K8s Events（Pod 调度/拉取/启动/崩溃事件）。
    /// 默认返回空（Docker 模式无 events 概念）。
    async fn get_app_events(&self, app_id: &str) -> ContainerRuntimeResult<Vec<AppEventInfo>> {
        let _ = app_id;
        Ok(vec![])
    }

    /// 查询 app 实时资源用量（CPU/内存用量 + 限额）。
    /// K8s 实现：metrics.k8s.io PodMetrics（用量）+ pod spec resources.limits（限额）。
    /// network（rx/tx）metrics.k8s.io 不提供，故不含。默认返回空（后端未实现/无 metrics-server），
    /// app_manager 层据此 + restart_count 组装对外 ResourceStats（用量 0 即降级为 0，不 500）。
    async fn get_app_resource_usage(&self, app_id: &str) -> ContainerRuntimeResult<ResourceUsage> {
        let _ = app_id;
        Ok(ResourceUsage::default())
    }

    /// 校验 app 管理前置条件（启动时 Fail Fast，防静默失败）
    ///
    /// K8s 模式探测 RBAC（list deployments，403 则明确报错指向 ClusterRole 缺权限）；
    /// Docker 模式默认 Ok。失败返回错误，调用方（AppService::new）据此 log warn 不阻塞启动
    /// （避免 API Server 临时不可达导致 rcoder 启动卡死）。
    async fn validate_app_prerequisites(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}

// ============================================================================
// 聚合 super-trait — ContainerRuntime (A + B + C) / UserAppRuntime (B + C 视图)
// ============================================================================

/// UserApp 视图: 同时具备 workspace (B) + UserApp Deployment (C) 能力, **不含 agent (A)**.
///
/// 供 app_manager 类型收紧 (file_server_embed 同理用 [`WorkspaceRuntime`] 单独收紧):
/// 任何调用点声明 `Arc<dyn UserAppRuntime>` 即可在编译期阻止使用 agent 方法 (Group A),
/// 暴露 ISP 违规 (设计上 app_manager 不该依赖 agent 能力).
///
/// **必须**作为 [`ContainerRuntime`] 的声明式 super-trait (而非仅 blanket impl) 才能让
/// trait 对象经 trait upcasting (Rust 1.86+) 从 `dyn ContainerRuntime` 收缩到 `dyn UserAppRuntime` ——
/// blanket impl 只对具体类型生效, 不改变 trait 对象的 super-trait 链.
pub trait UserAppRuntime: WorkspaceRuntime + UserAppDeploymentRuntime {}
impl<T> UserAppRuntime for T where T: WorkspaceRuntime + UserAppDeploymentRuntime {}

/// 聚合 super-trait: 同时具备 agent / workspace / UserApp 三类能力。
///
/// 旧调用点 (`Arc<dyn ContainerRuntime>`) 零改 —— 任何 impl A+B+C 的具体类型
/// (KubernetesRuntime / DockerRuntime) 自动 impl ContainerRuntime:
///   - A/B/C 由具体 impl 块提供;
///   - UserAppRuntime 由上方 blanket impl 自动提供 (B+C 已满足);
///   - ContainerRuntime 由下方 blanket impl 自动提供 (A+B+C+UserAppRuntime 已满足).
///
/// trait 对象经 trait upcasting (Rust 1.86+) 可收缩到任一子 trait:
///   `Arc<dyn ContainerRuntime>` → `Arc<dyn AgentContainerRuntime>` / `Arc<dyn WorkspaceRuntime>`
///   / `Arc<dyn UserAppDeploymentRuntime>` / `Arc<dyn UserAppRuntime>` 均可直接 upcast.
pub trait ContainerRuntime:
    AgentContainerRuntime + WorkspaceRuntime + UserAppDeploymentRuntime + UserAppRuntime
{
}

// Blanket impl: 任何 A+B+C+UserAppRuntime 类型自动 impl ContainerRuntime.
// Rust 不凭 super-trait bounds 自动 impl (即使 trait 体为空), 需显式声明 blanket impl.
// 含 `?Sized` 以覆盖 `dyn ContainerRuntime` 自身 (理论上可被嵌套 upcast).
impl<T> ContainerRuntime for T where
    T: AgentContainerRuntime
        + WorkspaceRuntime
        + UserAppDeploymentRuntime
        + UserAppRuntime
        + ?Sized
{
}
