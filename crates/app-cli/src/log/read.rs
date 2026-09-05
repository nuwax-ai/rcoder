//! 日志文件分页读取引擎：多行合并、行截断与 cursor 回退（从 service.rs 拆出）。

use std::collections::VecDeque;
use std::io::{BufRead, Read, Seek};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use workspace_manifest::LogFormat;

use super::filter::push_record;
use super::model::{LogQueryRequest, LogRecord};
use super::sources::SelectedSource;

pub(super) const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 超长行截断后保留进记录的前缀字节数（时间戳/级别/消息开头足以定位）。
const TRUNCATED_PREFIX_BYTES: usize = 4096;

pub(super) struct ReadOutcome {
    pub(super) records: Vec<LogRecord>,
    pub(super) offset: u64,
    pub(super) complete: bool,
}

pub(super) fn read_file(
    path: &Path,
    start: u64,
    selected: &SelectedSource,
    request: &LogQueryRequest,
    tail_limit: Option<usize>,
    record_limit: Option<usize>,
    cancelled: &AtomicBool,
) -> Result<ReadOutcome> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let safe_start = if start > length { 0 } else { start };
    file.seek(std::io::SeekFrom::Start(safe_start))?;
    let mut reader = std::io::BufReader::new(file);
    let mut offset = safe_start;
    let mut records = VecDeque::new();
    let multiline_start = if selected.source.format == LogFormat::Text {
        selected
            .source
            .multiline_start_pattern
            .as_deref()
            .map(regex::Regex::new)
            .transpose()
            .context("compile multiline_start_pattern")?
    } else {
        None
    };
    let mut pending_multiline: Option<LogRecord> = None;
    let mut complete = true;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("log query cancelled");
        }
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        let current_offset = offset;
        let mut consumed = u64::try_from(read).context("line size conversion")?;
        let message = if line.len() > MAX_LINE_BYTES {
            // 超长行（大堆栈/大 JSON dump）不毒化整个源：吞掉本行剩余字节、
            // 产出一条截断记录并推进 offset。旧行为是 bail!——该源 cursor
            // 不前进，每次查询都重读撞上同一行，恒定报错直到该行被轮转走。
            // 注：行长恰为 MAX+1 且以 \n 结尾时行已完整读入，无需再 drain
            // （否则会把下一行误吞进截断记录）。
            if !line.ends_with(b"\n") {
                loop {
                    let mut chunk = Vec::new();
                    let n = reader
                        .by_ref()
                        .take((MAX_LINE_BYTES + 1) as u64)
                        .read_until(b'\n', &mut chunk)?;
                    if n == 0 {
                        break;
                    }
                    consumed += u64::try_from(n).context("line size conversion")?;
                    if chunk.ends_with(b"\n") {
                        break;
                    }
                }
            }
            let prefix = &line[..TRUNCATED_PREFIX_BYTES.min(line.len())];
            let prefix = String::from_utf8_lossy(prefix);
            let prefix = prefix.trim_end_matches(['\r', '\n']);
            format!("{prefix}… [app-cli truncated: original line was {consumed} bytes]")
        } else {
            String::from_utf8_lossy(&line)
                .trim_end_matches(['\r', '\n'])
                .to_string()
        };
        offset += consumed;
        let (timestamp, level, rendered) = parse_line(&message, &selected.source.format);
        let record = LogRecord {
            service_id: selected.service_id.clone(),
            source_id: selected.source.id.clone(),
            file: path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            offset: current_offset,
            timestamp,
            level,
            message: rendered,
        };
        if let Some(start) = &multiline_start {
            if start.is_match(&record.message) || pending_multiline.is_none() {
                if let Some(previous) = pending_multiline.take()
                    && push_record(&mut records, previous, request, tail_limit, record_limit)
                {
                    // 当前行已读出但属于下一条逻辑记录；cursor 回到该行开头，
                    // 下一页重新读取，避免达到分页上限时丢日志。
                    offset = current_offset;
                    complete = false;
                    break;
                }
                pending_multiline = Some(record);
            } else if let Some(previous) = pending_multiline.as_mut() {
                previous.message.push('\n');
                previous.message.push_str(&record.message);
            }
        } else if push_record(&mut records, record, request, tail_limit, record_limit) {
            complete = false;
            break;
        }
    }
    if complete && let Some(record) = pending_multiline {
        let _ = push_record(&mut records, record, request, tail_limit, record_limit);
    }
    Ok(ReadOutcome {
        records: records.into(),
        offset,
        complete,
    })
}

fn parse_line(line: &str, format: &LogFormat) -> (Option<String>, Option<String>, String) {
    if format == &LogFormat::Jsonl
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
    {
        let timestamp = value
            .get("timestamp")
            .or_else(|| value.get("time"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let level = value
            .get("level")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let message = value
            .get("message")
            .or_else(|| value.get("msg"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(line)
            .to_owned();
        return (timestamp, level, message);
    }
    (None, None, line.to_owned())
}
