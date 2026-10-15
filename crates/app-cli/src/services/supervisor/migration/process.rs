use super::*;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::{io::AsyncRead, task::JoinHandle};

#[derive(Clone, Debug)]
pub(super) struct TransientReport {
    pub(super) outcome: MigrationOutcome,
    pub(super) process_success: bool,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) stdout_remaining: String,
    pub(super) stderr_remaining: String,
}

impl TransientReport {
    pub(super) fn empty(outcome: MigrationOutcome) -> Self {
        Self {
            process_success: matches!(outcome, MigrationOutcome::Completed),
            outcome,
            stdout: String::new(),
            stderr: String::new(),
            stdout_remaining: String::new(),
            stderr_remaining: String::new(),
        }
    }
    pub(super) fn advisory(kind: MigrationFailureKind, detail: impl Into<String>) -> Self {
        Self::empty(MigrationOutcome::advisory(kind, detail))
    }
}

#[derive(Debug)]
pub(super) struct TransientDiagnostics(pub(super) TransientReport);
impl std::fmt::Display for TransientDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Migration process cleanup remains unconfirmed")
    }
}
impl std::error::Error for TransientDiagnostics {}

#[derive(Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    streamed_until: usize,
    errors: Vec<String>,
    eof: bool,
}

struct OutputWorker(JoinHandle<()>);
impl Drop for OutputWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn captured(output: &Mutex<CapturedOutput>) -> MutexGuard<'_, CapturedOutput> {
    match output.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

async fn capture_pipe<R: AsyncRead + Unpin>(
    mut pipe: Option<R>,
    output: Arc<Mutex<CapturedOutput>>,
    logs: Option<MigrationLogs>,
    stream: &'static str,
) {
    // Persisting events/logs cannot backpressure the child's output pipes.
    // Capture is complete independently of a slow or interrupted log sink.
    let (sender, mut worker) = match logs {
        Some(logs) => {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            (
                Some(sender),
                Some(OutputWorker(tokio::spawn(publish_lines(
                    receiver,
                    logs,
                    output.clone(),
                    stream,
                )))),
            )
        }
        None => (None, None),
    };
    let mut pending = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut eof = pipe.is_none();
    if let Some(reader) = pipe.as_mut() {
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(count) => {
                    // Retain every read before the next await, including the
                    // last partial line when the process is forcibly stopped.
                    captured(&output).bytes.extend_from_slice(&chunk[..count]);
                    if let Some(sender) = &sender {
                        pending.extend_from_slice(&chunk[..count]);
                        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                            let line: Vec<u8> = pending.drain(..=end).collect();
                            if sender.send(line).is_err() {
                                captured(&output)
                                    .errors
                                    .push(format!("migration {stream} publisher closed"));
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    captured(&output)
                        .errors
                        .push(format!("capture migration {stream}: {error}"));
                    break;
                }
            }
        }
    }
    if !pending.is_empty()
        && let Some(sender) = &sender
        && sender.send(pending).is_err()
    {
        captured(&output).errors.push(format!(
            "migration {stream} partial-output publisher closed"
        ));
    }
    captured(&output).eof = eof;
    drop(sender);
    if let Some(worker) = worker.as_mut()
        && let Err(error) = (&mut worker.0).await
    {
        captured(&output)
            .errors
            .push(format!("join migration {stream} publisher: {error}"));
    }
}

async fn publish_lines(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    logs: MigrationLogs,
    output: Arc<Mutex<CapturedOutput>>,
    stream: &str,
) {
    let mut file = match async {
        tokio::fs::create_dir_all(&logs.directory).await?;
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs.path(stream))
            .await
    }
    .await
    {
        Ok(file) => Some(file),
        Err(error) => {
            captured(&output)
                .errors
                .push(format!("open migration {stream} log: {error}"));
            None
        }
    };
    let mut consumed = 0usize;
    while let Some(line) = receiver.recv().await {
        consumed += line.len();
        let safe = logs.redact(&String::from_utf8_lossy(&line));
        logs.emit(stream, &safe);
        if let Some(writer) = file.as_mut()
            && let Err(error) = writer.write_all(safe.as_bytes()).await
        {
            captured(&output)
                .errors
                .push(format!("write migration {stream}: {error}"));
            file = None;
        }
        captured(&output).streamed_until = consumed;
    }
    if let Some(file) = file.as_mut() {
        if let Err(error) = file.flush().await {
            captured(&output)
                .errors
                .push(format!("flush migration {stream}: {error}"));
        }
        if let Err(error) = file.sync_all().await {
            captured(&output)
                .errors
                .push(format!("sync migration {stream}: {error}"));
        }
    }
}

async fn finish_capture(
    mut task: JoinHandle<()>,
    output: Arc<Mutex<CapturedOutput>>,
    deadline: tokio::time::Instant,
    stream: &str,
) -> (String, String, Vec<String>) {
    match tokio::time::timeout_at(deadline, &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => captured(&output)
            .errors
            .push(format!("join migration {stream} capture: {error}")),
        Err(_) => {
            task.abort();
            let _joined = task.await;
            let mut capture = captured(&output);
            let retained = if capture.eof {
                "captured output retained for remaining publication"
            } else {
                "partial output retained"
            };
            capture
                .errors
                .push(format!("migration {stream} drain timed out; {retained}"));
        }
    }
    let capture = captured(&output);
    (
        String::from_utf8_lossy(&capture.bytes).into_owned(),
        String::from_utf8_lossy(&capture.bytes[capture.streamed_until..]).into_owned(),
        capture.errors.clone(),
    )
}

pub(super) async fn execute_transient<T: Send + 'static>(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    resource: T,
    logs: Option<MigrationLogs>,
) -> Result<(TransientReport, T)> {
    if let Err(error) = validate_transient_input(argv, cwd) {
        return Ok((
            TransientReport::advisory(
                MigrationFailureKind::Input,
                format!("migration input: {error:#}"),
            ),
            resource,
        ));
    }
    let program = argv.first().context("migration command is empty")?;
    let mut command = Command::new(crate::win_cmd::resolve_spawn_program(program));
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, spawn_failure) =
        match process_utils::guardian::spawn_owned_checked(command, None).await {
            Ok(child) => (child, None),
            Err(failure) => {
                let detail = format!("spawn migration: {:#}", failure.source);
                match failure.child {
                    Some(child) => (child, Some(detail)),
                    None => {
                        return Ok((
                            TransientReport::advisory(MigrationFailureKind::Spawn, detail),
                            resource,
                        ));
                    }
                }
            }
        };
    let stdout = Arc::new(Mutex::new(CapturedOutput::default()));
    let stderr = Arc::new(Mutex::new(CapturedOutput::default()));
    let out_task = tokio::spawn(capture_pipe(
        child.take_stdout(),
        stdout.clone(),
        logs.clone(),
        "out",
    ));
    let err_task = tokio::spawn(capture_pipe(
        child.take_stderr(),
        stderr.clone(),
        logs,
        "err",
    ));
    let wait = if spawn_failure.is_some() {
        None
    } else {
        tokio::select! {
            result = tokio::time::timeout(timeout, child.wait_root()) => Some(result),
            () = async { match cancel { Some(token) => token.cancelled().await, None => std::future::pending::<()>().await } } => None,
        }
    };
    // Root success cannot authorize startup while descendants still write.
    let stop = child.stop(Duration::ZERO).await;
    let cleanup_confirmed = stop != StopOutcome::Unconfirmed;
    let mut report = if let Some(detail) = spawn_failure {
        TransientReport::advisory(MigrationFailureKind::Spawn, detail)
    } else {
        match wait {
            Some(Ok(Ok(status))) if status.success() => {
                TransientReport::empty(MigrationOutcome::Completed)
            }
            Some(Ok(Ok(status))) => {
                TransientReport::empty(MigrationOutcome::Advisory(MigrationFailure {
                    kind: MigrationFailureKind::Exit,
                    exit_code: status.code(),
                    detail: format!(
                        "migrate exited {status}; exit_code={}",
                        status
                            .code()
                            .map_or_else(|| "none".to_owned(), |code| code.to_string())
                    ),
                }))
            }
            Some(Ok(Err(error))) => TransientReport::advisory(
                MigrationFailureKind::Wait,
                format!("wait migration: {error}"),
            ),
            Some(Err(_)) => TransientReport::advisory(
                MigrationFailureKind::Timeout,
                format!("migrate timed out after {}ms", timeout.as_millis()),
            ),
            None => {
                TransientReport::advisory(MigrationFailureKind::Cancelled, "Migration cancelled")
            }
        }
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let (out, err) = tokio::join!(
        finish_capture(out_task, stdout, deadline, "stdout"),
        finish_capture(err_task, stderr, deadline, "stderr")
    );
    (report.stdout, report.stdout_remaining) = (out.0, out.1);
    (report.stderr, report.stderr_remaining) = (err.0, err.1);
    for error in out.2.into_iter().chain(err.2) {
        report
            .outcome
            .add_diagnostic(MigrationFailureKind::Output, error);
    }
    if !cleanup_confirmed {
        // The actual child and workspace migration leases outlive this failed
        // bounded observation. The physical domain remains protected.
        process_utils::command_context::retain_cleanup_with_resource(Some(child), None, resource);
        report.outcome.add_diagnostic(
            MigrationFailureKind::Cleanup,
            "Migration process tree shutdown was not confirmed".into(),
        );
        return Err(anyhow::Error::new(ShutdownUnconfirmed(
            "Migration process tree shutdown was not confirmed".into(),
        ))
        .context(TransientDiagnostics(report)));
    }
    Ok((report, resource))
}
