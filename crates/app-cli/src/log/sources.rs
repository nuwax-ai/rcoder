//! 日志源解析：runtime 源注入、编排器内置源注入、目录布局与源类型（从 service.rs 拆出）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use workspace_manifest::{
    HealthSection, LockedService, LogFormat, LogSource, ProjectKind, ProjectType, ReleaseLock,
    RunSection,
};

/// 编排器内置日志源的保留服务名（logs/query `service_id="app-cli"`）。
pub(super) const ORCHESTRATOR_SERVICE_ID: &str = "app-cli";

const ORCHESTRATOR_SOURCE_ID: &str = "orchestrator";

/// app-cli tracing 文件层（init_tracing daily 轮转）的文件名形态，
/// 直接落在 log_root 根目录（与 LogService 同一 log_dir）。
const ORCHESTRATOR_GLOB: &str = "app-cli.log.*";

/// runtime 日志源为平台注入：supervisor 会为每个服务落盘 runtime.out.log /
/// runtime.err.log（轮转命名 runtime.out.N.log），即使 manifest 未声明也应可查。
/// 纯内存变换，不写回 release.lock；用户已声明同 id source 时以用户声明为准，不覆盖。
pub(super) fn inject_runtime_log_sources(release: &mut ReleaseLock, layout: LogLayout) {
    for service in &mut release.services {
        if service.logs.iter().any(|source| source.id == "runtime") {
            continue;
        }
        let glob = match layout {
            LogLayout::Builtin => "runtime.*.log".to_string(),
            // supervisord 单目录合流文件：{svc}.log（glob 相对 services/ 目录）
            LogLayout::Supervisord => format!("{}.log", service.service_id),
        };
        service.logs.push(LogSource {
            id: "runtime".into(),
            glob,
            format: LogFormat::Text,
            multiline_start_pattern: None,
        });
    }
}

/// 编排器内置日志源注入：把 app-cli 自身日志（log_root 根目录的 app-cli.log.<date>，
/// JSON 行格式）以虚拟服务 `app-cli` + 源 `orchestrator` 挂进查询面——logs/query|stream
/// 从此覆盖启停过程，无需独立的 dev server 进程日志接口。纯内存变换，不写回
/// release.lock；用户已声明同名服务时以用户为准，不注入。
pub(super) fn inject_orchestrator_log_source(release: &mut ReleaseLock) {
    if release
        .services
        .iter()
        .any(|service| service.service_id == ORCHESTRATOR_SERVICE_ID)
    {
        return;
    }
    release.services.push(orchestrator_service());
}

/// 合成的编排器虚拟服务：仅存在于日志查询面（enabled 服务集），不参与启停、
/// 状态与 proxy 路由。字段除 service/logs 外无业务语义，取类型默认值即可。
pub(super) fn orchestrator_service() -> LockedService {
    LockedService {
        service_id: ORCHESTRATOR_SERVICE_ID.into(),
        name: "app-cli orchestrator".into(),
        dir: ".".into(),
        r#type: ProjectType::Rust,
        kind: ProjectKind::Worker,
        enabled: true,
        port: 0,
        devbuild: None,
        run: RunSection {
            command: Vec::new(),
            migrate: Vec::new(),
            depends_on: Vec::new(),
            shutdown_timeout_seconds: 30,
        },
        devrun: None,
        health: HealthSection::default(),
        proxy: None,
        logs: vec![LogSource {
            id: ORCHESTRATOR_SOURCE_ID.into(),
            glob: ORCHESTRATOR_GLOB.into(),
            format: LogFormat::Jsonl,
            multiline_start_pattern: None,
        }],
        env: BTreeMap::new(),
        static_content_dir: None,
    }
}

/// runtime 日志源（服务 stdout/stderr 落盘）的目录布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLayout {
    /// builtin 引擎：app-cli 亲自 pipe 服务 stdout → `{log_root}/{svc}/runtime.*.log`。
    #[default]
    Builtin,
    /// supervisord 引擎：program stdout 由 supervisord 落盘
    /// `{log_root}/services/{svc}.log`（redirect_stderr 合流，轮转由 supervisord 管）。
    /// 用户声明源（应用自写文件，APP_LOG_DIR={log_root}/{svc}）两布局目录一致。
    Supervisord,
}

#[derive(Clone)]
pub(super) struct SelectedSource {
    pub(super) service_id: String,
    pub(super) source: LogSource,
}

pub(super) struct MatchedLogFile {
    pub(super) path: PathBuf,
    pub(super) identity: String,
    pub(super) len: u64,
    pub(super) modified: std::time::SystemTime,
}

#[cfg(unix)]
pub(super) fn file_identity(_path: &Path, metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
pub(super) fn file_identity(path: &Path, _metadata: &std::fs::Metadata) -> String {
    // Rust 标准库在所有非 Unix 平台上没有统一稳定的 file-id API。日志目录和
    // 文件名已经过边界校验，以路径作为稳定 identity 可避免每次 append 都重置 cursor。
    path.to_string_lossy().into_owned()
}
