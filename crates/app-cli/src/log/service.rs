use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::DateTime;
use globset::Glob;
use workspace_manifest::{LockedService, LogFormat, LogSource, ReleaseLock};

use super::model::{
    CursorState, LogQueryRequest, LogQueryResponse, LogRecord, LogSourceInfo, MAX_CURSOR_BYTES,
    MAX_KEYWORD_BYTES, MAX_SERVICES, MAX_SOURCES, MAX_TAIL_PER_SOURCE, SourceCursor, SourceError,
};

const MAX_FILES_PER_SOURCE: usize = 128;
const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct LogService {
    release: ReleaseLock,
    log_root: PathBuf,
    boot_id: String,
}

#[derive(Clone)]
struct SelectedSource {
    service_id: String,
    source: LogSource,
}

impl LogService {
    pub fn new(release: ReleaseLock, log_root: PathBuf) -> Self {
        Self {
            release,
            log_root,
            boot_id: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    pub fn sources(&self, request: &LogQueryRequest) -> Result<Vec<LogSourceInfo>> {
        self.select(request)?
            .into_iter()
            .map(|selected| {
                let files = self.match_files(&selected)?;
                Ok(LogSourceInfo {
                    service_id: selected.service_id,
                    source_id: selected.source.id,
                    format: match selected.source.format {
                        LogFormat::Jsonl => "jsonl",
                        LogFormat::Text => "text",
                    }
                    .into(),
                    matched_files: files
                        .iter()
                        .filter_map(|path| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().to_string())
                        })
                        .collect(),
                })
            })
            .collect()
    }

    pub fn query(&self, request: &LogQueryRequest) -> Result<LogQueryResponse> {
        self.validate_request(request)?;
        let selected = self.select(request)?;
        let mut cursor = self.decode_cursor(request.cursor.as_deref())?;
        let mut logs = Vec::new();
        let mut source_errors = Vec::new();
        for source in selected {
            match self.read_source(&source, request, &mut cursor) {
                Ok(mut records) => logs.append(&mut records),
                Err(error) => source_errors.push(SourceError {
                    service_id: source.service_id,
                    source_id: source.source.id,
                    code: "source_read_failed".into(),
                    message: error.to_string(),
                }),
            }
        }
        logs.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then_with(|| left.service_id.cmp(&right.service_id))
                .then_with(|| left.source_id.cmp(&right.source_id))
                .then_with(|| left.file.cmp(&right.file))
                .then_with(|| left.offset.cmp(&right.offset))
        });
        Ok(LogQueryResponse {
            logs,
            source_errors,
            cursor: self.encode_cursor(&cursor)?,
        })
    }

    fn validate_request(&self, request: &LogQueryRequest) -> Result<()> {
        if request.selectors.len() > MAX_SERVICES {
            anyhow::bail!("selectors exceeds maximum of {MAX_SERVICES} services");
        }
        if request.tail.unwrap_or(0) > MAX_TAIL_PER_SOURCE {
            anyhow::bail!("tail exceeds per-source maximum of {MAX_TAIL_PER_SOURCE}");
        }
        if request
            .keyword
            .as_deref()
            .is_some_and(|keyword| keyword.len() > MAX_KEYWORD_BYTES)
        {
            anyhow::bail!("keyword exceeds {MAX_KEYWORD_BYTES} bytes");
        }
        if request
            .cursor
            .as_deref()
            .is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES)
        {
            anyhow::bail!("cursor exceeds {MAX_CURSOR_BYTES} bytes");
        }
        for value in [request.since.as_deref(), request.until.as_deref()]
            .into_iter()
            .flatten()
        {
            DateTime::parse_from_rfc3339(value)
                .with_context(|| format!("invalid RFC3339 timestamp: {value}"))?;
        }
        Ok(())
    }

    fn select(&self, request: &LogQueryRequest) -> Result<Vec<SelectedSource>> {
        self.validate_request(request)?;
        let enabled: BTreeMap<&str, &LockedService> = self
            .release
            .services
            .iter()
            .filter(|service| service.enabled)
            .map(|service| (service.service_id.as_str(), service))
            .collect();
        let mut selected = Vec::new();
        let mut unique = BTreeSet::new();
        if request.selectors.is_empty() {
            for service in enabled.values() {
                for source in &service.logs {
                    unique.insert((service.service_id.clone(), source.id.clone()));
                    selected.push(SelectedSource {
                        service_id: service.service_id.clone(),
                        source: source.clone(),
                    });
                }
            }
        } else {
            for selector in &request.selectors {
                let service = enabled.get(selector.service_id.as_str()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown or disabled service selector: {}",
                        selector.service_id
                    )
                })?;
                let ids: Vec<&str> = if selector.source_ids.is_empty() {
                    service
                        .logs
                        .iter()
                        .map(|source| source.id.as_str())
                        .collect()
                } else {
                    selector.source_ids.iter().map(String::as_str).collect()
                };
                for id in ids {
                    let source = service
                        .logs
                        .iter()
                        .find(|source| source.id == id)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "unknown source selector: {}/{}",
                                selector.service_id,
                                id
                            )
                        })?;
                    if unique.insert((service.service_id.clone(), source.id.clone())) {
                        selected.push(SelectedSource {
                            service_id: service.service_id.clone(),
                            source: source.clone(),
                        });
                    }
                }
            }
        }
        if selected.len() > MAX_SOURCES {
            anyhow::bail!("selected sources exceeds maximum of {MAX_SOURCES}");
        }
        Ok(selected)
    }

    fn match_files(&self, selected: &SelectedSource) -> Result<Vec<PathBuf>> {
        let directory = self.log_root.join(&selected.service_id);
        let matcher = Glob::new(&selected.source.glob)
            .with_context(|| format!("invalid source glob {}", selected.source.glob))?
            .compile_matcher();
        let mut files = Vec::new();
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(files),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read log directory {}", directory.display()));
            }
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            if matcher.is_match(Path::new(&name)) {
                files.push(entry.path());
                if files.len() > MAX_FILES_PER_SOURCE {
                    anyhow::bail!(
                        "source {}/{} matches more than {MAX_FILES_PER_SOURCE} files",
                        selected.service_id,
                        selected.source.id
                    );
                }
            }
        }
        files.sort();
        Ok(files)
    }

    fn read_source(
        &self,
        selected: &SelectedSource,
        request: &LogQueryRequest,
        cursor: &mut CursorState,
    ) -> Result<Vec<LogRecord>> {
        let files = self.match_files(selected)?;
        if files.is_empty() {
            anyhow::bail!("no files match declared glob {}", selected.source.glob);
        }
        let key = format!("{}/{}", selected.service_id, selected.source.id);
        let prior = cursor.sources.get(&key).cloned();
        let mut records = Vec::new();
        for path in files {
            let file_name = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("log file has no name"))?
                .to_string_lossy()
                .to_string();
            let identity = file_identity(&path)?;
            if prior.as_ref().is_some_and(|state| file_name < state.file) {
                continue;
            }
            let start = prior
                .as_ref()
                .filter(|state| state.file == file_name && state.file_identity == identity)
                .map_or(0, |state| state.offset);
            let (mut file_records, end) =
                read_file(&path, start, selected, request).with_context(|| {
                    format!(
                        "read source {}/{} file {}",
                        selected.service_id, selected.source.id, file_name
                    )
                })?;
            records.append(&mut file_records);
            cursor.sources.insert(
                key.clone(),
                SourceCursor {
                    file: file_name,
                    file_identity: identity,
                    offset: end,
                },
            );
        }
        if request.cursor.is_none() {
            let tail = request.tail.unwrap_or(100);
            if records.len() > tail {
                records.drain(..records.len() - tail);
            }
        }
        Ok(records)
    }

    fn decode_cursor(&self, encoded: Option<&str>) -> Result<CursorState> {
        let Some(encoded) = encoded else {
            return Ok(CursorState {
                boot_id: self.boot_id.clone(),
                sources: BTreeMap::new(),
            });
        };
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .context("decode cursor")?;
        let mut cursor: CursorState = serde_json::from_slice(&bytes).context("parse cursor")?;
        if cursor.boot_id != self.boot_id {
            cursor = CursorState {
                boot_id: self.boot_id.clone(),
                sources: BTreeMap::new(),
            };
        }
        Ok(cursor)
    }

    fn encode_cursor(&self, cursor: &CursorState) -> Result<String> {
        let bytes = serde_json::to_vec(cursor).context("serialize cursor")?;
        if bytes.len() > MAX_CURSOR_BYTES {
            anyhow::bail!("generated cursor exceeds {MAX_CURSOR_BYTES} bytes");
        }
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }
}

fn read_file(
    path: &Path,
    start: u64,
    selected: &SelectedSource,
    request: &LogQueryRequest,
) -> Result<(Vec<LogRecord>, u64)> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let safe_start = if start > length { 0 } else { start };
    file.seek(std::io::SeekFrom::Start(safe_start))?;
    let mut reader = std::io::BufReader::new(file);
    let mut offset = safe_start;
    let mut records = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            anyhow::bail!("log line exceeds {MAX_LINE_BYTES} bytes");
        }
        let current_offset = offset;
        offset += u64::try_from(read).context("line size conversion")?;
        let message = String::from_utf8_lossy(&line)
            .trim_end_matches(['\r', '\n'])
            .to_string();
        let (timestamp, level, rendered) = parse_line(&message, &selected.source.format);
        if !matches_filters(request, timestamp.as_deref(), level.as_deref(), &rendered) {
            continue;
        }
        records.push(LogRecord {
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
        });
    }
    if selected.source.format == LogFormat::Text
        && let Some(pattern) = &selected.source.multiline_start_pattern
    {
        records = merge_multiline(records, pattern)?;
    }
    Ok((records, offset))
}

fn merge_multiline(records: Vec<LogRecord>, pattern: &str) -> Result<Vec<LogRecord>> {
    let start = regex::Regex::new(pattern).context("compile multiline_start_pattern")?;
    let mut merged: Vec<LogRecord> = Vec::new();
    for record in records {
        if start.is_match(&record.message) || merged.is_empty() {
            merged.push(record);
        } else if let Some(previous) = merged.last_mut() {
            previous.message.push('\n');
            previous.message.push_str(&record.message);
        }
    }
    Ok(merged)
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

fn matches_filters(
    request: &LogQueryRequest,
    timestamp: Option<&str>,
    level: Option<&str>,
    message: &str,
) -> bool {
    if let Some(keyword) = &request.keyword
        && !message.contains(keyword)
    {
        return false;
    }
    if !request.levels.is_empty()
        && !level.is_some_and(|value| {
            request
                .levels
                .iter()
                .any(|expected| expected.eq_ignore_ascii_case(value))
        })
    {
        return false;
    }
    if let (Some(since), Some(timestamp)) = (request.since.as_deref(), timestamp)
        && timestamp < since
    {
        return false;
    }
    if let (Some(until), Some(timestamp)) = (request.until.as_deref(), timestamp)
        && timestamp > until
    {
        return false;
    }
    true
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Result<String> {
    let metadata = std::fs::metadata(path)?;
    let modified = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .context("file modified time before epoch")?;
    Ok(format!("{}:{}", metadata.len(), modified.as_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(offset: u64, message: &str) -> LogRecord {
        LogRecord {
            service_id: "api".into(),
            source_id: "application".into(),
            file: "application.log".into(),
            offset,
            timestamp: None,
            level: None,
            message: message.into(),
        }
    }

    #[test]
    fn multiline_text_combines_stack_trace_lines() {
        let records = vec![
            record(0, "2026-07-29 ERROR failed"),
            record(24, "  at example::main"),
            record(45, "2026-07-29 INFO recovered"),
        ];
        let merged =
            merge_multiline(records, r"^\d{4}-\d{2}-\d{2}").expect("valid multiline pattern");
        assert_eq!(merged.len(), 2);
        assert!(merged[0].message.contains("at example::main"));
        assert_eq!(merged[1].offset, 45);
    }

    #[test]
    fn invalid_multiline_pattern_fails_fast() {
        assert!(merge_multiline(vec![record(0, "line")], "[").is_err());
    }
}
