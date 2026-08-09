//! 日志读取：列举、tail N 行、SSE 实时流。
//!
//! - `list_log_files`：扫描 log_dir 下所有 *.out.log + *.err.log + 轮转文件
//! - `read_last_n_lines`：从文件尾 backwards seek 读最后 N 行（O(N) 不读全文件）
//! - `tail_from`：从 offset 读到文件尾 → 继续 tail 新行（SSE stream 用）

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// 日志文件信息（列举用）。
#[derive(serde::Serialize)]
pub struct LogFileInfo {
    pub name: String,
    pub size: u64,
}

/// 子项目日志信息（out + err）。
#[derive(serde::Serialize)]
pub struct ProjectLogs {
    pub dir: String,
    pub out: Vec<LogFileInfo>,
    pub err: Vec<LogFileInfo>,
}

/// 扫描 log_dir，返回所有子项目的日志文件列表。
pub fn list_log_files(log_dir: &Path) -> Vec<ProjectLogs> {
    let mut result: std::collections::BTreeMap<String, ProjectLogs> = Default::default();
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return vec![];
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // 文件名格式：<dir>.out.log / <dir>.err.log / <dir>.out.1.log 等
        let (dir, kind) = if let Some(dir) = name
            .strip_suffix(".out.log")
            .or_else(|| name.strip_suffix(".err.log"))
            .or_else(|| {
                // 轮转文件：<dir>.out.1.log → strip → <dir>.out
                name.strip_suffix(".log").and_then(|n| {
                    n.strip_suffix(".out.1")
                        .or_else(|| n.strip_suffix(".out.2"))
                        .or_else(|| n.strip_suffix(".out.3"))
                        .or_else(|| n.strip_suffix(".err.1"))
                        .or_else(|| n.strip_suffix(".err.2"))
                        .or_else(|| n.strip_suffix(".err.3"))
                })
            }) {
            if name.contains(".err") {
                (dir.to_string(), "err")
            } else {
                (dir.to_string(), "out")
            }
        } else {
            continue; // 非 <dir>.out.log / <dir>.err.log
        };

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let info = LogFileInfo { name, size };
        let entry = result.entry(dir.clone()).or_insert_with(|| ProjectLogs {
            dir: dir.clone(),
            out: vec![],
            err: vec![],
        });
        if kind == "err" {
            entry.err.push(info);
        } else {
            entry.out.push(info);
        }
    }
    // 排序文件名（.log 在 .1.log 前面）
    let mut v: Vec<_> = result.into_values().collect();
    for p in &mut v {
        p.out.sort_by(|a, b| a.name.cmp(&b.name));
        p.err.sort_by(|a, b| a.name.cmp(&b.name));
    }
    v.sort_by(|a, b| a.dir.cmp(&b.dir));
    v
}

/// 从大文件末尾读取最后 N 行。O(N) 复杂度（不读整个文件）。
///
/// 算法（GNU tail 同理）：seek 到文件尾，按 8KB 块 backwards 读，数换行符。
/// 返回 `(lines, total_bytes, has_more)`：`has_more` 表示文件还有更早的行未读到，
/// 前端据此判断 `truncated`（请求 N 行但文件实际更多）。
pub fn read_last_n_lines(path: &Path, n: usize) -> std::io::Result<(Vec<String>, u64, bool)> {
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    if total == 0 {
        return Ok((vec![], 0, false));
    }

    const CHUNK: u64 = 8192;
    let mut pos = total;
    let mut buf: Vec<u8> = Vec::new(); // 反向收集的数据
    let mut nl_count = 0usize;

    while pos > 0 && nl_count <= n {
        let read_size = CHUNK.min(pos);
        pos -= read_size;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read_size as usize];
        file.read_exact(&mut chunk)?;
        nl_count += chunk.iter().filter(|&&b| b == b'\n').count();
        buf.extend_from_slice(&chunk);
    }

    // has_more：退出循环时 pos>0（文件还有更早内容未读）或 nl_count>n（已读范围内行数已超 N），
    // 都意味着 take(n) 必然丢弃了部分行 → 截断。
    let has_more = pos > 0 || nl_count > n;

    // 反转 → 按换行分割 → 取最后 N 行 → 再反转回正序
    buf.reverse();
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().rev().take(n).map(|s| s.to_string()).collect();
    lines.reverse();
    Ok((lines, total, has_more))
}

/// 从 `start_offset` 读文件到尾（返回行 + 每行的字节偏移）。
/// 用于 SSE stream 的「补漏」阶段：断线期间漏掉的日志，从 last-event-id 开始读到文件尾。
pub fn read_from_offset(
    path: &Path,
    start_offset: u64,
) -> std::io::Result<(Vec<(String, u64)>, u64)> {
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    if start_offset >= total {
        return Ok((vec![], total));
    }
    file.seek(SeekFrom::Start(start_offset))?;
    let mut buf = String::new();
    let read = file.read_to_string(&mut buf)?;
    let mut result = Vec::new();
    let mut offset = start_offset;
    for line in buf.lines() {
        let line_len = line.len() as u64 + 1; // +1 for \n
        result.push((line.to_string(), offset));
        offset += line_len;
    }
    // 最后一行可能没有 \n（文件尾），不修正 offset（不影响）
    let _ = read; // silence unused
    Ok((result, total))
}

/// 获取文件当前大小（不打开文件）。
pub fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
