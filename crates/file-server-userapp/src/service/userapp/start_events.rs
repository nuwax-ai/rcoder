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
            // Done 是终局判据——但**仅在未先观察到编排进程退出时**（R05：
            // 退出先于 Done 被观察到 → 迟到的缓存成功 Done 不能证明启动
            // 成功提交，Plan §阶段一"已观察到编排进程在启动完成提交前退出，
            // 不能仅凭缓存 Done 成功"；排空窗收集的失败清单保留在诊断里）。
            // Done 先到、之后才退出 = 成功提交后的运行健康变化，不在此路径。
            StartEvent::Done { failed } => {
                if let Some(exit) = exited.as_deref() {
                    let summary = failed
                        .into_iter()
                        .map(|(service, error)| format!("{service}: {error}"))
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(exited_failure(
                        exit,
                        format!(
                            "completion event arrived after orchestrator exit was observed \
                             (cached success is not a completion proof); failed services: {summary}"
                        ),
                    ));
                }
                return done_outcome(failed);
            }
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

    /// R05：退出先于成功 Done 被观察到 → 迟到的缓存成功不能证明启动成功
    ///（exit 0 与非零同拒——Plan §阶段一"退出后无有效 Done"）。
    #[tokio::test]
    async fn success_done_after_observed_exit_never_succeeds() {
        for exit in ["0", "1"] {
            let (tx, rx) = mpsc::unbounded_channel();
            let target = task().await;
            let consumer = tokio::spawn(consume(target, rx));
            tx.send(StartEvent::ProducerExited {
                exit: exit.to_string(),
            })
            .unwrap();
            tx.send(StartEvent::Done { failed: vec![] }).unwrap();
            let error = consumer.await.unwrap().unwrap_err();
            assert!(
                error.to_string().contains("exited before completion"),
                "exit {exit}: {error}"
            );
        }
    }

    /// R05 对照组：正常 Done 先提交、之后进程退出——成功不追改。
    #[tokio::test]
    async fn success_done_before_exit_still_succeeds() {
        let (tx, rx) = mpsc::unbounded_channel();
        let target = task().await;
        let consumer = tokio::spawn(consume(target, rx));
        tx.send(StartEvent::Done { failed: vec![] }).unwrap();
        consumer.await.unwrap().unwrap();
    }

    /// R05：退出 → 失败 Done（带失败清单）→ 失败保留清单诊断。
    #[tokio::test]
    async fn failed_done_after_observed_exit_keeps_failure_detail() {
        let (tx, rx) = mpsc::unbounded_channel();
        let target = task().await;
        let consumer = tokio::spawn(consume(target, rx));
        tx.send(StartEvent::ProducerExited { exit: "1".into() })
            .unwrap();
        tx.send(StartEvent::Done {
            failed: vec![("web".into(), "probe failed".into())],
        })
        .unwrap();
        let error = consumer.await.unwrap().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("exited before completion"), "{message}");
        assert!(message.contains("web: probe failed"), "{message}");
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
