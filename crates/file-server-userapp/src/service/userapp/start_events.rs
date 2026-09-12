//! Ordered startup progress. Done is a barrier in the same queue as service events.
use std::sync::Arc;
use std::time::Duration;

use file_server::error::{AppError, AppResult};
use shared_types::BuildProgressEvent;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::tasks::BuildTask;

#[derive(Debug)]
pub(crate) enum StartEvent {
    Event(BuildProgressEvent),
    Done { failed: Vec<(String, String)> },
}

/// Aborting the caller also stops the progress consumer, including while waiting.
pub(crate) struct StartEventPipe {
    consumer: JoinHandle<AppResult<()>>,
}

impl StartEventPipe {
    pub fn new(task: Arc<BuildTask>) -> (mpsc::UnboundedSender<StartEvent>, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        let consumer = tokio::spawn(consume(task, rx));
        (tx, Self { consumer })
    }

    pub async fn finish(mut self, timeout: Duration) -> AppResult<()> {
        match tokio::time::timeout(timeout, &mut self.consumer).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(AppError::system(format!(
                "Startup event consumer failed: {error}"
            ))),
            Err(_) => {
                self.consumer.abort();
                drop((&mut self.consumer).await);
                Err(AppError::business("Startup completion event timed out"))
            }
        }
    }
}

impl Drop for StartEventPipe {
    fn drop(&mut self) {
        self.consumer.abort();
    }
}

async fn consume(
    task: Arc<BuildTask>,
    mut rx: mpsc::UnboundedReceiver<StartEvent>,
) -> AppResult<()> {
    while let Some(event) = rx.recv().await {
        match event {
            StartEvent::Event(event) => task.emit(event).await,
            StartEvent::Done { failed } => {
                if failed.is_empty() {
                    return Ok(());
                }
                let summary = failed
                    .into_iter()
                    .map(|(service, error)| format!("{service}: {error}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(AppError::business(format!(
                    "Service startup failed: {summary}"
                )));
            }
        }
    }
    Err(AppError::business(
        "Startup event stream closed before completion",
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tasks::BuildTaskStore;
    use super::*;
    use crate::models::BuildTaskKind;

    async fn task() -> Arc<BuildTask> {
        BuildTaskStore::new()
            .create("start-events".into(), BuildTaskKind::DevStart)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn done_waits_for_delayed_consumer_and_preserves_service_order() {
        let task = task().await;
        let (tx, rx) = mpsc::unbounded_channel();
        let (release, barrier) = tokio::sync::oneshot::channel();
        let target = task.clone();
        let pipe = StartEventPipe {
            consumer: tokio::spawn(async move {
                barrier.await.unwrap();
                consume(target, rx).await
            }),
        };
        tx.send(StartEvent::Event(BuildProgressEvent::ServiceStarting {
            service: "web".into(),
        }))
        .unwrap();
        tx.send(StartEvent::Event(BuildProgressEvent::ServiceStartOk {
            service: "web".into(),
        }))
        .unwrap();
        tx.send(StartEvent::Done { failed: vec![] }).unwrap();
        let completion = tokio::spawn(pipe.finish(Duration::from_secs(2)));
        tokio::task::yield_now().await;
        assert!(
            !completion.is_finished(),
            "Done cannot bypass the event consumer"
        );
        release.send(()).unwrap();
        completion.await.unwrap().unwrap();
        task.emit(BuildProgressEvent::Completed {
            release_id: "B".into(),
            sha256: String::new(),
            size_bytes: 0,
            file_name: String::new(),
            artifact_path: String::new(),
        })
        .await;
        let (events, _) = task.subscribe(0).await;
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0].1,
            BuildProgressEvent::ServiceStarting { .. }
        ));
        assert!(matches!(
            events[1].1,
            BuildProgressEvent::ServiceStartOk { .. }
        ));
        assert!(matches!(events[2].1, BuildProgressEvent::Completed { .. }));
    }

    #[tokio::test]
    async fn missing_done_and_failed_done_never_succeed() {
        let (tx, pipe) = StartEventPipe::new(task().await);
        let error = pipe.finish(Duration::from_millis(20)).await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(tx.is_closed());
        let (tx, pipe) = StartEventPipe::new(task().await);
        drop(tx);
        assert!(
            pipe.finish(Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string()
                .contains("closed before")
        );
        let (tx, pipe) = StartEventPipe::new(task().await);
        tx.send(StartEvent::Done {
            failed: vec![("web".into(), "not ready".into())],
        })
        .unwrap();
        assert!(
            pipe.finish(Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string()
                .contains("web: not ready")
        );
    }

    #[tokio::test]
    async fn dropping_pending_pipe_closes_consumer() {
        let (tx, pipe) = StartEventPipe::new(task().await);
        drop(pipe);
        tokio::time::timeout(Duration::from_secs(1), tx.closed())
            .await
            .unwrap();
    }
}
