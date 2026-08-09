//! Dev server 启动辅助：日志路径、错误分类、manifest 读取与锁恢复。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{StderrRing, ViteStartupError, error_classify, log};
use crate::error::{AppError, AppResult};

pub(super) fn ldrtemp(log_dir: &Path, now: i64) -> PathBuf {
    log_dir.join(log::temp_log_name(now))
}

pub(super) fn early_exit_err(pid: u32, port: u16, ring: &Arc<StderrRing>) -> AppError {
    let lines = error_classify::ring_collect(ring);
    ViteStartupError::classify(&lines).into_app_error(pid, port)
}

pub(super) fn read_dev_script(project_path: &Path) -> AppResult<String> {
    let package_path = project_path.join("package.json");
    let content = std::fs::read_to_string(&package_path)
        .map_err(|error| AppError::business(format!("read package.json failed: {error}")))?;
    let package: serde_json::Value = serde_json::from_str(&content)
        .map_err(|error| AppError::business(format!("parse package.json failed: {error}")))?;
    package
        .get("scripts")
        .and_then(|scripts| scripts.get("dev"))
        .and_then(|script| script.as_str())
        .map(str::to_string)
        .ok_or_else(|| AppError::business("package.json has no scripts.dev"))
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> AppResult<std::sync::MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|error| AppError::system(format!("mutex poisoned: {error}")))
}
