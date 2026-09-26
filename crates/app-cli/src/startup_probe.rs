//! Startup evidence tied to an owned execution, never a process name or cached PID.
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use workspace_manifest::{PROCESS_STARTUP_OBSERVATION_SECONDS, StartupProbe};

use crate::{manifest::ServiceSpec, platform::process_tree::ManagedChild};

pub(crate) fn deadline(spec: &ServiceSpec, started: Instant) -> Result<Instant> {
    started
        .checked_add(Duration::from_secs(spec.health.startup_timeout_seconds))
        .context("startup timeout is outside the supported clock range")
}

pub(crate) async fn within_budget<T>(
    deadline: Instant,
    cancel: Option<&CancellationToken>,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let cancelled = async {
        match cancel {
            Some(cancel) => cancel.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = cancelled => bail!("Startup check cancelled"),
        result = tokio::time::timeout_at(deadline, future) => result.context("Startup check deadline exceeded")?,
    }
}

pub(crate) fn root_alive(child: &mut ManagedChild) -> Result<()> {
    if let Some(status) = child
        .try_wait_root()
        .context("observe owned service root process")?
    {
        bail!("Owned service root exited before startup completed: {status}");
    }
    Ok(())
}

pub(crate) async fn builtin(
    spec: &ServiceSpec,
    child: &mut ManagedChild,
    dev_profile: bool,
    started: Instant,
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    let Some(probe) = spec.health.startup_probe else {
        // Preserve legacy probe selection and its historical budget start.
        return within_budget(deadline(spec, Instant::now())?, cancel, async {
            if dev_profile && spec.devrun.is_some() {
                crate::supervisor::wait_for_port_open(spec, spec.health.startup_timeout_seconds)
                    .await
            } else {
                crate::supervisor::wait_for_service_ready_within(
                    spec,
                    spec.health.startup_timeout_seconds,
                )
                .await
            }
        })
        .await;
    };
    within_budget(deadline(spec, started)?, cancel, async {
        root_alive(child)?;
        if probe == StartupProbe::Process {
            // Launch preparation consumes the deadline but is not evidence that
            // the child was alive. Observe five full seconds after spawn.
            let observed = Instant::now();
            loop {
                root_alive(child)?;
                if observed.elapsed() >= Duration::from_secs(PROCESS_STARTUP_OBSERVATION_SECONDS) {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let network = network(spec, probe);
        tokio::pin!(network);
        loop {
            root_alive(child)?;
            tokio::select! {
                result = &mut network => {
                    result?;
                    return root_alive(child);
                }
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    })
    .await
}

/// Caller owns the absolute deadline and execution-identity observations.
pub(crate) async fn network(spec: &ServiceSpec, probe: StartupProbe) -> Result<()> {
    anyhow::ensure!(
        probe != StartupProbe::Process,
        "process probe cannot use a network endpoint"
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .context("build startup HTTP client")?;
    loop {
        let ready = match probe {
            StartupProbe::Http => client
                .get(format!(
                    "http://127.0.0.1:{}{}",
                    spec.port, spec.health.readiness_path
                ))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success()),
            StartupProbe::Tcp => tokio::net::TcpStream::connect(("127.0.0.1", spec.port))
                .await
                .is_ok(),
            StartupProbe::Process => bail!("process probe cannot use a network endpoint"),
        };
        if ready {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::process_tree::{StopOutcome, spawn_managed};

    fn worker() -> ServiceSpec {
        let mut lock = workspace_manifest::load_release_lock(include_str!(
            "../../workspace-manifest/tests/fixtures/lock_v1.toml"
        ))
        .unwrap();
        let mut spec = lock.services.remove(0);
        spec.kind = workspace_manifest::ProjectKind::Worker;
        spec.proxy = None;
        spec.health = workspace_manifest::HealthSection {
            startup_probe: Some(StartupProbe::Process),
            ..Default::default()
        };
        spec
    }
    fn child(script: &str) -> ManagedChild {
        let python = if cfg!(windows) { "python" } else { "python3" };
        let mut command = tokio::process::Command::new(python);
        command
            .args(["-c", script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        spawn_managed(command).unwrap()
    }
    async fn stop(child: &mut ManagedChild) {
        assert!(matches!(
            child.stop(Duration::from_secs(1)).await,
            StopOutcome::Graceful(_) | StopOutcome::Forced(_)
        ));
    }

    #[tokio::test]
    async fn process_start_requires_owned_root_and_is_cancellable() {
        let spec = worker();
        let mut running = child("import time; time.sleep(30)");
        let started = Instant::now();
        builtin(&spec, &mut running, true, started, None)
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(5));
        stop(&mut running).await;
        assert!(
            root_alive(&mut running).is_err(),
            "commit recheck must reject a departed root"
        );

        for script in [
            "raise SystemExit(0)",
            "raise SystemExit(9)",
            "import subprocess,sys; subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']); raise SystemExit(0)",
        ] {
            let mut exiting = child(script);
            let result = builtin(&spec, &mut exiting, true, Instant::now(), None).await;
            stop(&mut exiting).await;
            assert!(
                format!("{:#}", result.unwrap_err()).contains("root exited"),
                "{script}"
            );
        }
        let mut cancelled = child("import time; time.sleep(30)");
        let token = CancellationToken::new();
        token.cancel();
        let result = builtin(&spec, &mut cancelled, false, Instant::now(), Some(&token)).await;
        stop(&mut cancelled).await;
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn explicit_http_overrides_dev_tcp_without_silent_fallback() {
        // TCP accepts connections but never serves HTTP, reproducing a misleading
        // devrun port-only success. All modes observe the same live owned process.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut spec = worker();
        spec.port = listener.local_addr().unwrap().port();
        spec.health.startup_timeout_seconds = 1;
        spec.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["unused".into()],
        });
        let mut running = child("import time; time.sleep(30)");
        for probe in [None, Some(StartupProbe::Tcp), Some(StartupProbe::Http)] {
            spec.health.startup_probe = probe;
            let result = builtin(&spec, &mut running, true, Instant::now(), None).await;
            assert_eq!(
                result.is_ok(),
                probe != Some(StartupProbe::Http),
                "{probe:?}: {result:?}"
            );
        }
        stop(&mut running).await;
    }
}
