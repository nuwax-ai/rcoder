//! Dev server 日志 (对齐 nuwax `logUtils.js` + `getDevLogUtils.js`)。
//!
//! - 双流: 主日志 `dev-{YYYY-MM-DD}.log` (按日期轮转) + 临时日志 `dev-temp-{ms}.log`
//! - 每行 prepend 时间戳前缀 `[YYYY/MM/DD HH:MM:SS] `
//! - log dir: 普通 projectId → `{LOG_BASE_DIR}/{projectId}/`; `computer:userId:cId` → `{COMPUTER_LOG_DIR}/{userId}/{cId}/`

use std::path::{Path, PathBuf};

use chrono::Local;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{AppError, AppResult};

/// 解析 dev 日志目录 (对齐 nuwax getLogDir)。
pub fn log_dir(cfg: &crate::Config, project_id: &str) -> PathBuf {
    if let Some(rest) = project_id.strip_prefix("computer:") {
        // computer:{userId}:{cId}
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        let (user, cid) = match parts.as_slice() {
            [u, c] => (*u, *c),
            [u] => (*u, ""),
            _ => ("", ""),
        };
        cfg.computer_log_dir.join(user).join(cid)
    } else {
        cfg.log_base_dir.join(project_id)
    }
}

/// 当日主日志文件名 `dev-YYYY-MM-DD.log`。
pub fn main_log_name() -> String {
    format!("dev-{}.log", Local::now().format("%Y-%m-%d"))
}

/// 临时日志文件名 `dev-temp-{ms}.log`。
pub fn temp_log_name(now_ms: i64) -> String {
    format!("dev-temp-{now_ms}.log")
}

/// 时间戳前缀 `[YYYY/MM/DD HH:MM:SS] `。
pub fn timestamp_prefix() -> String {
    format!("[{}] ", Local::now().format("%Y/%m/%d %H:%M:%S"))
}

/// 异步把一行 (已含前缀) append 到指定文件。
pub async fn append_line(path: &Path, line: &str) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| AppError::system(format!("open dev log {}: {e}", path.display())))?;
    file.write_all(line.as_bytes())
        .await
        .map_err(|e| AppError::system(format!("write dev log: {e}")))?;
    file.write_all(b"\n")
        .await
        .map_err(|e| AppError::system(format!("write dev log nl: {e}")))?;
    Ok(())
}

/// 把 child 的一个 stdout/stderr 流管道到主+临时日志, 每行加时间戳前缀。
/// 返回的 JoinHandle 供调用方跟踪 (通常 fire-and-forget)。
pub fn spawn_log_pipe<R>(reader: R, main_path: PathBuf, temp_path: PathBuf)
-> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let prefixed = format!("{}{}", timestamp_prefix(), line);
            // 主+临时都写; 单流失败不阻塞另一流
            let _ = append_line(&main_path, &prefixed).await;
            let _ = append_line(&temp_path, &prefixed).await;
        }
    })
}

/// 一行日志 (对齐 nuwax getDevLog 响应)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogLine {
    pub line: usize,
    pub content: String,
}

/// 读 dev 日志 (对齐 nuwax getDevLog)。
/// - log_type="main": 当日 `dev-YYYY-MM-DD.log`
/// - log_type="temp" (默认): 最新 `dev-temp-*.log` (按文件名时间戳降序)
pub async fn read_dev_log(
    dir: &Path,
    start_index: usize,
    log_type: &str,
) -> AppResult<ReadDevLogResult> {
    let file_name = if log_type == "main" {
        main_log_name()
    } else {
        latest_temp_log(dir).await.unwrap_or_else(main_log_name)
    };
    let path = dir.join(&file_name);
    if !path.exists() {
        return Ok(ReadDevLogResult {
            logs: vec![],
            total_lines: 0,
            start_index,
            log_file_name: file_name,
        });
    }
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AppError::system(format!("read dev log: {e}")))?;
    let all: Vec<&str> = content.lines().collect();
    let total = all.len();
    let start = start_index.saturating_sub(1).min(total);
    let logs = all[start..]
        .iter()
        .enumerate()
        .map(|(i, l)| LogLine {
            line: start + i + 1,
            content: sanitize_sensitive_paths(l),
        })
        .collect();
    Ok(ReadDevLogResult {
        logs,
        total_lines: total,
        start_index: start + 1,
        log_file_name: file_name,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadDevLogResult {
    pub logs: Vec<LogLine>,
    pub total_lines: usize,
    pub start_index: usize,
    pub log_file_name: String,
}

/// 目录下最新的 `dev-temp-*.log` (按文件名内时间戳降序)。
async fn latest_temp_log(dir: &Path) -> Option<String> {
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    let mut temps: Vec<(i64, String)> = Vec::new();
    while let Ok(Some(e)) = entries.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(ms) = name
            .strip_prefix("dev-temp-")
            .and_then(|s| s.strip_suffix(".log"))
            .and_then(|s| s.parse::<i64>().ok())
        {
            temps.push((ms, name));
        }
    }
    temps.sort_by_key(|&(ms, _)| std::cmp::Reverse(ms));
    temps.into_iter().next().map(|(_, n)| n)
}

/// 脱敏敏感路径 (对齐 nuwax sanitizeSensitivePaths): 把绝对工作区路径替换为相对。
pub fn sanitize_sensitive_paths(s: &str) -> String {
    // 简化: 去掉常见的 /app/project_workspace 与 /app/computer-project-workspace 前缀
    s.replace("/app/project_workspace/", "")
        .replace("/app/computer-project-workspace/", "")
}

/// 清理目录下所有 `dev-temp-*.log` (stop 时调用)。
pub async fn cleanup_temp_logs(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(e)) = entries.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("dev-temp-") && name.ends_with(".log") {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}
