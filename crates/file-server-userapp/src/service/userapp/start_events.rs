//! Ordered startup progress. Done is a barrier in the same queue as service events.
//!
//! P1-04：编排进程退出与管道结束也进入同一队列——终态不再只依赖 Done 或
//! 等待窗超时：进程退出（任意退出码，含 0 但缺 Done）在短排空窗后判失败；
//! 管道 EOF/读错在进程仍存活时判通道异常。事件顺序由队列天然保证。
use std::sync::Arc;
use std::time::Duration;

use file_server::error::{AppError, AppResult};
use shared_types::BuildProgressEvent;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::tasks::BuildTask;

/// 进程退出后等待迟到 Done/StreamEnded 的排空窗（短且有界：正常路径 Done
/// 先于退出或紧随其后；后代继承 stdout 时管道 EOF 不可依赖）。
const PRODUCER_EXIT_DRAIN_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) enum StartEvent {
    Event(BuildProgressEvent),
    Done {
        failed: Vec<(String, String)>,
    },
    /// 编排进程退出（`exit`=人读描述 + 可选 stderr 尾部；任意退出码）。
    ProducerExited {
        exit: String,
    },
    /// stdout 事件管道结束（EOF/读错）——事件通道不再有新输入。
    StreamEnded {
        reason: String,
    },
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

fn done_outcome(failed: Vec<(String, String)>) -> AppResult<()> {
    if failed.is_empty() {
        return Ok(());
    }
    let summary = failed
        .into_iter()
        .map(|(service, error)| format!("{service}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(AppError::business(format!(
        "Service startup failed: {summary}"
    )))
}

fn exited_failure(exit: &str, detail: String) -> AppError {
    AppError::business(format!(
        "orchestrator exited before completion ({exit}); {detail}"
    ))
}

async fn consume(
    task: Arc<BuildTask>,
    mut rx: mpsc::UnboundedReceiver<StartEvent>,
) -> AppResult<()> {
    // 已观察到进程退出 + 排空截止（此后仍无 Done → 失败，不再等整窗）。
    let mut exited: Option<String> = None;
    let mut drain_deadline: Option<tokio::time::Instant> = None;
    loop {
        let event = match drain_deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(event) => event,
                Err(_) => {
                    let exit = exited.as_deref().unwrap_or("unknown");
                    return Err(exited_failure(
                        exit,
                        "drain window elapsed without completion event".into(),
                    ));
                }
            },
            None => rx.recv().await,
        };
        let Some(event) = event else {
            // 所有发送端释放且无 Done：结合是否已观察到退出给出可诊断文案。
            return Err(match exited {
                Some(exit) => exited_failure(
                    &exit,
                    "event channel closed without completion event".into(),
                ),
                None => AppError::business("Startup event stream closed before completion"),
            });
        };
        match event {
            StartEvent::Event(event) => task.emit(event).await,
            // Done 是终局判据（可能迟到于 ProducerExited——排空窗内到达即有效；
            // 已入队的服务事件先于终态，顺序由同队列保证）。
            StartEvent::Done { failed } => return done_outcome(failed),
            StartEvent::ProducerExited { exit } => {
                tracing::warn!(%exit, "startup orchestrator exited before completion event");
                exited = Some(exit);
                drain_deadline = Some(tokio::time::Instant::now() + PRODUCER_EXIT_DRAIN_WINDOW);
            }
            StartEvent::StreamEnded { reason } => {
                if let Some(exit) = exited {
                    return Err(exited_failure(
                        &exit,
                        format!("event stream ended ({reason})"),
                    ));
                }
                // 进程仍存活但事件管道结束（读错/异常 EOF）：通道不可信，
                // 不能继续等满等待窗。
                return Err(AppError::business(format!(
                    "startup event stream failed before completion ({reason}); orchestrator still running"
                )));
            }
        }
    }
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
