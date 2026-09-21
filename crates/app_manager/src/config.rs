//! 应用管理服务配置

use serde::{Deserialize, Serialize};
use tracing::warn;

use container_runtime_api::HttpExpose;

/// Gateway NodePort 默认值（未配置时使用）
const DEFAULT_GATEWAY_NODE_PORT: u16 = 30080;

/// 从 env `RCODER_APP_HTTP_EXPOSE` 读取 HTTP 暴露策略（pingora|gateway，默认 pingora）。
/// 无效值 warn 后回退 Pingora（Fail Fast：显眼告警，避免静默走错模式）。
/// **必须与 `docker_manager::kubernetes_runtime` 同源读取**，保证 service 层与 K8s 后端一致。
fn http_expose_from_env() -> HttpExpose {
    match std::env::var("RCODER_APP_HTTP_EXPOSE").ok().as_deref() {
        Some("gateway") => HttpExpose::Gateway,
        Some("pingora") | None => HttpExpose::Pingora,
        Some(other) => {
            warn!(
                "unrecognized RCODER_APP_HTTP_EXPOSE={other:?}, falling back to pingora (valid: pingora|gateway)"
            );
            HttpExpose::Pingora
        }
    }
}

/// 应用后端运行时模式（Docker / Kubernetes）—— 仅决定资源形态（容器 vs Deployment）。
///
/// 注意：HTTP 对外暴露机制由 [`HttpExpose`]（`http_expose` 配置）决定，不再绑死后端模式；
/// TCP 初期不对外。详见设计文档 §8。
///
/// 默认由编译期 feature 决定（kubernetes → Kubernetes，否则 Docker），
/// 可被环境变量 `RCODER_APP_ACCESS_MODE` 覆盖，便于双后端运行时切换。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppAccessMode {
    Docker,
    Kubernetes,
}

impl Default for AppAccessMode {
    fn default() -> Self {
        // 运行时统一构造（plan G2）：默认值由 feature 给出，env 可覆盖。
        match std::env::var("RCODER_APP_ACCESS_MODE").ok().as_deref() {
            Some("docker") => AppAccessMode::Docker,
            Some("kubernetes") | Some("k8s") => AppAccessMode::Kubernetes,
            _ => {
                #[cfg(feature = "kubernetes")]
                {
                    AppAccessMode::Kubernetes
                }
                #[cfg(not(feature = "kubernetes"))]
                {
                    AppAccessMode::Docker
                }
            }
        }
    }
}

/// 应用管理服务配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppManagerConfig {
    /// 是否启用
    pub enabled: bool,

    /// 工作空间根目录（Docker 模式 = 宿主机路径；K8s 模式 = rcoder Pod 内 PVC 挂载点）
    pub workspace_root: Option<String>,

    /// Docker operation locks must live on the shared userApp data filesystem.
    /// All replicas using that data must use the same directory.
    pub operation_lock_root: String,

    /// K8s 命名空间
    pub namespace: String,

    /// K8s Gateway 名称（可选，未配置时不使用 Gateway）
    pub gateway_name: Option<String>,

    /// K8s Gateway 命名空间（可选）
    pub gateway_namespace: Option<String>,

    /// K8s 节点 IP（可选，用于外部访问地址构建）
    pub node_ip: Option<String>,

    /// K8s Gateway NodePort（可选）
    pub gateway_node_port: Option<u16>,

    /// 存储类（K8s，保留字段；当前 app 复用 rcoder-workspace PVC，不新建独立 PVC）
    pub storage_class: Option<String>,

    // ===== Layer 5 接线字段 =====
    /// 后端运行时模式（Docker / Kubernetes）
    pub access_mode: AppAccessMode,

    /// HTTP 服务对外暴露策略（v2 D10）：Pingora（默认，两后端统一）/ Gateway（可选，HTTPRoute）。
    /// 决定 HTTP 端口走 RCoder 内置 Pingora（`/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}`，免端口）还是外部 Gateway（`/apps/{id}`）。
    /// **只从 env `RCODER_APP_HTTP_EXPOSE` 读取**（serde skip，禁止 config.yml 覆盖）——
    /// 保证与 docker_manager K8s 后端（也读 env）同源一致。
    #[serde(skip, default = "http_expose_from_env")]
    pub http_expose: HttpExpose,

    /// 工作空间 PVC 名（K8s 模式，app 复用的 RWX PVC；运行时也直接读 env
    /// `RCODER_WORKSPACE_PVC_NAME`，此处仅作可观测/兜底）
    pub workspace_pvc_name: Option<String>,

    /// 部署预算配置（旁路：stage/absolute/SQL/no-progress/failure thresholds）
    pub deploy_budget: DeployBudgetConfig,
}

impl Default for AppManagerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            workspace_root: std::env::var("RCODER_WORKSPACE_ROOT").ok(),
            operation_lock_root: default_operation_lock_root(),
            namespace: std::env::var("RCODER_K8S_NAMESPACE")
                .unwrap_or_else(|_| "default".to_string()),
            gateway_name: std::env::var("RCODER_K8S_GATEWAY_NAME").ok(),
            gateway_namespace: std::env::var("RCODER_K8S_GATEWAY_NAMESPACE").ok(),
            node_ip: std::env::var("RCODER_K8S_NODE_IP").ok(),
            gateway_node_port: std::env::var("RCODER_K8S_GATEWAY_NODE_PORT")
                .ok()
                .and_then(|s| s.parse().ok()),
            storage_class: std::env::var("RCODER_K8S_STORAGE_CLASS").ok(),
            access_mode: AppAccessMode::default(),
            http_expose: http_expose_from_env(),
            workspace_pvc_name: std::env::var("RCODER_WORKSPACE_PVC_NAME").ok(),
            deploy_budget: deploy_budget_from_env(),
        }
    }
}

impl AppManagerConfig {
    /// 获取节点 IP
    pub fn get_node_ip(&self) -> String {
        self.node_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    /// 获取 Gateway 名称
    pub fn get_gateway_name(&self) -> String {
        self.gateway_name
            .clone()
            .unwrap_or_else(|| "nuwax-gateway".to_string())
    }

    /// 获取 Gateway 命名空间
    pub fn get_gateway_namespace(&self) -> String {
        self.gateway_namespace
            .clone()
            .unwrap_or_else(|| "default".to_string())
    }

    /// 获取 Gateway NodePort
    pub fn get_gateway_node_port(&self) -> u16 {
        self.gateway_node_port.unwrap_or(DEFAULT_GATEWAY_NODE_PORT)
    }
}

/// 部署预算配置。所有值单位为秒。
/// 环境变量覆盖：`RCODER_USERAPP_DEPLOY_<FIELD>_SECS`（failure_restart_threshold
/// 和 oom_restart_threshold 使用 `RCODER_USERAPP_DEPLOY_<FIELD>`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DeployBudgetConfig {
    /// progress_v1 活跃且 step=running_sql 时使用此值替代 no-progress timer；
    /// 同时也是 SQL 阶段预算（活动增长不退出）。
    pub sql_stage_budget_secs: u64,
    /// 非 SQL 阶段的 no-progress 超时（仅 progress_v1 声明时生效）。
    pub no_progress_timeout_secs: u64,
    /// pre-appcli 阶段预算（app-cli download + 启动前的总时间）。
    pub pre_appcli_stage_budget_secs: u64,
    /// 绝对 deadline（从受理起的墙钟总时间上限，epoch ms 绑定到旁记录）。
    pub absolute_budget_secs: u64,
    /// 连续 crash restart 次数阈值（触发 CrashLoopBackOff 分类）。
    pub failure_restart_threshold: u32,
    /// 连续 OOM restart 次数阈值（触发 OomRestartStorm 分类）。
    pub oom_restart_threshold: u32,
    /// 围栏状态多久后触发告警（秒）。
    pub fenced_alert_after_secs: u64,
}

impl Default for DeployBudgetConfig {
    fn default() -> Self {
        Self {
            sql_stage_budget_secs: 1800,
            no_progress_timeout_secs: 600,
            pre_appcli_stage_budget_secs: 1800,
            absolute_budget_secs: 3600,
            failure_restart_threshold: 3,
            oom_restart_threshold: 2,
            fenced_alert_after_secs: 900,
        }
    }
}

/// 从环境变量读取部署预算配置，无效值直接 panic（fail-fast）。
pub fn deploy_budget_from_env() -> DeployBudgetConfig {
    let d = DeployBudgetConfig::default();
    let parse_env = |key: &str, default: u64| -> u64 {
        std::env::var(key)
            .ok()
            .map(|v| {
                v.parse::<u64>()
                    .unwrap_or_else(|e| panic!("{key}={v}: invalid u64: {e}"))
            })
            .unwrap_or(default)
    };
    let parse_u32 = |key: &str, default: u32| -> u32 {
        std::env::var(key)
            .ok()
            .map(|v| {
                v.parse::<u32>()
                    .unwrap_or_else(|e| panic!("{key}={v}: invalid u32: {e}"))
            })
            .unwrap_or(default)
    };
    DeployBudgetConfig {
        sql_stage_budget_secs: parse_env(
            "RCODER_USERAPP_DEPLOY_SQL_STAGE_BUDGET_SECS",
            d.sql_stage_budget_secs,
        ),
        no_progress_timeout_secs: parse_env(
            "RCODER_USERAPP_DEPLOY_NO_PROGRESS_TIMEOUT_SECS",
            d.no_progress_timeout_secs,
        ),
        pre_appcli_stage_budget_secs: parse_env(
            "RCODER_USERAPP_DEPLOY_PRE_APPCLI_STAGE_BUDGET_SECS",
            d.pre_appcli_stage_budget_secs,
        ),
        absolute_budget_secs: parse_env(
            "RCODER_USERAPP_DEPLOY_ABSOLUTE_BUDGET_SECS",
            d.absolute_budget_secs,
        ),
        failure_restart_threshold: parse_u32(
            "RCODER_USERAPP_DEPLOY_FAILURE_RESTART_THRESHOLD",
            d.failure_restart_threshold,
        ),
        oom_restart_threshold: parse_u32(
            "RCODER_USERAPP_DEPLOY_OOM_RESTART_THRESHOLD",
            d.oom_restart_threshold,
        ),
        fenced_alert_after_secs: parse_env(
            "RCODER_USERAPP_DEPLOY_FENCED_ALERT_AFTER_SECS",
            d.fenced_alert_after_secs,
        ),
    }
}

/// 运行时数据根（Docker 形态锁根与数据根同树约定的单一事实源）。
///
/// 容器形态：常量 `/app/userapp-workspace`。deploy-host 宿主机形态：容器常量
/// 路径不存在，锚定 `~/.rcoder/workspace/userapp`（与 docker_manager host_map
/// 默认同源约定）；env `RCODER_OPERATION_LOCK_ROOT` 显式设置永远优先。
/// [`crate::service::AppService::new`] 的锁根一致性校验与
/// [`AppManagerConfig`] 默认值**必须**共用本函数——两处分头推导会造成
/// 宿主机形态启动即 Validation 拒启（2026-09-22 自检实测回归）。
pub fn default_operation_lock_root() -> String {
    #[cfg(feature = "deploy-host")]
    {
        std::env::var("RCODER_OPERATION_LOCK_ROOT").unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|home| format!("{home}/.rcoder/workspace/userapp"))
                .unwrap_or_else(|_| shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT.to_owned())
        })
    }
    #[cfg(not(feature = "deploy-host"))]
    {
        shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT.to_owned()
    }
}
