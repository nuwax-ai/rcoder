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
// app-cli's virtual service: orchestrator, owner-recovery, management-launch, dev-server.
const MAX_BUILTIN_SOURCES: usize = 4;
#[derive(Clone)]
pub struct LogService {
    /// enabled 服务集（已注入 runtime 日志源）。server 动态形态下每次查询按当前
    /// release 构造；空集 = 未部署（idle）——查询返回空、游标按代际失效。
    services: Vec<LockedService>,
    log_root: PathBuf,
    boot_id: String,
    layout: LogLayout,
    roots: BTreeMap<String, Vec<PathBuf>>,
    diagnostics: Vec<SourceError>,
    builtin_orchestrator: bool,
}

impl LogService {
    pub fn new(release: ReleaseLock, log_root: PathBuf) -> Self {
        Self::with_layout(release, log_root, LogLayout::Builtin)
    }

    pub fn with_layout(mut release: ReleaseLock, log_root: PathBuf, layout: LogLayout) -> Self {
        let builtin_orchestrator = !release
            .services
            .iter()
            .any(|service| service.service_id == ORCHESTRATOR_SERVICE_ID);
        inject_runtime_log_sources(&mut release, layout);
        inject_orchestrator_log_source(&mut release);
        let descriptions = release
            .services
            .iter()
            .filter(|service| service.enabled)
            .map(|service| (&service.service_id, &service.logs))
            .collect::<Vec<_>>();
        let stable_id = format!(
            "{}:{}:{layout:?}:{descriptions:?}",
            release.release_id,
            log_root.display()
        );
        Self {
            services: release
                .services
                .into_iter()
                .filter(|service| service.enabled)
                .collect(),
            log_root,
            boot_id: stable_id,
            layout,
            roots: BTreeMap::new(),
            diagnostics: Vec::new(),
            builtin_orchestrator,
        }
    }

    /// 未部署（idle）形态：空服务集，boot_id 固定 "idle"（无代际可言）。
    /// 编排器源仍注入——空容器/部署失败恰是最需要 app-cli 自身日志的场景。
    pub fn idle(log_root: PathBuf) -> Self {
        let builtin_orchestrator = true;
        Self {
            services: vec![orchestrator_service()],
            log_root,
            boot_id: "idle".to_string(),
            layout: LogLayout::Builtin,
            roots: BTreeMap::new(),
            diagnostics: Vec::new(),
            builtin_orchestrator,
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
        let builtin_orchestrator = !release
            .services
            .iter()
            .any(|service| service.service_id == ORCHESTRATOR_SERVICE_ID);
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
            roots: BTreeMap::new(),
            diagnostics: Vec::new(),
            builtin_orchestrator,
        }
    }

    pub async fn sources(&self, request: LogQueryRequest) -> Result<Vec<LogSourceInfo>> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || service.sources_blocking(&request))
            .await
            .context("join log source query")?
    }

    fn sources_blocking(&self, request: &LogQueryRequest) -> Result<Vec<LogSourceInfo>> {
        let mut sources = Vec::new();
        for selected in self.select(request)? {
            let (files, diagnostic) = match self.match_files(&selected) {
                Ok(files) => (files, None),
                Err(error) => (
                    Vec::new(),
                    Some(SourceError {
                        service_id: selected.service_id.clone(),
                        source_id: selected.source.id.clone(),
                        code: "source_read_failed".into(),
                        message: format!("{error:#}"),
                    }),
                ),
            };
            sources.push(LogSourceInfo {
                service_id: selected.service_id,
                source_id: selected.source.id,
                format: match selected.source.format {
                    LogFormat::Jsonl => "jsonl",
                    LogFormat::Text => "text",
                }
                .into(),
                matched_files: files
                    .iter()
                    .filter_map(|f| f.path.file_name().map(|n| n.to_string_lossy().to_string()))
                    .collect(),
                diagnostic,
            });
        }
        sources.extend(self.diagnostics.iter().map(|diagnostic| LogSourceInfo {
            service_id: diagnostic.service_id.clone(),
            source_id: diagnostic.source_id.clone(),
            format: "text".into(),
            matched_files: Vec::new(),
            diagnostic: Some(diagnostic.clone()),
        }));
        Ok(sources)
    }

    pub fn from_catalog(catalog: crate::catalog::LogCatalog) -> Self {
        let builtin_orchestrator = catalog.builtin_orchestrator;
        let services = catalog
            .services
            .into_iter()
            .map(|descriptor| {
                let mut service = orchestrator_service();
                service.service_id = descriptor.service_id;
                service.logs = descriptor.sources;
                service
            })
            .collect();
        Self {
            services,
            log_root: catalog.log_root,
            boot_id: catalog.version,
            layout: catalog.layout,
            roots: BTreeMap::new(),
            diagnostics: Vec::new(),
            builtin_orchestrator,
        }
    }

    /// Additional exact directories; callers must authorize roots from platform config.
    pub fn add_directory(
        &mut self,
        service_id: &str,
        source: workspace_manifest::LogSource,
        directory: PathBuf,
    ) -> Result<()> {
        workspace_manifest::validate_service_id(service_id).context("invalid log service id")?;
        let key = format!("{service_id}/{}", source.id);
        let roots = self.roots.entry(key).or_default();
        if !roots.contains(&directory) {
            roots.push(directory);
        }
        if let Some(service) = self
            .services
            .iter_mut()
            .find(|s| s.service_id == service_id)
        {
            if !service.logs.iter().any(|s| s.id == source.id) {
                service.logs.push(source);
            }
        } else {
            let mut service = orchestrator_service();
            service.service_id = service_id.into();
            service.logs = vec![source];
            self.services.push(service);
        }
        Ok(())
    }

    /// Existing per-source patterns apply to each known writer root.
    pub fn add_writer_root(&mut self, root: &Path) {
        for service in &self.services {
            for source in &service.logs {
                if service.service_id == ORCHESTRATOR_SERVICE_ID && source.id == "orchestrator" {
                    continue;
                }
                let directory = if source.id == "runtime" && self.layout == LogLayout::Supervisord {
                    root.join("services")
                } else {
                    root.join(&service.service_id)
                };
                let key = format!("{}/{}", service.service_id, source.id);
                let roots = self.roots.entry(key).or_insert_with(|| {
                    vec![
                        if source.id == "runtime" && self.layout == LogLayout::Supervisord {
                            self.log_root.join("services")
                        } else {
                            self.log_root.join(&service.service_id)
                        },
                    ]
                });
                if !roots.contains(&directory) {
                    roots.push(directory);
                }
            }
        }
    }

    /// Only the two verified pre-catalog runtime writer layouts are recognized.
    pub fn add_legacy_runtime_roots(&mut self, roots: &[PathBuf]) {
        for service in &mut self.services {
            for source in &mut service.logs {
                if source.id != "runtime"
                    || source.format != LogFormat::Text
                    || (source.glob != "runtime.*.log"
                        && source.glob != format!("{}.log", service.service_id))
                {
                    continue;
                }
                source.glob = format!("{{runtime.*.log,{}.log}}", service.service_id);
                let directories = self
                    .roots
                    .entry(format!("{}/{}", service.service_id, source.id))
                    .or_default();
                for root in roots {
                    for directory in [root.join(&service.service_id), root.join("services")] {
                        if !directories.contains(&directory) {
                            directories.push(directory);
                        }
                    }
                }
            }
        }
    }

    pub fn replace_pattern(&mut self, service_id: &str, source_id: &str, pattern: String) {
        if let Some(source) = self
            .services
            .iter_mut()
            .find(|s| s.service_id == service_id)
            .and_then(|service| service.logs.iter_mut().find(|s| s.id == source_id))
        {
            source.glob = pattern;
        }
    }

    fn cursor_version(&self) -> String {
        use sha2::{Digest, Sha256};
        let descriptions = self
            .services
            .iter()
            .map(|s| (&s.service_id, &s.logs))
            .collect::<Vec<_>>();
        hex::encode(Sha256::digest(format!(
            "{}:{descriptions:?}:{:?}",
            self.boot_id, self.roots
        )))
    }

    pub fn add_diagnostic(&mut self, diagnostic: SourceError) {
        self.diagnostics.push(diagnostic);
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
        let mut source_errors = self.diagnostics.clone();
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
        let business_selectors = request
            .selectors
            .iter()
            .filter(|selector| {
                !(self.builtin_orchestrator && selector.service_id == ORCHESTRATOR_SERVICE_ID)
            })
            .count();
        if request.selectors.len() > MAX_SERVICES + usize::from(self.builtin_orchestrator)
            || business_selectors > MAX_SERVICES
        {
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
            .filter(|source| {
                !(self.builtin_orchestrator && source.service_id == ORCHESTRATOR_SERVICE_ID)
            })
            .map(|source| source.service_id.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        if selected_service_count > MAX_SERVICES {
            anyhow::bail!("selected services exceeds maximum of {MAX_SERVICES}");
        }
        let builtin_sources = selected
            .iter()
            .filter(|source| {
                self.builtin_orchestrator && source.service_id == ORCHESTRATOR_SERVICE_ID
            })
            .count();
        if selected.len() - builtin_sources > MAX_SOURCES || builtin_sources > MAX_BUILTIN_SOURCES {
            anyhow::bail!(
                "selected sources exceeds maximum of {MAX_SOURCES} business sources and {MAX_BUILTIN_SOURCES} builtin sources"
            );
        }
        Ok(selected)
    }

    fn default_directory(&self, selected: &SelectedSource) -> PathBuf {
        if selected.service_id == ORCHESTRATOR_SERVICE_ID && selected.source.id == "orchestrator" {
            // 编排器源：app-cli 自身日志直接落在 log_root 根目录（非 {svc}/ 子目录）
            self.log_root.clone()
        } else if self.layout == LogLayout::Supervisord && selected.source.id == "runtime" {
            self.log_root.join("services")
        } else {
            // 用户声明源（应用自写文件）两布局同目录：{log_root}/{svc}/
            self.log_root.join(&selected.service_id)
        }
    }

    fn match_files(&self, selected: &SelectedSource) -> Result<Vec<MatchedLogFile>> {
        workspace_manifest::validate_service_id(&selected.service_id)
            .context("invalid service log path")?;
        let key = format!("{}/{}", selected.service_id, selected.source.id);
        let directories = self
            .roots
            .get(&key)
            .cloned()
            .unwrap_or_else(|| vec![self.default_directory(selected)]);
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        for directory in directories {
            for file in self.match_directory(&selected.source.glob, &directory)? {
                if seen.insert(file.identity.clone()) {
                    files.push(file);
                }
                anyhow::ensure!(
                    files.len() <= MAX_FILES_PER_SOURCE,
                    "log source matches too many files"
                );
            }
        }
        files.sort_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(files)
    }

    fn match_directory(&self, pattern: &str, directory: &Path) -> Result<Vec<MatchedLogFile>> {
        let matcher = Glob::new(pattern)
            .with_context(|| format!("invalid source glob {}", pattern))?
            .compile_matcher();
        let mut files = Vec::new();
        let directory_metadata = match std::fs::symlink_metadata(directory) {
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
        let entries = match std::fs::read_dir(directory) {
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
                    anyhow::bail!("source matches more than {MAX_FILES_PER_SOURCE} files");
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
            let identity = matched.identity.clone();
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
                &matched,
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
            Some(cursor) if cursor.boot_id == self.cursor_version() => Ok((cursor, false)),
            _ => Ok((self.empty_cursor(), true)),
        }
    }

    fn empty_cursor(&self) -> CursorState {
        CursorState {
            boot_id: self.cursor_version(),
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
#[path = "service_tests.rs"]
mod tests;
