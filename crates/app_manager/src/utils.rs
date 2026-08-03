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
        ArchiveError::TooManyEntries { count, max } => {
            AppOperationError::Validation(format!("archive has too many entries: {count} > {max}"))
        }
        ArchiveError::EntryTooLarge { size, max } => {
            AppOperationError::Validation(format!("archive entry too large: {size} > {max}"))
        }
        ArchiveError::PathTooLong { len, max } => {
            AppOperationError::Validation(format!("archive entry path too long: {len} > {max}"))
        }
        ArchiveError::Io(e) => map_io_error("archive IO error", e, true),
    }
}

/// canonicalize `target` 并校验仍在 `canonical_app_dir` 内（path traversal 防护）。
///
/// 文件操作（upload/extract/list/delete）共用。
/// 调用前需保证 `target` 已存在（否则 canonicalize 抛 OS 错误 → Backend）；
/// 需要 NotFound 语义的调用方（如日志文件读取）应先 `target.exists()` 守卫。
/// `canonical_app_dir` 应由调用方预先 canonicalize（通常在创建目录后立即取）。
pub(super) fn ensure_within_app_dir(
    target: &std::path::Path,
    canonical_app_dir: &std::path::Path,
) -> AppResult<std::path::PathBuf> {
    let canonical = target
        .canonicalize()
        .map_err(|e| map_io_error("failed to resolve path", e, false))?;
    if !canonical.starts_with(canonical_app_dir) {
        return Err(AppOperationError::Validation(
            "path is outside app dir".to_string(),
        ));
    }
    Ok(canonical)
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
    // 首字符：必须字母或下划线（is_empty 已由上方长度校验拦截，无需再处理 None）
    if let Some(first) = name.chars().next()
        && !(first.is_ascii_alphabetic() || first == '_')
    {
        return Err(AppOperationError::Validation(
            "PG identifier must start with letter or '_'".to_string(),
        ));
    }
    if !name
        .chars()
        .skip(1)
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
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

/// 校验 K8s 存储大小（K8s Quantity）：1Gi ≤ size ≤ 100Ti
///
/// 独立校验函数（不依赖 rcoder），create/update 时 storage/ephemeral_storage 校验共用。
/// 与 `rcoder::handler::pod_handler::validate_k8s_storage_size` 语义一致（同源）。
// validate_k8s_storage_size 下沉到 container-runtime-api（共享，避免双份维护）
pub(super) use container_runtime_api::validate_k8s_storage_size;

#[cfg(test)]
mod tests {
    use super::*;
    use container_runtime_api::{ExposeType as RtExposeType, HealthCheckType as RtHealthCheckType};

    // ---------------- validate_app_id ----------------

    #[test]
    fn validate_app_id_ok() {
        assert!(validate_app_id("app-order-svc").is_ok());
        assert!(validate_app_id("app-1a2b3c4d").is_ok());
        assert!(validate_app_id("app-a").is_ok()); // 最短合法
    }

    #[test]
    fn validate_app_id_err_no_prefix() {
        // 无 app- 前缀
        assert!(validate_app_id("order-svc").is_err());
    }

    #[test]
    fn validate_app_id_err_empty_after_prefix() {
        // prefix 后为空
        assert!(validate_app_id("app-").is_err());
    }

    #[test]
    fn validate_app_id_err_uppercase() {
        // 大写非法（DNS-1123 label）
        assert!(validate_app_id("app-UPPER").is_err());
    }

    #[test]
    fn validate_app_id_err_path_traversal() {
        // 含 ../ 等穿越字符
        assert!(validate_app_id("app-../../../etc").is_err());
    }

    #[test]
    fn validate_app_id_err_too_long() {
        // rest > 63 字符
        let too_long = format!("app-{}", "a".repeat(64));
        assert!(validate_app_id(&too_long).is_err());
    }

    #[test]
    fn validate_app_id_err_trailing_dash() {
        // 尾部 '-' 非法
        assert!(validate_app_id("app-trailing-").is_err());
    }

    // ---------------- validate_pg_identifier ----------------

    #[test]
    fn validate_pg_identifier_ok() {
        assert!(validate_pg_identifier("mydb").is_ok());
        assert!(validate_pg_identifier("_underscore").is_ok());
        assert!(validate_pg_identifier("MixedCase123").is_ok());
    }

    #[test]
    fn validate_pg_identifier_err_empty() {
        assert!(validate_pg_identifier("").is_err());
    }

    #[test]
    fn validate_pg_identifier_err_starts_with_digit() {
        assert!(validate_pg_identifier("1num").is_err());
    }

    #[test]
    fn validate_pg_identifier_err_dash() {
        assert!(validate_pg_identifier("has-dash").is_err());
    }

    #[test]
    fn validate_pg_identifier_err_space() {
        assert!(validate_pg_identifier("has space").is_err());
    }

    #[test]
    fn validate_pg_identifier_err_injection() {
        assert!(validate_pg_identifier("a;b").is_err());
    }

    // ---------------- validate_upload_target ----------------

    #[test]
    fn validate_upload_target_ok() {
        assert!(validate_upload_target("code/app.jar").is_ok());
        assert!(validate_upload_target("data/db/file").is_ok());
    }

    #[test]
    fn validate_upload_target_err_traversal() {
        assert!(validate_upload_target("../etc/passwd").is_err());
    }

    #[test]
    fn validate_upload_target_err_absolute() {
        assert!(validate_upload_target("/absolute").is_err());
    }

    #[test]
    fn validate_upload_target_err_empty() {
        assert!(validate_upload_target("").is_err());
    }

    // ---------------- phase_to_status ----------------

    #[test]
    fn phase_to_status_running() {
        assert_eq!(phase_to_status("Running"), AppStatus::Running);
    }

    #[test]
    fn phase_to_status_stopped() {
        assert_eq!(phase_to_status("Stopped"), AppStatus::Stopped);
        assert_eq!(phase_to_status("ScaledDown"), AppStatus::Stopped);
    }

    #[test]
    fn phase_to_status_starting() {
        assert_eq!(phase_to_status("Starting"), AppStatus::Starting);
        assert_eq!(phase_to_status("Pending"), AppStatus::Starting);
    }

    #[test]
    fn phase_to_status_error() {
        assert_eq!(phase_to_status("Error"), AppStatus::Error);
        assert_eq!(phase_to_status("Failed"), AppStatus::Error);
    }

    #[test]
    fn phase_to_status_unknown_falls_back_to_created() {
        assert_eq!(phase_to_status("unknown"), AppStatus::Created);
    }

    // ---------------- extract_reason ----------------

    #[test]
    fn extract_reason_finds_known_code() {
        assert_eq!(
            extract_reason("Back-off restarting... CrashLoopBackOff"),
            Some("CrashLoopBackOff")
        );
        assert_eq!(
            extract_reason("ImagePullBackOff: pull failed"),
            Some("ImagePullBackOff")
        );
        assert_eq!(extract_reason("ErrImagePull"), Some("ErrImagePull"));
        assert_eq!(extract_reason("OOMKilled"), Some("OOMKilled"));
    }

    #[test]
    fn extract_reason_returns_none_for_normal_log() {
        assert_eq!(extract_reason("normal log message"), None);
    }

    // ---------------- http_port_numbers ----------------

    #[test]
    fn http_port_numbers_filters_http_only() {
        // 2 HTTP + 1 TCP → 只返回 HTTP 端口
        let ports = Some(vec![
            PortConfig {
                name: "web".into(),
                port: 8080,
                expose_type: ExposeType::Http,
                strip_prefix: None,
            },
            PortConfig {
                name: "db".into(),
                port: 5432,
                expose_type: ExposeType::Tcp,
                strip_prefix: None,
            },
        ]);
        assert_eq!(http_port_numbers(&ports), vec![8080]);
    }

    #[test]
    fn http_port_numbers_none_returns_empty() {
        let result: Vec<u16> = http_port_numbers(&None);
        assert!(result.is_empty());
    }

    #[test]
    fn http_port_numbers_tcp_only_returns_empty() {
        let tcp_only = Some(vec![PortConfig {
            name: "db".into(),
            port: 5432,
            expose_type: ExposeType::Tcp,
            strip_prefix: None,
        }]);
        let result: Vec<u16> = http_port_numbers(&tcp_only);
        assert!(result.is_empty());
    }

    // ---------------- map_expose_type / map_health_check_type ----------------

    #[test]
    fn map_expose_type_maps_variants() {
        assert!(matches!(
            map_expose_type(&ExposeType::Http),
            RtExposeType::Http
        ));
        assert!(matches!(
            map_expose_type(&ExposeType::Tcp),
            RtExposeType::Tcp
        ));
    }

    #[test]
    fn map_health_check_type_maps_variants() {
        assert!(matches!(
            map_health_check_type(&HealthCheckType::Http),
            RtHealthCheckType::Http
        ));
        assert!(matches!(
            map_health_check_type(&HealthCheckType::Tcp),
            RtHealthCheckType::Tcp
        ));
        assert!(matches!(
            map_health_check_type(&HealthCheckType::Exec),
            RtHealthCheckType::Exec
        ));
        assert!(matches!(
            map_health_check_type(&HealthCheckType::None),
            RtHealthCheckType::None
        ));
    }
}
