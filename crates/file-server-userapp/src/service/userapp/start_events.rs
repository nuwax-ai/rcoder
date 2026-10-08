//! Ordered startup progress. Done is a barrier in the same queue as service events.
//!
//! P1-04：编排进程退出与管道结束也进入同一队列——终态不再只依赖 Done 或
//! 等待窗超时：进程退出（任意退出码，含 0 但缺 Done）在短排空窗后判失败；
//! 管道 EOF/读错在进程仍存活时判通道异常。事件顺序由队列天然保证。
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use file_server::error::{AppError, AppResult};
use shared_types::BuildProgressEvent;
use tokio::sync::{mpsc, oneshot};
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
    /// 只确认此前入队的事件已经消费，不表示启动成功，也不等待未来的 Done。
    Flush {
        completed: oneshot::Sender<()>,
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

    pub async fn flush(
        &mut self,
        tx: &mpsc::UnboundedSender<StartEvent>,
        timeout: Duration,
    ) -> AppResult<()> {
        let (completed, barrier) = oneshot::channel();
        let queued = tx.send(StartEvent::Flush { completed }).is_ok();
        let drain = async {
            if queued && barrier.await.is_ok() {
                return Ok(());
            }
            // 消费者已到 Done/通道终态时屏障可能未被读取；等待它退出即可，
            // 原启动错误由调用方保留，不能用此处的排空结果改写为启动成功。
            (&mut self.consumer).await.map(|_| ()).map_err(|error| {
                AppError::system(format!("Startup event consumer failed: {error}"))
            })
        };
        tokio::time::timeout(timeout, drain)
            .await
            .map_err(|_| AppError::system("Startup event drain timed out"))?
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

/// 保留结构化事件，同时让只展示 `log` 的消费者看见服务启动结果。
/// 重复同一结果不刷屏，补到具体失败原因时更新；构建成功不代表启动成功。
async fn emit_startup_result_log(
    task: &BuildTask,
    results: &mut HashMap<String, Option<String>>,
    service: &str,
    error: Option<&str>,
) {
    let outcome = error
        .map(|error| file_server::service::dev_server::log::sanitize_sensitive_paths(error.trim()));
    if let Some(previous) = results.get(service)
        && (previous == &outcome
            || matches!((previous, &outcome), (Some(previous), Some(current)) if !previous.is_empty() && current.is_empty()))
    {
        // 重复同一结果不刷屏；没有原因的新事件不能覆盖已经展示的具体原因。
        return;
    }
    if let Some(error) = outcome.as_deref() {
        use shared_types::{
            UserAppDiagnosticCode as Code, UserAppDiagnosticPhase as Phase,
            UserAppDiagnosticScope as Scope, UserAppRepairTarget as Target,
        };
        let root = task.workspace_root().await;
        let mut item = super::diagnostics::diagnostic(
            Code::StartFailed,
            Phase::Start,
            Target::Project,
            root.as_deref(),
            error,
            "查看对应阶段和服务日志，修复失败原因后重试。",
        );
        item.scope = Scope::Service;
        item.service_id = Some(service.to_owned());
        task.record_diagnostic(item).await;
    }
    results.insert(service.to_owned(), outcome.clone());
    let line = match outcome.as_deref() {
        None => format!("服务 {service} 启动成功（启动探测已通过）"),
        Some("") => {
            format!("服务 {service} 启动失败：未获取到具体原因，请查看上方日志")
        }
        Some(error) => format!("服务 {service} 启动失败：{error}"),
    };
    task.emit(BuildProgressEvent::Log {
        service: service.to_owned(),
        line,
    })
    .await;
}

async fn consume(
    task: Arc<BuildTask>,
    mut rx: mpsc::UnboundedReceiver<StartEvent>,
) -> AppResult<()> {
    // 已观察到进程退出 + 排空截止（此后仍无 Done → 失败，不再等整窗）。
    let mut exited: Option<String> = None;
    let mut drain_deadline: Option<tokio::time::Instant> = None;
    let mut startup_results = HashMap::new();
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
            StartEvent::Flush { completed } => {
                // 等待者可能已超时；它退出不改变此前事件已经消费的事实。
                let _ = completed.send(());
            }
            StartEvent::Event(event) => {
                // Copy only the small startup result fields. Ordinary log lines
                // can be large and should move straight into the event journal.
                let result_log = match &event {
                    BuildProgressEvent::ServiceStarting { service } => {
                        startup_results.remove(service);
                        None
                    }
                    // 已观察到编排进程退出时，迟到的缓存事件不是启动成功证明。
                    BuildProgressEvent::ServiceStartOk { service } if exited.is_none() => {
                        Some((service.clone(), None))
                    }
                    BuildProgressEvent::ServiceStartFail { service, error } => {
                        Some((service.clone(), Some(error.clone())))
                    }
                    _ => None,
                };
                task.emit(event).await;
                if let Some((service, error)) = result_log {
                    emit_startup_result_log(
                        &task,
                        &mut startup_results,
                        &service,
                        error.as_deref(),
                    )
                    .await;
                }
            }
            // Done 是终局判据。到达本通道的 Done 必然先于进程退出写入管道
            // （管道字节序），"观察到退出"与"读到 Done"来自不同发送任务、
            // 到达顺序不携带信息——R05 的"退出后迟到的 Done 是缓存成功"
            // 仅对非管道来源的事件成立；本通道内 EOF 前的 Done 一律可信。
            // 排空窗超时/通道关闭仍无 Done 才按退出失败收场。
            StartEvent::Done { failed } => {
                for (service, error) in &failed {
                    emit_startup_result_log(&task, &mut startup_results, service, Some(error))
                        .await;
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
        let (release, barrier) = oneshot::channel();
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
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[0].1,
            BuildProgressEvent::ServiceStarting { .. }
        ));
        assert!(matches!(
            events[1].1,
            BuildProgressEvent::ServiceStartOk { .. }
        ));
        assert!(matches!(
            &events[2].1,
            BuildProgressEvent::Log { service, line }
                if service == "web" && line.contains("启动成功")
        ));
        assert!(matches!(events[3].1, BuildProgressEvent::Completed { .. }));
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
    async fn startup_result_logs_preserve_errors_and_deduplicate_summaries() {
        let task = task().await;
        let (tx, pipe) = StartEventPipe::new(task.clone());
        for event in [
            BuildProgressEvent::BuildOk {
                service: "build-only".into(),
            },
            BuildProgressEvent::ServiceStartOk {
                service: "frontend-react-vite".into(),
            },
            BuildProgressEvent::ServiceStartOk {
                service: "frontend-react-vite".into(),
            },
            BuildProgressEvent::ServiceStartFail {
                service: "worker".into(),
                error: "readiness probe timed out".into(),
            },
            BuildProgressEvent::ServiceStartFail {
                service: "worker".into(),
                error: "readiness probe timed out".into(),
            },
            BuildProgressEvent::ServiceStarting {
                service: "worker".into(),
            },
            BuildProgressEvent::ServiceStartOk {
                service: "worker".into(),
            },
            BuildProgressEvent::ServiceStartFail {
                service: "api".into(),
                error: "TCP connection refused".into(),
            },
            BuildProgressEvent::ServiceStartFail {
                service: "api".into(),
                error: String::new(),
            },
            BuildProgressEvent::ServiceStartFail {
                service: "detail-upgrade".into(),
                error: String::new(),
            },
        ] {
            tx.send(StartEvent::Event(event)).unwrap();
        }
        tx.send(StartEvent::Done {
            failed: vec![
                ("api".into(), "TCP connection refused".into()),
                (
                    "detail-upgrade".into(),
                    "  spawn failed: executable not found  ".into(),
                ),
                (
                    "summary-only".into(),
                    "migration failed: invalid SQL".into(),
                ),
            ],
        })
        .unwrap();
        pipe.finish(Duration::from_secs(2)).await.unwrap_err();
        let (events, _) = task.subscribe(0).await;
        let logs = events
            .iter()
            .filter_map(|(_, event)| match event {
                BuildProgressEvent::Log { service, line } => Some((service, line)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            logs.len(),
            7,
            "duplicate terminal events must not repeat logs"
        );
        assert_eq!(logs[0].0, "frontend-react-vite");
        assert!(logs[0].1.contains("启动成功"));
        assert!(logs[1].1.contains("readiness probe timed out"));
        assert!(
            logs[2].1.contains("启动成功"),
            "a new startup attempt is visible"
        );
        assert!(logs[3].1.contains("TCP connection refused"));
        assert_eq!(logs[4].0, "detail-upgrade");
        assert!(logs[4].1.contains("未获取到具体原因"));
        assert_eq!(logs[5].0, "detail-upgrade");
        assert!(logs[5].1.ends_with("spawn failed: executable not found"));
        assert_eq!(logs[6].0, "summary-only");
        assert!(logs[6].1.contains("migration failed: invalid SQL"));
        for (service, line) in logs {
            assert_ne!(service, "build-only", "build_ok does not prove readiness");
            assert!(!line.trim().is_empty());
            let wire = serde_json::to_value(BuildProgressEvent::Log {
                service: service.clone(),
                line: line.clone(),
            })
            .unwrap();
            assert_eq!(wire["event"], "log");
            assert_eq!(wire["service"], service.as_str());
        }
    }

    #[tokio::test]
    async fn failure_flush_keeps_service_logs_before_task_failed_without_done() {
        let task = task().await;
        let (tx, rx) = mpsc::unbounded_channel();
        let (release, barrier) = oneshot::channel();
        let target = task.clone();
        let mut pipe = StartEventPipe {
            consumer: tokio::spawn(async move {
                barrier.await.unwrap();
                consume(target, rx).await
            }),
        };
        tx.send(StartEvent::Event(BuildProgressEvent::ServiceStartFail {
            service: "frontend-react-vite".into(),
            error: "spawn failed: executable not found".into(),
        }))
        .unwrap();
        let task_for_failure = task.clone();
        let fail = tokio::spawn(async move {
            pipe.flush(&tx, Duration::from_secs(2)).await.unwrap();
            task_for_failure
                .emit(BuildProgressEvent::Failed {
                    error: "original startup error".into(),
                })
                .await;
        });
        tokio::task::yield_now().await;
        assert!(
            !fail.is_finished(),
            "TaskFailed must wait for queued service events"
        );
        release.send(()).unwrap();
        fail.await.unwrap();
        let (events, _) = task.subscribe(0).await;
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[0].1,
            BuildProgressEvent::ServiceStartFail { .. }
        ));
        assert!(matches!(
            &events[1].1,
            BuildProgressEvent::Log { service, line }
                if service == "frontend-react-vite"
                    && line.contains("spawn failed: executable not found")
        ));
        assert!(matches!(
            &events[2].1,
            BuildProgressEvent::Log { service, line } if service == "workspace" && line.contains("应用服务启动失败")
        ));
        assert!(matches!(
            &events[3].1,
            BuildProgressEvent::Failed { error } if error == "original startup error"
        ));
    }

    /// 退出观察与 Done 到达来自不同发送任务，顺序不携带信息；经管道到达的
    /// Done 必然先于进程退出写入（管道字节序）——EOF 前的 Done 一律可信。
    #[tokio::test]
    async fn success_done_after_observed_exit_still_succeeds() {
        for exit in ["0", "1"] {
            let (tx, rx) = mpsc::unbounded_channel();
            let target = task().await;
            let consumer = tokio::spawn(consume(target.clone(), rx));
            tx.send(StartEvent::ProducerExited {
                exit: exit.to_string(),
            })
            .unwrap();
            tx.send(StartEvent::Event(BuildProgressEvent::ServiceStartOk {
                service: "web".into(),
            }))
            .unwrap();
            tx.send(StartEvent::Done { failed: vec![] }).unwrap();
            consumer.await.unwrap().unwrap();
        }
    }

    /// 退出后通道内**始终没有 Done**（EOF/排空窗耗尽）→ 仍按退出失败收场
    ///——可信的判据是"有没有 Done"，不是"Done 与退出观察的先后"。
    #[tokio::test]
    async fn exit_without_any_done_still_fails() {
        let (tx, rx) = mpsc::unbounded_channel();
        let target = task().await;
        let consumer = tokio::spawn(consume(target, rx));
        tx.send(StartEvent::ProducerExited { exit: "1".into() })
            .unwrap();
        drop(tx);
        let error = consumer.await.unwrap().unwrap_err();
        assert!(
            error.to_string().contains("exited before completion"),
            "{error}"
        );
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

    /// 失败 Done（带失败清单）无论退出观察先后都按清单失败——诊断保留明细。
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
        assert!(message.contains("Service startup failed"), "{message}");
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
