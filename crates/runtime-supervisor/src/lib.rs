//! Independent, same-binary supervision for one CLI scope.
//!
//! This crate proves local process quiescence. It never authorizes replay of a
//! database migration or claims that a remote operation completed successfully.
mod cleanup;
mod control;
pub mod domain;
mod epoch;
mod guardian;
mod monitor;
mod record;
mod recovery;
mod session;
mod worker;

pub use cleanup::{CleanupCommand, CleanupOutcome};
pub use control::{
    Action, Binding, FailureCode, Phase, Problem, RecoveryError, Request, Snapshot, control,
    control_verified, last_snapshot,
};
pub use monitor::{Options, Owner, Policy};
pub use record::{Intent, Quiescence, verify_live, verify_local_quiescent, verify_quiescent};
pub use recovery::{
    StopMode, StopRefused, StopWorkAttempt, continue_stop_work, continue_stop_work_with_checkpoint,
    continue_stop_work_with_cleanup, is_stop_refused, prepare_stop_work, stop_work,
    stop_work_with_cleanup,
};
pub use session::{
    BusinessFactory, BusinessLaunch, BusinessRun, FenceState, OwnerSession, SessionOptions,
};
pub use worker::{Worker, WorkerControl};

pub const WORKER_ENV: &str = "RCODER_SUPERVISOR_WORKER";
pub const TOKEN_ENV: &str = "RCODER_SUPERVISOR_TOKEN";
const GUARDIAN_ARG: &str = "--runtime-worker-guardian";

/// Retain for the entire external cleanup. A replacement cannot publish
/// quiescence while an older cleanup callback can still mutate the engine.
#[must_use]
pub struct CleanupGuard {
    _lock: std::fs::File,
}

/// Validate a cleanup-only callback launched by the guardian retaining the
/// original generation lock. This grants no right to spawn business commands.
pub fn verify_cleanup_callback() -> anyhow::Result<CleanupGuard> {
    use anyhow::{Context, ensure};
    let root = std::env::var_os("RCODER_SUPERVISOR_CLEANUP_ROOT")
        .context("cleanup callback root missing")?;
    let root = std::path::Path::new(&root);
    let lock = record::lock(&root.join("external-cleanup.lock"))?;
    let value = record::generation(root)?;
    ensure!(
        value.phase == record::GenerationPhase::Draining,
        "cleanup callback generation is not draining"
    );
    ensure!(
        std::env::var("RCODER_SUPERVISOR_CLEANUP_TOKEN")
            .ok()
            .as_deref()
            == Some(value.token.as_str()),
        "cleanup callback identity mismatch"
    );
    record::is_locked(&root.join("generation.lock"))?;
    Ok(CleanupGuard { _lock: lock })
}

/// Supervisors and short-lived control clients do little work. Do not allocate
/// one scheduler thread per CPU for each additional parent process. The actual
/// business worker keeps Tokio's normal sizing; this hint grants no authority.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if std::env::var_os(WORKER_ENV).is_none() {
        builder.worker_threads(2);
    }
    builder.enable_all().build()
}

/// Dispatch before argument parsing or initializing the business runtime.
/// Helpers use one lightweight executor, regardless of the machine's CPU count.
pub fn auxiliary_entry() -> Option<anyhow::Result<i32>> {
    let mut args = std::env::args_os().skip(1);
    let entry = args.next()?;
    if entry != GUARDIAN_ARG && entry != "--native-command-guardian" {
        return None;
    }
    Some((|| {
        let root = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("guardian root missing"))?;
        anyhow::ensure!(args.next().is_none(), "unexpected guardian argument");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            if entry == GUARDIAN_ARG {
                guardian::run(std::path::Path::new(&root)).await
            } else {
                process_utils::guardian::run(std::path::Path::new(&root)).await
            }
        });
        // Tokio stdin uses a blocking read that select! cannot cancel. All
        // owned children are already settled here; do not let runtime Drop
        // wait for the still-open parent lease before returning the exit code.
        runtime.shutdown_background();
        result
    })())
}
