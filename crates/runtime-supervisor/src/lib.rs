//! Independent, same-binary supervision for one CLI scope.
//!
//! This crate proves local process quiescence. It never authorizes replay of a
//! database migration or claims that a remote operation completed successfully.
mod control;
mod guardian;
mod monitor;
mod record;
mod worker;

pub use control::{Action, Binding, Request, Snapshot, control, last_snapshot};
pub use monitor::{Options, Owner, Policy};
pub use record::{Intent, Quiescence, verify_live, verify_quiescent};
pub use worker::{Worker, WorkerControl};

pub const WORKER_ENV: &str = "RCODER_SUPERVISOR_WORKER";
pub const TOKEN_ENV: &str = "RCODER_SUPERVISOR_TOKEN";
const GUARDIAN_ARG: &str = "--runtime-worker-guardian";

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
