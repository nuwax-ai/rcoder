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
    /// Total pre-admission wait for an explicit production restart. This is
    /// independent from application shutdown grace and deployment execution.
    pub restart_admission_wait_secs: u64,
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
            // Default is infallible and does not inspect unselected input.
            // The loader resolves environment only for an enabled manager
            // without an explicit budget section.
            deploy_budget: DeployBudgetConfig::default(),
            restart_admission_wait_secs: 30,
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

/// Load selected deployment budget input. An invalid value remains a typed
/// configuration failure; callers decide whether this configuration is in use.
pub fn deploy_budget_from_env() -> Result<DeployBudgetConfig, shared_types::AppError> {
    deploy_budget_from_env_with(|key| std::env::var_os(key))
}

pub fn restart_admission_wait_from_env() -> Result<u64, shared_types::AppError> {
    restart_admission_wait_from_env_with(|| {
        std::env::var_os("RCODER_USERAPP_RESTART_ADMISSION_WAIT_SECS")
    })
}

pub fn validate_restart_admission_wait_secs(seconds: u64) -> Result<(), shared_types::AppError> {
    if seconds == 0
        || std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(seconds.saturating_add(300)))
            .is_none()
    {
        return Err(shared_types::AppError::with_message(
            shared_types::ERR_RUNTIME_CONFIGURATION,
            "restart_admission_wait_secs must be positive and within the monotonic clock range",
        ));
    }
    Ok(())
}

pub fn restart_admission_wait_from_env_with(
    lookup: impl FnOnce() -> Option<std::ffi::OsString>,
) -> Result<u64, shared_types::AppError> {
    let Some(value) = lookup() else {
        return Ok(30);
    };
    let invalid = || {
        shared_types::AppError::with_message(
            shared_types::ERR_RUNTIME_CONFIGURATION,
            "RCODER_USERAPP_RESTART_ADMISSION_WAIT_SECS must be a positive unsigned integer",
        )
    };
    let seconds = value
        .into_string()
        .map_err(|_| invalid())?
        .parse::<u64>()
        .map_err(|_| invalid())?;
    validate_restart_admission_wait_secs(seconds)?;
    Ok(seconds)
}

/// Explicit lookup makes environment resolution deterministic without mutating
/// process-global environment in tests or changing configuration precedence.
pub fn deploy_budget_from_env_with(
    mut lookup: impl FnMut(&str) -> Option<std::ffi::OsString>,
) -> Result<DeployBudgetConfig, shared_types::AppError> {
    fn parse<T: std::str::FromStr>(
        lookup: &mut impl FnMut(&str) -> Option<std::ffi::OsString>,
        key: &str,
        default: T,
        kind: &str,
    ) -> Result<T, shared_types::AppError> {
        let Some(value) = lookup(key) else {
            return Ok(default);
        };
        let invalid = || {
            let detail = format!(
                "Invalid deployment budget configuration: {key} must be a Unicode unsigned {kind} integer"
            );
            shared_types::AppError::with_message(shared_types::ERR_RUNTIME_CONFIGURATION, &detail)
                .with_error_detail(shared_types::ErrorDetail::new(
                    shared_types::ERR_RUNTIME_CONFIGURATION,
                    "deploy_budget_configuration",
                    detail,
                ))
        };
        // Never echo environment values: a misconfigured field may contain a
        // secret copied into the wrong key. Missing and non-Unicode differ.
        let text = value.into_string().map_err(|_| invalid())?;
        text.parse::<T>().map_err(|_| invalid())
    }
    let defaults = DeployBudgetConfig::default();
    Ok(DeployBudgetConfig {
        sql_stage_budget_secs: parse(
            &mut lookup,
            "RCODER_USERAPP_DEPLOY_SQL_STAGE_BUDGET_SECS",
            defaults.sql_stage_budget_secs,
            "64-bit",
        )?,
        no_progress_timeout_secs: parse(
            &mut lookup,
            "RCODER_USERAPP_DEPLOY_NO_PROGRESS_TIMEOUT_SECS",
            defaults.no_progress_timeout_secs,
            "64-bit",
        )?,
        pre_appcli_stage_budget_secs: parse(
            &mut lookup,
            "RCODER_USERAPP_DEPLOY_PRE_APPCLI_STAGE_BUDGET_SECS",
            defaults.pre_appcli_stage_budget_secs,
            "64-bit",
        )?,
        absolute_budget_secs: parse(
            &mut lookup,
            "RCODER_USERAPP_DEPLOY_ABSOLUTE_BUDGET_SECS",
            defaults.absolute_budget_secs,
            "64-bit",
        )?,
        failure_restart_threshold: parse(
            &mut lookup,
            "RCODER_USERAPP_DEPLOY_FAILURE_RESTART_THRESHOLD",
            defaults.failure_restart_threshold,
            "32-bit",
        )?,
        oom_restart_threshold: parse(
            &mut lookup,
            "RCODER_USERAPP_DEPLOY_OOM_RESTART_THRESHOLD",
            defaults.oom_restart_threshold,
            "32-bit",
        )?,
        fenced_alert_after_secs: parse(
            &mut lookup,
            "RCODER_USERAPP_DEPLOY_FENCED_ALERT_AFTER_SECS",
            defaults.fenced_alert_after_secs,
            "64-bit",
        )?,
    })
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
        // 推导链：显式 env > PATH_MAP 映射（保持"锁根与数据根同树"——
        // map 覆盖 /app/userapp-workspace 后两者同变）> ~/.rcoder 默认
        if let Ok(explicit) = std::env::var("RCODER_OPERATION_LOCK_ROOT") {
            return explicit;
        }
        if let Ok(map) = docker_manager::path::host_map::resolve_map()
            && let Some(host_root) = docker_manager::path::host_map::resolve_host_path(
                &map,
                std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT),
            )
        {
            return host_root.to_string_lossy().into_owned();
        }
        std::env::var("HOME")
            .map(|home| format!("{home}/.rcoder/workspace/userapp"))
            .unwrap_or_else(|_| shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT.to_owned())
    }
    #[cfg(not(feature = "deploy-host"))]
    {
        shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT.to_owned()
    }
}

#[cfg(test)]
mod deploy_budget_tests {
    #[test]
    fn restart_wait_configuration_has_positive_bounded_selected_input() {
        assert_eq!(
            super::restart_admission_wait_from_env_with(|| None).unwrap(),
            30
        );
        assert_eq!(
            super::restart_admission_wait_from_env_with(|| Some("7".into())).unwrap(),
            7
        );
        for value in ["0", "-1", "misplaced-secret-value", "18446744073709551615"] {
            let error =
                super::restart_admission_wait_from_env_with(|| Some(value.into())).unwrap_err();
            assert!(!error.to_string().contains("misplaced-secret-value"));
            assert_eq!(
                error.into_http_result::<()>("en-US").code,
                shared_types::ERR_RUNTIME_CONFIGURATION
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            assert!(
                super::restart_admission_wait_from_env_with(|| Some(std::ffi::OsString::from_vec(
                    vec![0xff]
                )))
                .is_err()
            );
        }
    }

    #[test]
    fn selected_budget_values_and_invalid_errors_keep_configuration_evidence() {
        let valid = super::deploy_budget_from_env_with(|key| {
            if key == "RCODER_USERAPP_DEPLOY_SQL_STAGE_BUDGET_SECS" {
                Some("27".into())
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(valid.sql_stage_budget_secs, 27);
        assert_eq!(valid.failure_restart_threshold, 3);
        let error = super::deploy_budget_from_env_with(|key| {
            if key == "RCODER_USERAPP_DEPLOY_FAILURE_RESTART_THRESHOLD" {
                Some("misplaced-secret-value".into())
            } else {
                None
            }
        })
        .unwrap_err();
        assert!(!error.to_string().contains("misplaced-secret-value"));
        let response = error.into_http_result::<()>("zh-CN");
        assert_eq!(response.code, shared_types::ERR_RUNTIME_CONFIGURATION);
        let detail = response.error_detail.unwrap();
        assert_eq!(detail.stage, "deploy_budget_configuration");
        assert!(!detail.retryable);
        assert_eq!(
            detail.hint,
            shared_types::get_error_hint(shared_types::ERR_RUNTIME_CONFIGURATION, "zh-CN")
        );
    }
}
