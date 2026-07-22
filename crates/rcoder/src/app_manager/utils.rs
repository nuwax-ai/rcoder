//! service 层工具函数（错误映射 / 校验 / 状态派生 / 类型映射）

use container_runtime_api::{
    ContainerRuntimeError, DeploymentStatus, ExposeType as RtExposeType,
    HealthCheckType as RtHealthCheckType,
};
use download_utils::ArchiveError;
use shared_types::ServiceType;

use super::models::*;

/// ContainerRuntimeError → AppOperationError 精确映射。
///
/// `ctx` = 动作前缀（如 "[APP] create_deployment 失败 app_id=app-xxx"），拼入 message。
/// thiserror variant 无 source，`{e}` 即 variant Display（含原始 daemon message）。
pub(super) fn map_runtime_error(ctx: &str, e: ContainerRuntimeError) -> AppOperationError {
    match e {
        // 容器/deployment 不存在 = app 不存在（404）
        ContainerRuntimeError::ContainerNotFound(_) => {
            AppOperationError::NotFound(format!("{ctx}: {e}"))
        }
        // 其余 8 类（Connection/Creation/Start/Stop/Configuration/Timeout/K8s/Docker）
        // 都是后端运行时/基础设施问题，客户端不可恢复，归 Backend(500)。
        // ConfigurationError 在 runtime 是内部前置条件（params 缺字段），非用户输入，
        // 归 Backend 最保守，避免误判 400。
        _ => AppOperationError::Backend(format!("{ctx}: {e}")),
    }
}

/// io::Error → AppOperationError 精确映射。
///
/// `is_file_op=true`（read_to_string/write/remove_file）：NotFound → FileNotFound(404)
/// `is_file_op=false`（create_dir_all/metadata/canonicalize/read_dir/remove_dir_all）：→ Backend
/// （目录层 NotFound 通常已被上游 app_dir.exists() 守卫拦截，漏到这属异常，归 Backend）
pub(super) fn map_io_error(ctx: &str, e: std::io::Error, is_file_op: bool) -> AppOperationError {
    match e.kind() {
        std::io::ErrorKind::NotFound if is_file_op => {
            AppOperationError::FileNotFound(format!("{ctx}: {e}"))
        }
        _ => AppOperationError::Backend(format!("{ctx}: {e}")),
    }
}

/// `ArchiveError`（download_utils 解压错误）→ `AppOperationError`。
/// 非法路径 / 解压超限 / 无效压缩包 → `Validation`（400，客户端错误）；
/// IO → `Backend`（500）。
pub(super) fn map_archive_error(e: ArchiveError) -> AppOperationError {
    match e {
        ArchiveError::PathTraversal(msg) => {
            AppOperationError::Validation(format!("archive contains illegal path: {msg}"))
        }
        ArchiveError::ArchiveBomb { size, max } => AppOperationError::Validation(format!(
            "archive extraction exceeded size limit: {size} > {max}"
        )),
        ArchiveError::InvalidArchive(msg) => {
            AppOperationError::Validation(format!("invalid archive: {msg}"))
        }
        ArchiveError::Io(e) => map_io_error("archive IO error", e, true),
    }
}

/// 校验 upload target（app 根相对路径）。
///
/// 拒绝空 / 绝对路径 / 含 `..` 组件——在 `create_dir_all` **之前**拦截，避免 path traversal
/// 副作用（target 含 `../` 时 create_dir_all 会先在工作空间外创建目录，虽后续 `starts_with`
/// 拒绝，但目录已落盘）。
pub(super) fn validate_upload_target(target: &str) -> AppResult<()> {
    if target.trim_end_matches('/').is_empty() {
        return Err(AppOperationError::Validation(
            "target must not be empty".to_string(),
        ));
    }
    if target.starts_with('/') {
        return Err(AppOperationError::Validation(
            "target must be relative (app-root-relative)".to_string(),
        ));
    }
    if std::path::Path::new(target)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AppOperationError::Validation(
            "target must not contain '..'".to_string(),
        ));
    }
    Ok(())
}

/// 校验 app_id 格式（create_app 生成 `app-` + 8 个十六进制字符）
///
/// app_id 直接来自 HTTP 路径参数，会流入文件系统路径拼接（delete/upload/logs/list）。
/// 此校验是路径穿越的纵深防御（Fail Fast）：拒绝 `..`、绝对路径、非法格式，
/// 避免恶意 app_id 触达工作空间目录之外。
pub(super) fn validate_app_id(app_id: &str) -> AppResult<()> {
    // 必须 app- 前缀（统一，和自动生成一致）
    let rest = app_id.strip_prefix("app-").ok_or_else(|| {
        AppOperationError::Validation("invalid app_id: must start with 'app-'".to_string())
    })?;
    if rest.is_empty() {
        return Err(AppOperationError::Validation(
            "invalid app_id: empty after 'app-'".to_string(),
        ));
    }
    // DNS-1123 label 合规（[a-z0-9]([-a-z0-9]*[a-z0-9])?，≤63；支持 app-order-svc 等业务名）
    if rest.len() > 63
        || !rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(AppOperationError::Validation(format!(
            "invalid app_id: must be DNS-1123 label (lowercase alphanumeric or '-', got '{rest}')"
        )));
    }
    if rest.starts_with('-') || rest.ends_with('-') {
        return Err(AppOperationError::Validation(
            "invalid app_id: must not start/end with '-'".to_string(),
        ));
    }
    Ok(())
}

/// 校验 PG 标识符（数据库名 / 用户名）：首字符字母或下划线，其余字母/数字/下划线，≤63 字符。
/// 防 SQL 注入（标识符进双引号，但严格白名单更稳）。
pub(super) fn validate_pg_identifier(name: &str) -> AppResult<()> {
    if name.is_empty() || name.len() > 63 {
        return Err(AppOperationError::Validation(
            "PG identifier must be 1..=63 chars".to_string(),
        ));
    }
    match name.chars().next() {
        Some(first) if !(first.is_ascii_alphabetic() || first == '_') => {
            return Err(AppOperationError::Validation(
                "PG identifier must start with letter or '_'".to_string(),
            ));
        }
        None => {
            return Err(AppOperationError::Validation(
                "PG identifier empty".to_string(),
            ))
        }
        _ => {}
    }
    if !name.chars().skip(1).all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(AppOperationError::Validation(
            "PG identifier: only [a-zA-Z0-9_] allowed".to_string(),
        ));
    }
    Ok(())
}

/// 从 PortConfig 列表提取 HTTP 端口号（供 Pingora backend 注册，create/update 共用）
pub(super) fn http_port_numbers(ports: &Option<Vec<PortConfig>>) -> Vec<u16> {
    ports
        .as_ref()
        .map(|ps| {
            ps.iter()
                .filter(|p| p.expose_type == ExposeType::Http)
                .map(|p| p.port)
                .collect()
        })
        .unwrap_or_default()
}

/// 运行时 phase → 应用状态枚举
pub(super) fn phase_to_status(phase: &str) -> AppStatus {
    match phase {
        "Running" => AppStatus::Running,
        "Stopped" | "ScaledDown" => AppStatus::Stopped,
        "Starting" | "Pending" | "Creating" => AppStatus::Starting,
        "Error" | "Failed" => AppStatus::Error,
        _ => AppStatus::Created,
    }
}

/// 从 message 中提取简短机器码原因（CrashLoopBackOff / ImagePullBackOff 等）
pub(super) fn extract_reason(msg: &str) -> Option<&str> {
    const KNOWN: &[&str] = &[
        "CrashLoopBackOff",
        "ImagePullBackOff",
        "ErrImagePull",
        "CreateContainerConfigError",
        "CreateContainerError",
        "InvalidImageName",
        "RunContainerError",
        "StartError",
        "OOMKilled",
    ];
    KNOWN.iter().find(|k| msg.contains(*k)).copied()
}

/// 由 DeploymentStatus 派生 conditions（见设计文档 §6.3 派生表）
///
/// 与 headline 的 `AppStatus` 同源、不矛盾：`status` 给 Java 做状态机判断，
/// `conditions[]` 给人/前端做细粒度诊断。`last_transition_time` 在无状态下不持久
/// 追踪（rcoder 不持有上一时刻状态），统一为 `None`。
pub(super) fn derive_conditions(status: &DeploymentStatus) -> Vec<Condition> {
    let app_status = phase_to_status(&status.phase);
    let mk = |t: &str, s: &str, reason: Option<&str>, msg: Option<String>| Condition {
        r#type: t.to_string(),
        status: s.to_string(),
        reason: reason.map(str::to_string),
        message: msg,
        last_transition_time: None,
    };
    match app_status {
        AppStatus::Error => {
            let reason = status
                .message
                .as_deref()
                .and_then(extract_reason)
                .unwrap_or("Error");
            vec![
                mk("Error", "True", Some(reason), status.message.clone()),
                mk("Ready", "False", Some("Error"), None),
            ]
        }
        AppStatus::Running => vec![
            mk("Ready", "True", None, None),
            mk("Available", "True", None, None),
        ],
        AppStatus::Stopped => vec![
            mk("Ready", "False", Some("ScaledDown"), None),
            mk("Available", "False", Some("ScaledDown"), None),
        ],
        AppStatus::Starting => vec![
            mk("Progressing", "True", Some("Starting"), None),
            mk("Ready", "False", Some("Starting"), None),
        ],
        AppStatus::Stopping => vec![mk("Progressing", "True", Some("Stopping"), None)],
        AppStatus::Deleting => vec![mk("Progressing", "True", Some("Deleting"), None)],
        AppStatus::Created => vec![mk("Ready", "False", Some("Created"), None)],
    }
}

/// 由 DeploymentStatus 派生健康信息
pub(super) fn health_from_status(status: &DeploymentStatus) -> HealthInfo {
    HealthInfo {
        status: status.phase.clone(),
        instance: Some(InstanceInfo {
            name: format!(
                "{}-{}",
                ServiceType::UserApp.container_prefix(),
                status.app_id
            ),
            phase: status.phase.clone(),
            ready: status.ready_replicas > 0,
            restart_count: status.restart_count,
            node: status.node.clone().unwrap_or_default(),
            ip: status.pod_ip.clone().unwrap_or_default(),
            started_at: status.started_at.clone(),
        }),
        probes: None,
    }
}

/// models::ExposeType → container_runtime_api::ExposeType
pub(super) fn map_expose_type(e: &ExposeType) -> RtExposeType {
    match e {
        ExposeType::Http => RtExposeType::Http,
        ExposeType::Tcp => RtExposeType::Tcp,
    }
}

/// models::HealthCheckType → container_runtime_api::HealthCheckType
pub(super) fn map_health_check_type(t: &HealthCheckType) -> RtHealthCheckType {
    match t {
        HealthCheckType::Http => RtHealthCheckType::Http,
        HealthCheckType::Tcp => RtHealthCheckType::Tcp,
        HealthCheckType::Exec => RtHealthCheckType::Exec,
        HealthCheckType::None => RtHealthCheckType::None,
    }
}
