use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::DateTime;
use globset::Glob;
use workspace_manifest::{LockedService, LogFormat, ReleaseLock};

use super::filter::compare_timestamps;
use super::model::{
    CursorState, FileCursor, LogQueryRequest, LogQueryResponse, LogRecord, LogSourceInfo,
    MAX_CURSOR_BYTES, MAX_KEYWORD_BYTES, MAX_SERVICES, MAX_SOURCES, MAX_TAIL_PER_SOURCE,
    SourceCursor, SourceError,
};
use super::read::read_file;
pub use super::sources::LogLayout;
use super::sources::{
    MatchedLogFile, ORCHESTRATOR_SERVICE_ID, SelectedSource, file_identity,
    inject_orchestrator_log_source, inject_runtime_log_sources, orchestrator_service,
};

const MAX_FILES_PER_SOURCE: usize = 128;
#[derive(Clone)]
pub struct LogService {
    /// enabled 服务集（已注入 runtime 日志源）。server 动态形态下每次查询按当前
    /// release 构造；空集 = 未部署（idle）——查询返回空、游标按代际失效。
    services: Vec<LockedService>,
    log_root: PathBuf,
    boot_id: String,
    layout: LogLayout,
}

impl LogService {
    pub fn new(release: ReleaseLock, log_root: PathBuf) -> Self {
        Self::with_layout(release, log_root, LogLayout::Builtin)
    }

    pub fn with_layout(mut release: ReleaseLock, log_root: PathBuf, layout: LogLayout) -> Self {
        inject_runtime_log_sources(&mut release, layout);
        inject_orchestrator_log_source(&mut release);
        Self {
            services: release
                .services
                .into_iter()
                .filter(|service| service.enabled)
                .collect(),
            log_root,
            boot_id: uuid::Uuid::new_v4().simple().to_string(),
            layout,
        }
    }

    /// 未部署（idle）形态：空服务集，boot_id 固定 "idle"（无代际可言）。
    /// 编排器源仍注入——空容器/部署失败恰是最需要 app-cli 自身日志的场景。
    pub fn idle(log_root: PathBuf) -> Self {
        Self {
            services: vec![orchestrator_service()],
            log_root,
            boot_id: "idle".to_string(),
            layout: LogLayout::Builtin,
        }
    }

    /// server 动态形态：按当前 release + 部署代（=release_id）构造——换代后
    /// 旧 cursor 的 boot_id 不匹配 → cursor_reset 重放（语义与进程代际一致）。
    pub fn with_boot_id(
        mut release: ReleaseLock,
        log_root: PathBuf,
        boot_id: String,
        layout: LogLayout,
    ) -> Self {
        inject_runtime_log_sources(&mut release, layout);
        inject_orchestrator_log_source(&mut release);
        Self {
            services: release
                .services
                .into_iter()
                .filter(|service| service.enabled)
                .collect(),
            log_root,
            boot_id,
            layout,
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
                    // {:#} 保留 anyhow 完整错误链——to_string() 只有最外层
                    // context，根因（如非法正则/IO 错误）会被吞掉，无法排障。
                    message: format!("{error:#}"),
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
        let directory = if selected.service_id == ORCHESTRATOR_SERVICE_ID
            && selected.source.id == "orchestrator"
        {
            // 编排器源：app-cli 自身日志直接落在 log_root 根目录（非 {svc}/ 子目录）
            self.log_root.clone()
        } else if self.layout == LogLayout::Supervisord && selected.source.id == "runtime" {
            self.log_root.join("services")
        } else {
            // 用户声明源（应用自写文件）两布局同目录：{log_root}/{svc}/
            self.log_root.join(&selected.service_id)
        };
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
            // 零匹配是正常状态（文件尚未写出/已被轮转清理），不是读失败：
            // 报错会让全局查询在首启窗口与 static 服务上恒定带
            // source_read_failed 噪音。匹配可见性由 sources/query 的
            // matched_files=[] 承担。
            return Ok(Vec::new());
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
        // 损坏游标（base64/JSON 解不开：截断、手改）自愈为全量重读而非 400
        // ——与 cursor_reset 契约一致（客户端丢弃本地 cursor 从 tail 重读，
        // 最坏重复消费，不丢日志）。
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CursorState>(&bytes).ok());
        match decoded {
            Some(cursor) if cursor.boot_id == self.boot_id => Ok((cursor, false)),
            _ => Ok((self.empty_cursor(), true)),
        }
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

#[cfg(test)]
mod tests {
    use workspace_manifest::LogSource;

    use super::super::model::LogSelector;
    use super::super::read::MAX_LINE_BYTES;
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

    #[tokio::test]
    async fn source_error_does_not_commit_partial_cursor_progress() {
        // 触发器说明：超长行已改为截断续读（见 oversized_line 测试），这里用
        // 非法 multiline 正则构造确定性的源读失败，验证坏源不产出日志、
        // 好源进度正常提交（增量续拉只返回好源新行）。
        let root = tempfile::tempdir().expect("log root");
        let api_dir = root.path().join("api");
        std::fs::create_dir_all(&api_dir).expect("api log directory");
        std::fs::create_dir_all(root.path().join("broken")).expect("broken log directory");
        let api_log = api_dir.join("application-1.log");
        std::fs::write(
            &api_log,
            "{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"first\"}\n",
        )
        .expect("api log");
        std::fs::write(root.path().join("broken/application.log"), "line\n").expect("broken log");
        let mut release = release_lock();
        let mut broken = release.services[0].clone();
        broken.service_id = "broken".into();
        broken.name = "broken".into();
        broken.logs[0].format = workspace_manifest::LogFormat::Text;
        broken.logs[0].multiline_start_pattern = Some("[".into()); // 非法正则：读文件时编译失败
        release.services.push(broken);
        let service = LogService::new(release, root.path().to_path_buf());

        let initial = service
            .query(LogQueryRequest::default())
            .await
            .expect("initial query");
        assert_eq!(initial.logs.len(), 1);
        assert_eq!(initial.logs[0].message, "first");
        assert_eq!(initial.source_errors.len(), 1);
        assert_eq!(initial.source_errors[0].service_id, "broken");

        // 好源进度已提交：追加一行后增量续拉只返回新行（坏源仍报错、不产日志）。
        std::fs::write(
            &api_log,
            "{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"first\"}\n{\"timestamp\":\"2026-08-03T00:00:02Z\",\"message\":\"new\"}\n",
        )
        .expect("append api log");
        let incremental = service
            .query(LogQueryRequest {
                cursor: Some(initial.cursor),
                ..Default::default()
            })
            .await
            .expect("incremental query");
        assert_eq!(incremental.logs.len(), 1);
        assert_eq!(incremental.logs[0].message, "new");
    }

    /// 超长行截断续读：旧行为 bail! 会毒化整源（cursor 不前进、每次重读撞
    /// 同一行直到轮转走）；新行为产一条截断记录并越过该行。
    #[tokio::test]
    async fn oversized_line_is_truncated_instead_of_poisoning_the_source() {
        let root = tempfile::tempdir().expect("log root");
        let directory = root.path().join("api");
        std::fs::create_dir_all(&directory).expect("service log directory");
        let oversized = "x".repeat(MAX_LINE_BYTES + 10);
        let line_bytes = oversized.len() + 1; // 计入换行
        let mut contents = format!(
            "{{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"before\"}}\n{oversized}\n\
             {{\"timestamp\":\"2026-08-03T00:00:01Z\",\"message\":\"after\"}}\n"
        );
        std::fs::write(directory.join("application-1.log"), &contents).expect("log file");
        std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
        let service = LogService::new(release_lock(), root.path().to_path_buf());

        let snapshot = service
            .query(LogQueryRequest::default())
            .await
            .expect("snapshot query");
        assert!(
            snapshot.source_errors.is_empty(),
            "{:?}",
            snapshot.source_errors
        );
        assert_eq!(snapshot.logs.len(), 3);
        assert_eq!(snapshot.logs[0].message, "before");
        assert_eq!(snapshot.logs[1].message, "after");
        // 无时间戳的截断记录排最后（compare_timestamps 里 ts 为 None 恒排后）。
        let truncated = &snapshot.logs[2];
        assert!(
            truncated.message.starts_with("xxxx"),
            "{}",
            truncated.message
        );
        assert!(truncated.message.contains(&format!(
            "[app-cli truncated: original line was {line_bytes} bytes]"
        )));
        assert_eq!(truncated.timestamp, None);

        // cursor 已越过超长行：追加后增量续拉只返回新行。
        contents.push_str("{\"timestamp\":\"2026-08-03T00:00:02Z\",\"message\":\"new\"}\n");
        std::fs::write(directory.join("application-1.log"), &contents).expect("append log");
        let incremental = service
            .query(LogQueryRequest {
                cursor: Some(snapshot.cursor),
                ..Default::default()
            })
            .await
            .expect("incremental query");
        assert_eq!(incremental.logs.len(), 1);
        assert_eq!(incremental.logs[0].message, "new");
    }

    /// 行长恰为 MAX_LINE_BYTES+1 且以 \n 结尾：截断判定触发但行已完整读入，
    /// 不得把下一行误吞进截断记录。
    #[tokio::test]
    async fn boundary_oversized_line_does_not_swallow_the_next_line() {
        let root = tempfile::tempdir().expect("log root");
        let directory = root.path().join("api");
        std::fs::create_dir_all(&directory).expect("service log directory");
        // 恰好 MAX+1 字节（含换行）的单行 + 前后各一条正常行。
        let boundary = format!("{}\n", "y".repeat(MAX_LINE_BYTES));
        assert_eq!(boundary.len(), MAX_LINE_BYTES + 1);
        std::fs::write(
            directory.join("application-1.log"),
            format!(
                "{{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"before\"}}\n{boundary}\
                 {{\"timestamp\":\"2026-08-03T00:00:01Z\",\"message\":\"after\"}}\n"
            ),
        )
        .expect("log file");
        std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
        let service = LogService::new(release_lock(), root.path().to_path_buf());

        let response = service
            .query(LogQueryRequest::default())
            .await
            .expect("query");
        assert!(
            response.source_errors.is_empty(),
            "{:?}",
            response.source_errors
        );
        assert_eq!(response.logs.len(), 3);
        assert_eq!(response.logs[0].message, "before");
        assert_eq!(response.logs[1].message, "after");
        assert!(response.logs[2].message.contains(&format!(
            "[app-cli truncated: original line was {} bytes]",
            MAX_LINE_BYTES + 1
        )));
    }

    /// 损坏 cursor（base64/JSON 解不开）自愈为全量重读，与 cursor_reset 契约一致。
    #[tokio::test]
    async fn undecodable_cursor_self_heals_as_reset() {
        use base64::Engine;
        let root = tempfile::tempdir().expect("log root");
        std::fs::create_dir_all(root.path().join("api")).expect("service log directory");
        std::fs::write(
            root.path().join("api/application-1.log"),
            "{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"first\"}\n",
        )
        .expect("log file");
        std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
        let service = LogService::new(release_lock(), root.path().to_path_buf());
        let not_base64 = "@@@not-base64@@@".to_string();
        let not_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("not json")
            .to_string();
        for garbage in [not_base64, not_json] {
            let response = service
                .query(LogQueryRequest {
                    cursor: Some(garbage),
                    ..Default::default()
                })
                .await
                .expect("self-heal instead of 400");
            assert!(response.cursor_reset);
            assert_eq!(response.logs.len(), 1);
            assert_eq!(response.logs[0].message, "first");
        }
    }

    /// static 服务无进程，runtime.{out,err}.log 永不存在——不注入，避免
    /// 全局查询/流恒定带 no-match 噪音。
    #[tokio::test]
    async fn static_service_is_not_injected_runtime_source() {
        let mut release = release_lock();
        release.services[0].r#type = workspace_manifest::ProjectType::Static;
        release.services[0].logs.clear();
        let service = LogService::new(release, PathBuf::from("/nonexistent-log-root"));
        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].service_id, "app-cli");
    }

    /// 零匹配（文件尚未写出/已被轮转清理）是正常状态，不是读失败——
    /// 匹配可见性由 sources/query 的 matched_files=[] 承担。
    #[tokio::test]
    async fn zero_matched_files_is_not_a_source_error() {
        let root = tempfile::tempdir().expect("log root");
        std::fs::create_dir_all(root.path().join("api")).expect("service log directory");
        std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
        let service = LogService::new(release_lock(), root.path().to_path_buf());
        let response = service
            .query(LogQueryRequest::default())
            .await
            .expect("query");
        assert!(response.logs.is_empty());
        assert!(
            response.source_errors.is_empty(),
            "{:?}",
            response.source_errors
        );
        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        assert!(
            sources
                .iter()
                .any(|source| source.service_id == "api" && source.matched_files.is_empty())
        );
    }

    /// source_error 文案保留 anyhow 完整错误链（{:#}）——to_string() 只剩
    /// 最外层 context，根因（非法正则等）会被吞掉，无法排障。
    #[tokio::test]
    async fn source_error_message_keeps_root_cause_chain() {
        let root = tempfile::tempdir().expect("log root");
        let directory = root.path().join("api");
        std::fs::create_dir_all(&directory).expect("service log directory");
        std::fs::write(directory.join("application-1.log"), "line\n").expect("log file");
        std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
        let mut release = release_lock();
        release.services[0].logs[0].format = workspace_manifest::LogFormat::Text;
        release.services[0].logs[0].multiline_start_pattern = Some("[".into());
        let service = LogService::new(release, root.path().to_path_buf());
        let response = service
            .query(LogQueryRequest::default())
            .await
            .expect("query");
        assert_eq!(response.source_errors.len(), 1);
        let message = &response.source_errors[0].message;
        assert!(message.contains("read source api/application"), "{message}");
        assert!(
            message.contains("compile multiline_start_pattern"),
            "{message}"
        );
        assert!(message.contains("regex parse error"), "{message}");
    }

    #[tokio::test]
    async fn cursor_from_previous_boot_is_reported_as_reset() {
        let root = tempfile::tempdir().expect("log root");
        std::fs::create_dir_all(root.path().join("api")).expect("service log directory");
        std::fs::write(root.path().join("api/application.log"), "").expect("log file");
        std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
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
        // api/runtime + 内置 app-cli/orchestrator（空 selectors 全遍历）。
        assert_eq!(sources.len(), 2);
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
        // api/runtime（用户声明）+ 内置 app-cli/orchestrator。
        assert_eq!(sources.len(), 2);
        // 用户已声明同 id source：不重复注入，且保留用户声明（jsonl 而非平台合成 text）。
        assert_eq!(sources[0].format, "jsonl");
    }

    #[tokio::test]
    async fn injected_runtime_source_coexists_with_user_declared_sources() {
        // release_lock() 已声明 application 源；runtime 源应共存不覆盖，
        // 内置 app-cli/orchestrator 源并存。
        let service = LogService::new(release_lock(), PathBuf::from("/nonexistent-log-root"));
        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        assert_eq!(sources.len(), 3);
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
        // 内置 orchestrator 源匹配 log_root 根目录，须有真实文件避免其 error 混入断言。
        std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
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

    /// 内置编排器源：空 selectors 可见，文件匹配走 log_root 根目录特判，
    /// JSON 行（文件层 JSON 化产物）解析出 timestamp/level/message。
    #[tokio::test]
    async fn orchestrator_source_matches_root_directory_glob() {
        let root = tempfile::tempdir().expect("log root");
        std::fs::write(
            root.path().join("app-cli.log.2026-08-31"),
            "{\"timestamp\":\"2026-08-31T00:00:00Z\",\"level\":\"INFO\",\"message\":\"🚀 start web\"}\n",
        )
        .expect("orchestrator log");
        let service = LogService::new(release_lock(), root.path().to_path_buf());

        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        let orchestrator = sources
            .iter()
            .find(|source| source.service_id == "app-cli")
            .expect("orchestrator source present");
        assert_eq!(orchestrator.source_id, "orchestrator");
        assert_eq!(orchestrator.format, "jsonl");
        assert!(
            orchestrator
                .matched_files
                .contains(&"app-cli.log.2026-08-31".to_string())
        );

        let response = service
            .query(LogQueryRequest {
                selectors: vec![LogSelector {
                    service_id: "app-cli".into(),
                    source_ids: Vec::new(),
                }],
                ..Default::default()
            })
            .await
            .expect("orchestrator query");
        let record = response
            .logs
            .iter()
            .find(|log| log.service_id == "app-cli")
            .expect("orchestrator record");
        assert_eq!(record.level.as_deref(), Some("INFO"));
        assert!(record.message.contains("🚀 start web"), "{record:?}");
    }

    /// 用户 manifest 占用 "app-cli" 服务名时不注入（用户声明优先），且其
    /// 日志目录解析不受编排器根目录特判影响。
    #[tokio::test]
    async fn orchestrator_source_skipped_when_service_id_taken() {
        let mut release = release_lock();
        release.services[0].service_id = "app-cli".into();
        let service = LogService::new(release, PathBuf::from("/nonexistent-log-root"));
        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        assert!(
            !sources
                .iter()
                .any(|source| source.source_id == "orchestrator"),
            "{sources:?}"
        );
    }

    /// idle 形态（空容器/未部署）仍注入编排器源——部署失败排障恰需此源。
    #[tokio::test]
    async fn idle_service_still_exposes_orchestrator_source() {
        let root = tempfile::tempdir().expect("log root");
        std::fs::write(root.path().join("app-cli.log.2026-08-31"), "{}\n").expect("log");
        let service = LogService::idle(root.path().to_path_buf());
        let sources = service
            .sources(LogQueryRequest::default())
            .await
            .expect("sources query");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].service_id, "app-cli");
        assert_eq!(sources[0].source_id, "orchestrator");
    }
}
