use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, FixedOffset, Utc};
use globset::Glob;
use workspace_manifest::{LockedService, LogFormat, LogSource, ReleaseLock};

use super::model::{
    CursorState, FileCursor, LogQueryRequest, LogQueryResponse, LogRecord, LogSourceInfo,
    MAX_CURSOR_BYTES, MAX_KEYWORD_BYTES, MAX_SERVICES, MAX_SOURCES, MAX_TAIL_PER_SOURCE,
    SourceCursor, SourceError,
};

const MAX_FILES_PER_SOURCE: usize = 128;
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// runtime 日志源为平台注入：supervisor 会为每个服务落盘 runtime.out.log /
/// runtime.err.log（轮转命名 runtime.out.N.log），即使 manifest 未声明也应可查。
/// 纯内存变换，不写回 release.lock；用户已声明同 id source 时以用户声明为准，不覆盖。
fn inject_runtime_log_sources(release: &mut ReleaseLock) {
    for service in &mut release.services {
        if !service.logs.iter().any(|source| source.id == "runtime") {
            service.logs.push(LogSource {
                id: "runtime".into(),
                glob: "runtime.*.log".into(),
                format: LogFormat::Text,
                multiline_start_pattern: None,
            });
        }
    }
}

#[derive(Clone)]
pub struct LogService {
    /// enabled 服务集（已注入 runtime 日志源）。server 动态形态下每次查询按当前
    /// release 构造；空集 = 未部署（idle）——查询返回空、游标按代际失效。
    services: Vec<LockedService>,
    log_root: PathBuf,
    boot_id: String,
}

#[derive(Clone)]
struct SelectedSource {
    service_id: String,
    source: LogSource,
}

struct MatchedLogFile {
    path: PathBuf,
    identity: String,
    len: u64,
    modified: std::time::SystemTime,
}

impl LogService {
    pub fn new(mut release: ReleaseLock, log_root: PathBuf) -> Self {
        inject_runtime_log_sources(&mut release);
        Self {
            services: release
                .services
                .into_iter()
                .filter(|service| service.enabled)
                .collect(),
            log_root,
            boot_id: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    /// 未部署（idle）形态：空服务集，boot_id 固定 "idle"（无代际可言）。
    pub fn idle(log_root: PathBuf) -> Self {
        Self {
            services: Vec::new(),
            log_root,
            boot_id: "idle".to_string(),
        }
    }

    /// server 动态形态：按当前 release + 部署代（=release_id）构造——换代后
    /// 旧 cursor 的 boot_id 不匹配 → cursor_reset 重放（语义与进程代际一致）。
    pub fn with_boot_id(mut release: ReleaseLock, log_root: PathBuf, boot_id: String) -> Self {
        inject_runtime_log_sources(&mut release);
        Self {
            services: release
                .services
                .into_iter()
                .filter(|service| service.enabled)
                .collect(),
            log_root,
            boot_id,
        }
    }

    pub async fn sources(&self, request: LogQueryRequest) -> Result<Vec<LogSourceInfo>> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || service.sources_blocking(&request))
            .await
            .context("join log source query")?
    }

    fn sources_blocking(&self, request: &LogQueryRequest) -> Result<Vec<LogSourceInfo>> {
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
                        .filter_map(|file| {
                            file.path
                                .file_name()
                                .map(|name| name.to_string_lossy().to_string())
                        })
                        .collect(),
                })
            })
            .collect()
    }

    pub async fn query(&self, request: LogQueryRequest) -> Result<LogQueryResponse> {
        self.query_with_cancel(request, Arc::new(AtomicBool::new(false)))
            .await
    }

    pub async fn query_with_cancel(
        &self,
        request: LogQueryRequest,
        cancelled: Arc<AtomicBool>,
    ) -> Result<LogQueryResponse> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || service.query_blocking(&request, &cancelled))
            .await
            .context("join log query")?
    }

    fn query_blocking(
        &self,
        request: &LogQueryRequest,
        cancelled: &AtomicBool,
    ) -> Result<LogQueryResponse> {
        self.validate_request(request)?;
        let selected = self.select(request)?;
        let (mut cursor, cursor_reset) = self.decode_cursor(request.cursor.as_deref())?;
        let mut logs = Vec::new();
        let mut source_errors = Vec::new();
        for source in selected {
            if cancelled.load(Ordering::Relaxed) {
                anyhow::bail!("log query cancelled");
            }
            match self.read_source(&source, request, &mut cursor, cancelled) {
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
            compare_timestamps(left.timestamp.as_deref(), right.timestamp.as_deref())
                .then_with(|| left.service_id.cmp(&right.service_id))
                .then_with(|| left.source_id.cmp(&right.source_id))
                .then_with(|| left.file.cmp(&right.file))
                .then_with(|| left.offset.cmp(&right.offset))
        });
        Ok(LogQueryResponse {
            logs,
            source_errors,
            cursor: self.encode_cursor(&cursor)?,
            cursor_reset,
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
            .services
            .iter()
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
        let selected_service_count = selected
            .iter()
            .map(|source| source.service_id.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        if selected_service_count > MAX_SERVICES {
            anyhow::bail!("selected services exceeds maximum of {MAX_SERVICES}");
        }
        if selected.len() > MAX_SOURCES {
            anyhow::bail!("selected sources exceeds maximum of {MAX_SOURCES}");
        }
        Ok(selected)
    }

    fn match_files(&self, selected: &SelectedSource) -> Result<Vec<MatchedLogFile>> {
        let directory = self.log_root.join(&selected.service_id);
        let matcher = Glob::new(&selected.source.glob)
            .with_context(|| format!("invalid source glob {}", selected.source.glob))?
            .compile_matcher();
        let mut files = Vec::new();
        let directory_metadata = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect log directory {}", directory.display()));
            }
        };
        if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
            anyhow::bail!(
                "service log directory must be a real directory: {}",
                directory.display()
            );
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
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
                let path = entry.path();
                let metadata = entry.metadata()?;
                let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
                files.push(MatchedLogFile {
                    identity: file_identity(&path, &metadata),
                    path,
                    len: metadata.len(),
                    modified,
                });
                if files.len() > MAX_FILES_PER_SOURCE {
                    anyhow::bail!(
                        "source {}/{} matches more than {MAX_FILES_PER_SOURCE} files",
                        selected.service_id,
                        selected.source.id
                    );
                }
            }
        }
        files.sort_by(|left, right| {
            left.modified
                .cmp(&right.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(files)
    }

    fn read_source(
        &self,
        selected: &SelectedSource,
        request: &LogQueryRequest,
        cursor: &mut CursorState,
        cancelled: &AtomicBool,
    ) -> Result<Vec<LogRecord>> {
        let files = self.match_files(selected)?;
        if files.is_empty() {
            anyhow::bail!("no files match declared glob {}", selected.source.glob);
        }
        let key = format!("{}/{}", selected.service_id, selected.source.id);
        let prior = cursor.sources.get(&key).cloned().unwrap_or(SourceCursor {
            files: BTreeMap::new(),
        });
        let mut next = prior.clone();
        let mut seen_identities = BTreeSet::new();
        let mut records = Vec::new();
        let mut exhausted_all_files = true;
        let initial_tail = request
            .cursor
            .is_none()
            .then(|| request.tail.unwrap_or(100));
        let mut remaining = initial_tail.is_none().then_some(MAX_TAIL_PER_SOURCE);
        for matched in files {
            let path = &matched.path;
            let file_name = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("log file has no name"))?
                .to_string_lossy()
                .to_string();
            let identity = matched.identity;
            seen_identities.insert(identity.clone());
            let start = prior.files.get(&identity).map_or(0, |state| state.offset);
            if request.cursor.is_some() && start == matched.len {
                next.files.insert(
                    identity,
                    FileCursor {
                        file: file_name,
                        offset: start,
                    },
                );
                continue;
            }
            let outcome = read_file(
                path,
                start,
                selected,
                request,
                initial_tail,
                remaining,
                cancelled,
            )
            .with_context(|| {
                format!(
                    "read source {}/{} file {}",
                    selected.service_id, selected.source.id, file_name
                )
            })?;
            next.files.insert(
                identity,
                FileCursor {
                    file: file_name,
                    offset: outcome.offset,
                },
            );
            if let Some(limit) = initial_tail {
                records.extend(outcome.records);
                if records.len() > limit {
                    records.drain(..records.len() - limit);
                }
            } else {
                let count = outcome.records.len();
                records.extend(outcome.records);
                remaining = remaining.map(|value| value.saturating_sub(count));
                if !outcome.complete || remaining == Some(0) {
                    exhausted_all_files = false;
                    break;
                }
            }
        }
        if exhausted_all_files {
            next.files
                .retain(|identity, _| seen_identities.contains(identity));
        }
        cursor.sources.insert(key, next);
        Ok(records)
    }

    fn decode_cursor(&self, encoded: Option<&str>) -> Result<(CursorState, bool)> {
        let Some(encoded) = encoded else {
            return Ok((self.empty_cursor(), false));
        };
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .context("decode cursor")?;
        let cursor: CursorState = serde_json::from_slice(&bytes).context("parse cursor")?;
        if cursor.boot_id != self.boot_id {
            return Ok((self.empty_cursor(), true));
        }
        Ok((cursor, false))
    }

    fn empty_cursor(&self) -> CursorState {
        CursorState {
            boot_id: self.boot_id.clone(),
            sources: BTreeMap::new(),
        }
    }

    fn encode_cursor(&self, cursor: &CursorState) -> Result<String> {
        let bytes = serde_json::to_vec(cursor).context("serialize cursor")?;
        if bytes.len() > MAX_CURSOR_BYTES {
            anyhow::bail!("generated cursor exceeds {MAX_CURSOR_BYTES} bytes");
        }
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }
}

struct ReadOutcome {
    records: Vec<LogRecord>,
    offset: u64,
    complete: bool,
}

fn read_file(
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
        if line.len() > MAX_LINE_BYTES {
            anyhow::bail!("log line exceeds {MAX_LINE_BYTES} bytes");
        }
        let current_offset = offset;
        offset += u64::try_from(read).context("line size conversion")?;
        let message = String::from_utf8_lossy(&line)
            .trim_end_matches(['\r', '\n'])
            .to_string();
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

fn push_record(
    records: &mut VecDeque<LogRecord>,
    record: LogRecord,
    request: &LogQueryRequest,
    tail_limit: Option<usize>,
    record_limit: Option<usize>,
) -> bool {
    if !matches_filters(
        request,
        record.timestamp.as_deref(),
        record.level.as_deref(),
        &record.message,
    ) {
        return false;
    }
    records.push_back(record);
    if let Some(limit) = tail_limit {
        if records.len() > limit {
            records.pop_front();
        }
        false
    } else {
        records.len() >= record_limit.unwrap_or(MAX_TAIL_PER_SOURCE)
    }
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
    if request.since.is_some() || request.until.is_some() {
        let Some(timestamp) = timestamp.and_then(parse_timestamp) else {
            return false;
        };
        if let Some(since) = request.since.as_deref().and_then(parse_timestamp)
            && timestamp < since
        {
            return false;
        }
        if let Some(until) = request.until.as_deref().and_then(parse_timestamp)
            && timestamp > until
        {
            return false;
        }
    }
    true
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::<FixedOffset>::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn compare_timestamps(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    match (
        left.and_then(parse_timestamp),
        right.and_then(parse_timestamp),
    ) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.cmp(&right),
    }
}

#[cfg(unix)]
fn file_identity(_path: &Path, metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(path: &Path, _metadata: &std::fs::Metadata) -> String {
    // Rust 标准库在所有非 Unix 平台上没有统一稳定的 file-id API。日志目录和
    // 文件名已经过边界校验，以路径作为稳定 identity 可避免每次 append 都重置 cursor。
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_lock() -> ReleaseLock {
        toml::from_str(
            r#"
schema_version = 1
release_id = "release-1"
workspace_name = "test"
minimum_app_cli_version = "0.1.0"
runtime_image_digest = "runtime:test"

[pingap]
mode = "managed"
version = "test"
commit = "test"

[[services]]
service_id = "api"
name = "API"
dir = "api"
type = "go"
kind = "web"
enabled = true
port = 18080

[services.run]
command = ["./api"]

[services.health]

[services.env]

[[services.logs]]
id = "application"
glob = "application*.log"
format = "jsonl"
"#,
        )
        .expect("valid release lock")
    }

    #[test]
    fn multiline_text_combines_stack_trace_lines() {
        let root = tempfile::tempdir().expect("log root");
        let path = root.path().join("application.log");
        std::fs::write(
            &path,
            "2026-07-29 ERROR failed\n  at example::main\n2026-07-29 INFO recovered\n",
        )
        .expect("write multiline log");
        let selected = SelectedSource {
            service_id: "api".into(),
            source: LogSource {
                id: "application".into(),
                glob: "application*.log".into(),
                format: LogFormat::Text,
                multiline_start_pattern: Some(r"^\d{4}-\d{2}-\d{2}".into()),
            },
        };
        let outcome = read_file(
            &path,
            0,
            &selected,
            &LogQueryRequest::default(),
            Some(2),
            None,
            &AtomicBool::new(false),
        )
        .expect("read multiline log");
        assert_eq!(outcome.records.len(), 2);
        assert!(outcome.records[0].message.contains("at example::main"));
        assert_eq!(outcome.records[1].message, "2026-07-29 INFO recovered");

        let tail = read_file(
            &path,
            0,
            &selected,
            &LogQueryRequest::default(),
            Some(1),
            None,
            &AtomicBool::new(false),
        )
        .expect("tail multiline log");
        assert_eq!(tail.records.len(), 1);
        assert_eq!(tail.records[0].message, "2026-07-29 INFO recovered");
    }

    #[test]
    fn invalid_multiline_pattern_fails_fast() {
        let root = tempfile::tempdir().expect("log root");
        let path = root.path().join("application.log");
        std::fs::write(&path, "line\n").expect("write log");
        let selected = SelectedSource {
            service_id: "api".into(),
            source: LogSource {
                id: "application".into(),
                glob: "application*.log".into(),
                format: LogFormat::Text,
                multiline_start_pattern: Some("[".into()),
            },
        };
        assert!(
            read_file(
                &path,
                0,
                &selected,
                &LogQueryRequest::default(),
                Some(100),
                None,
                &AtomicBool::new(false),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn source_error_does_not_commit_partial_cursor_progress() {
        let root = tempfile::tempdir().expect("log root");
        let directory = root.path().join("api");
        std::fs::create_dir_all(&directory).expect("service log directory");
        let first = directory.join("application-1.log");
        let second = directory.join("application-2.log");
        std::fs::write(&first, "").expect("first log");
        std::fs::write(&second, "").expect("second log");
        // 平台注入的 runtime 源也需有匹配文件，避免其 source error 干扰断言。
        std::fs::write(directory.join("runtime.out.log"), "").expect("runtime log");
        let service = LogService::new(release_lock(), root.path().to_path_buf());

        let initial = service
            .query(LogQueryRequest::default())
            .await
            .expect("initial cursor");
        std::fs::write(
            &first,
            "{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"first\"}\n",
        )
        .expect("append first");
        std::fs::write(&second, vec![b'x'; MAX_LINE_BYTES + 1]).expect("oversized second");
        let request = LogQueryRequest {
            cursor: Some(initial.cursor.clone()),
            ..Default::default()
        };
        let failed = service
            .query(request.clone())
            .await
            .expect("source error response");
        assert!(failed.logs.is_empty());
        assert_eq!(failed.source_errors.len(), 1);
        assert_eq!(failed.cursor, initial.cursor);

        std::fs::write(
            &second,
            "{\"timestamp\":\"2026-08-03T00:00:01Z\",\"message\":\"second\"}\n",
        )
        .expect("repair second");
        let recovered = service.query(request).await.expect("recovered source");
        assert_eq!(recovered.logs.len(), 2);
        assert_eq!(recovered.logs[0].message, "first");
        assert_eq!(recovered.logs[1].message, "second");
    }

    #[tokio::test]
    async fn cursor_from_previous_boot_is_reported_as_reset() {
        let root = tempfile::tempdir().expect("log root");
        std::fs::create_dir_all(root.path().join("api")).expect("service log directory");
        std::fs::write(root.path().join("api/application.log"), "").expect("log file");
        let first = LogService::new(release_lock(), root.path().to_path_buf());
        let cursor = first
            .query(LogQueryRequest::default())
            .await
            .expect("first query")
            .cursor;
        let second = LogService::new(release_lock(), root.path().to_path_buf());
        let response = second
            .query(LogQueryRequest {
                cursor: Some(cursor),
                ..Default::default()
            })
            .await
            .expect("second query");
        assert!(response.cursor_reset);
    }

    #[tokio::test]
    async fn runtime_log_source_is_injected_when_service_declares_no_logs() {
        let mut release = release_lock();
        release.services[0].logs.clear();
        let service = LogService::new(release, PathBuf::from("/nonexistent-log-root"));
        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].service_id, "api");
        assert_eq!(sources[0].source_id, "runtime");
        assert_eq!(sources[0].format, "text");
    }

    #[tokio::test]
    async fn existing_runtime_source_is_neither_duplicated_nor_overridden() {
        let mut release = release_lock();
        release.services[0].logs = vec![LogSource {
            id: "runtime".into(),
            glob: "custom-runtime*.log".into(),
            format: LogFormat::Jsonl,
            multiline_start_pattern: None,
        }];
        let service = LogService::new(release, PathBuf::from("/nonexistent-log-root"));
        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        assert_eq!(sources.len(), 1);
        // 用户已声明同 id source：不重复注入，且保留用户声明（jsonl 而非平台合成 text）。
        assert_eq!(sources[0].format, "jsonl");
    }

    #[tokio::test]
    async fn injected_runtime_source_coexists_with_user_declared_sources() {
        // release_lock() 已声明 application 源；runtime 源应共存不覆盖。
        let service = LogService::new(release_lock(), PathBuf::from("/nonexistent-log-root"));
        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        assert_eq!(sources.len(), 2);
        let ids: Vec<&str> = sources
            .iter()
            .map(|source| source.source_id.as_str())
            .collect();
        assert!(ids.contains(&"application"), "{ids:?}");
        assert!(ids.contains(&"runtime"), "{ids:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn service_log_directory_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("log root");
        let outside = tempfile::tempdir().expect("outside directory");
        std::fs::write(outside.path().join("application.log"), "secret\n").expect("outside log");
        symlink(outside.path(), root.path().join("api")).expect("service directory symlink");
        let service = LogService::new(release_lock(), root.path().to_path_buf());

        let response = service
            .query(LogQueryRequest::default())
            .await
            .expect("query isolates source errors");
        assert!(response.logs.is_empty());
        // application 与平台注入的 runtime 两个源都命中 symlink 目录 → 各报一条 source error。
        assert_eq!(response.source_errors.len(), 2);
        assert!(
            response
                .source_errors
                .iter()
                .all(|error| error.message.contains("must be a real directory"))
        );
    }
}
