//! Shared SSE state machine. Providers refresh descriptions without bootstrapping.
use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use futures_util::{Stream, future::BoxFuture};
use shared_types::{LogQueryRequest, LogRecord, SourceError};

use crate::LogService;

pub trait LogProvider: Send + Sync + 'static {
    fn load(&self) -> BoxFuture<'_, Result<LogService>>;
}

pub struct CancelOnDrop(pub Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub enum LogStreamEvent {
    Log(LogRecord),
    SourceError(SourceError),
    SourceRecovered {
        service_id: String,
        source_id: String,
    },
    CursorReset,
    Checkpoint(String),
    Heartbeat,
}
impl LogStreamEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Log(_) => "log",
            Self::SourceError(_) => "source_error",
            Self::SourceRecovered { .. } => "source_recovered",
            Self::CursorReset => "cursor_reset",
            Self::Checkpoint(_) => "checkpoint",
            Self::Heartbeat => "heartbeat",
        }
    }
    pub fn data(&self) -> Result<String> {
        Ok(match self {
            Self::Log(record) => serde_json::to_string(record)?,
            Self::SourceError(error) => serde_json::to_string(error)?,
            Self::SourceRecovered {
                service_id,
                source_id,
            } => serde_json::json!({"service_id": service_id, "source_id": source_id}).to_string(),
            Self::CursorReset => {
                serde_json::json!({"message": "cursor belongs to a previous log catalog"})
                    .to_string()
            }
            Self::Checkpoint(cursor) => cursor.clone(),
            Self::Heartbeat => "{}".into(),
        })
    }
}

pub fn stream(
    provider: Arc<dyn LogProvider>,
    mut request: LogQueryRequest,
) -> Pin<Box<dyn Stream<Item = LogStreamEvent> + Send>> {
    Box::pin(async_stream::stream! {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _guard = CancelOnDrop(cancelled.clone());
        let mut first = true;
        let mut checkpoint: Option<String> = None;
        let mut failures: BTreeSet<(String, String)> = BTreeSet::new();
        let mut last_heartbeat = tokio::time::Instant::now();
        loop {
            if !first { request.tail = None; }
            let result = match provider.load().await {
                Ok(logs) => logs.query_with_cancel(request.clone(), cancelled.clone()).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(response) => {
                    if response.cursor_reset { yield LogStreamEvent::CursorReset; }
                    for record in response.logs { yield LogStreamEvent::Log(record); }
                    let mut current = BTreeSet::new();
                    for error in response.source_errors {
                        let key = (error.service_id.clone(), error.source_id.clone());
                        current.insert(key.clone());
                        if !failures.contains(&key) { yield LogStreamEvent::SourceError(error); }
                    }
                    for (service_id, source_id) in failures.difference(&current) {
                        yield LogStreamEvent::SourceRecovered { service_id: service_id.clone(), source_id: source_id.clone() };
                    }
                    failures = current;
                    request.cursor = Some(response.cursor.clone());
                    if checkpoint.as_deref() != Some(&response.cursor) {
                        checkpoint = Some(response.cursor.clone());
                        yield LogStreamEvent::Checkpoint(response.cursor);
                    }
                }
                Err(error) => {
                    let key = ("workspace".to_string(), "catalog".to_string());
                    if failures.insert(key) {
                        yield LogStreamEvent::SourceError(SourceError {
                            service_id: "workspace".into(), source_id: "catalog".into(),
                            code: "log_observation_failed".into(), message: format!("{error:#}"),
                        });
                    }
                    // A transient observation failure is not a catalog change.
                    // Preserve the checkpoint so recovery cannot silently drop data.
                }
            }
            first = false;
            if last_heartbeat.elapsed() >= Duration::from_secs(15) {
                yield LogStreamEvent::Heartbeat;
                last_heartbeat = tokio::time::Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::atomic::AtomicUsize;

    struct Provider {
        root: std::path::PathBuf,
        calls: AtomicUsize,
    }
    impl LogProvider for Provider {
        fn load(&self) -> BoxFuture<'_, Result<LogService>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(LogService::idle(self.root.clone())) })
        }
    }
    #[tokio::test]
    async fn stream_refreshes_and_resumes_without_spurious_cursor_reset() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("app-cli.log.2026-10-04");
        std::fs::write(&path, "{\"message\":\"before\"}\n").unwrap();
        let provider = Arc::new(Provider {
            root: root.path().to_path_buf(),
            calls: AtomicUsize::new(0),
        });
        let mut stream = stream(provider.clone(), LogQueryRequest::default());
        assert!(
            matches!(stream.next().await, Some(LogStreamEvent::Log(record)) if record.message == "before")
        );
        let Some(LogStreamEvent::Checkpoint(cursor)) = stream.next().await else {
            panic!("checkpoint missing")
        };
        drop(stream);
        std::fs::write(&path, "{\"message\":\"before\"}\n{\"message\":\"after\"}\n").unwrap();
        let mut resumed = super::stream(
            provider.clone(),
            LogQueryRequest {
                cursor: Some(cursor),
                ..Default::default()
            },
        );
        assert!(
            matches!(resumed.next().await, Some(LogStreamEvent::Log(record)) if record.message == "after")
        );
        assert!(provider.calls.load(Ordering::Relaxed) >= 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stream_emits_error_then_recovery_without_losing_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("blocked")).unwrap();
        let provider = Arc::new(Provider {
            root: root.path().join("blocked"),
            calls: AtomicUsize::new(0),
        });
        let mut stream = stream(provider, LogQueryRequest::default());
        assert!(
            matches!(stream.next().await, Some(LogStreamEvent::SourceError(error)) if error.code == "source_read_failed")
        );
        assert!(matches!(
            stream.next().await,
            Some(LogStreamEvent::Checkpoint(_))
        ));
        std::fs::remove_file(root.path().join("blocked")).unwrap();
        std::fs::create_dir(root.path().join("blocked")).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), stream.next())
                .await
                .unwrap(),
            Some(LogStreamEvent::SourceRecovered { .. })
        ));
    }

    #[test]
    fn dropped_read_scope_cancels_blocking_reader() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = CancelOnDrop(cancelled.clone());
        drop(guard);
        assert!(cancelled.load(Ordering::Relaxed));
    }
}
