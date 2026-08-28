//! 日志源解析：runtime 源注入、目录布局与源类型（从 service.rs 拆出）。

use std::path::{Path, PathBuf};

use workspace_manifest::{LogFormat, LogSource, ReleaseLock};

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
