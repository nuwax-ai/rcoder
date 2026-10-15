//! Fault injection is confined to this opt-in test binary.
use anyhow::{Context, Result};
use runtime_supervisor::{Options, Owner, Worker, WorkerControl};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

struct Adapter {
    scope: PathBuf,
    cancel: CancellationToken,
}
#[async_trait::async_trait]
impl WorkerControl for Adapter {
    async fn probe(&self) -> Result<()> {
        if self.scope.join("hang").exists() {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        if self.scope.join("hang").exists() {
            std::future::pending::<()>().await;
        }
        if !self.scope.join("slow-shutdown").exists() {
            self.cancel.cancel();
        }
        Ok(())
    }
    fn shutdown_grace(&self) -> Duration {
        Duration::from_secs(120)
    }
}
fn main() -> Result<()> {
    if let Some(result) = runtime_supervisor::auxiliary_entry() {
        std::process::exit(result?);
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(run())
}
async fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "--resident-owner") {
        return resident_owner(PathBuf::from(
            args.get(1).context("resident owner root missing")?,
        ))
        .await;
    }
    if args.first().is_some_and(|a| a == "--output-flood") {
        use std::io::Write;
        let chunk = vec![b'x'; 8192];
        let mut out = std::io::stdout().lock();
        let mut err = std::io::stderr().lock();
        for _ in 0..256 {
            out.write_all(&chunk)?;
            err.write_all(&chunk)?;
        }
        std::fs::write(
            PathBuf::from(args.get(1).context("output marker missing")?),
            "drained",
        )?;
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "--cleanup") {
        let _cleanup = runtime_supervisor::verify_cleanup_callback()?;
        let scope = PathBuf::from(args.get(1).context("cleanup scope missing")?);
        #[cfg(unix)]
        if scope.join("pause-cleanup").exists() {
            let parent = tokio::process::Command::new("ps")
                .args(["-o", "ppid=", "-p", &std::process::id().to_string()])
                .output()
                .await?;
            anyhow::ensure!(parent.status.success(), "read cleanup parent PID");
            std::fs::write(scope.join("cleanup-parent"), parent.stdout)?;
            while scope.join("pause-cleanup").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        anyhow::ensure!(
            !scope.join("fail-cleanup").exists(),
            "fixture cleanup failed"
        );
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "--leaf") {
        let path = PathBuf::from(args.get(1).context("leaf address path missing")?);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        std::fs::write(path, listener.local_addr()?.to_string())?;
        loop {
            drop(listener.accept().await?);
        }
    }
    let scope = PathBuf::from(args.first().context("scope missing")?);
    if let Some(worker) = Worker::from_env(&scope).await? {
        std::fs::write(
            scope.join("one-shot-env-present"),
            std::env::var_os("SUPERVISION_FIXTURE_ONE_SHOT")
                .is_some()
                .to_string(),
        )?;
        let cancel = CancellationToken::new();
        // A management restart with Stopped must never launch business work.
        let mut child = if worker.intent() != runtime_supervisor::Intent::Stopped {
            let mut cmd = tokio::process::Command::new(std::env::current_exe()?);
            cmd.arg("--leaf").arg(scope.join("leaf-address"));
            Some(process_utils::guardian::spawn_owned(cmd, None).await?)
        } else {
            None
        };
        let _control = worker
            .serve(Arc::new(Adapter {
                scope: scope.clone(),
                cancel: cancel.clone(),
            }))
            .await?;
        cancel.cancelled().await;
        if let Some(child) = &mut child {
            let _ = child.stop(Duration::from_millis(100)).await;
        }
        return Ok(());
    }
    let owner = Owner::try_acquire(&scope)?.context("owner already active")?;
    let mut options = Options::new(args);
    options.restart_on_exit = !scope.join("one-shot-owner").exists();
    if scope.join("external-cleanup-enabled").exists() {
        options.external_cleanup_args =
            Some(vec!["--cleanup".into(), scope.clone().into_os_string()]);
    }
    options.recovery_remove_env = vec!["SUPERVISION_FIXTURE_ONE_SHOT".into()];
    options.policy.probe_interval = Duration::from_millis(100);
    options.policy.probe_timeout = Duration::from_millis(150);
    options.policy.unresponsive_for = Duration::from_millis(600);
    options.policy.initialization_timeout = Duration::from_secs(5);
    options.policy.graceful_stop = Duration::from_millis(250);
    options.policy.negotiate_shutdown_grace = false;
    let shutdown = options.shutdown.clone();
    tokio::spawn(async move {
        drop(tokio::signal::ctrl_c().await);
        shutdown.cancel();
    });
    let code = owner.run(options).await?;
    anyhow::ensure!(code == 0, "fixture exit {code}");
    Ok(())
}

struct ResidentCleanup {
    scope: runtime_supervisor::ResidentScope,
    child: Arc<tokio::sync::Mutex<process_utils::guardian::OwnedChild>>,
    root: PathBuf,
}
#[async_trait::async_trait]
impl runtime_supervisor::OwnerCleanup for ResidentCleanup {
    async fn shutdown(&self) -> Result<()> {
        self.scope.close()?;
        if self.root.join("pause-resident-cleanup").exists() {
            self.scope
                .record_unknown("fixture owner cleanup intentionally paused")?;
            anyhow::bail!("fixture owner cleanup intentionally paused");
        }
        let outcome = self
            .child
            .lock()
            .await
            .stop(Duration::from_millis(100))
            .await;
        anyhow::ensure!(
            outcome != process_utils::managed_tree::StopOutcome::Unconfirmed,
            "fixture resident cleanup unknown"
        );
        self.scope.record_quiescent()
    }
}

async fn resident_owner(root: PathBuf) -> Result<()> {
    use runtime_supervisor::{Binding, BusinessRun, OwnerSession, SessionOptions};
    let owner = Owner::try_acquire(&root)?.context("resident fixture owner already active")?;
    let binding = Binding {
        component: "resident-fixture".into(),
        resource: std::fs::canonicalize(&root)?,
    };
    let session = Arc::new(
        OwnerSession::start(
            owner,
            SessionOptions {
                binding,
                policy: runtime_supervisor::Policy::default(),
                cleanup_adapter: None,
                restart_on_exit: true,
                shutdown: CancellationToken::new(),
            },
        )
        .await?,
    );
    let scope = session.resident_scope("fixture-app").await?;
    anyhow::ensure!(
        session.resident_scope("foreign-app").await.is_err(),
        "foreign application acquired resident"
    );
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command.arg("--leaf").arg(root.join("resident-address"));
    let child = process_utils::guardian::spawn_guarded_owner(command, &scope, false).await?;
    std::fs::write(
        root.join("resident-pid"),
        child.id().context("resident PID missing")?.to_string(),
    )?;
    let child = Arc::new(tokio::sync::Mutex::new(child));
    session.set_owner_cleanup(Arc::new(ResidentCleanup {
        scope: scope.clone(),
        child,
        root: root.clone(),
    }))?;
    let business_session = session.clone();
    let code = session
        .run(Box::new(move |launch| {
            let token = CancellationToken::new();
            let control = Arc::new(Adapter {
                scope: root.clone(),
                cancel: token.clone(),
            });
            let ready = business_session.ready_guard(&launch.generation);
            let resident_scope = scope.clone();
            let root = root.clone();
            Ok(BusinessRun {
                control,
                end: Box::pin(async move {
                    // Exercise resident registration while business current_scope is
                    // installed, including flood output inherited instead of piped.
                    let mut probe = tokio::process::Command::new(std::env::current_exe()?);
                    probe.arg("--output-flood").arg(root.join("flood-complete"));
                    let mut probe =
                        process_utils::guardian::spawn_guarded_owner(probe, &resident_scope, false)
                            .await?;
                    tokio::time::timeout(Duration::from_secs(10), probe.wait_root()).await??;
                    anyhow::ensure!(
                        probe.stop(Duration::ZERO).await
                            != process_utils::managed_tree::StopOutcome::Unconfirmed,
                        "output flood guardian unconfirmed"
                    );
                    ready.mark_ready()?;
                    token.cancelled().await;
                    Ok(None)
                }),
            })
        }))
        .await?;
    anyhow::ensure!(code == 0, "resident fixture owner exit {code}");
    Ok(())
}
