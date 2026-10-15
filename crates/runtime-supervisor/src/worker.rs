use crate::{
    TOKEN_ENV, WORKER_ENV, control,
    record::{self, Generation, GenerationPhase, Intent},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::net::TcpListener;

/// Implemented by the CLI, through its actual admission/cancellation path.
#[async_trait::async_trait]
pub trait WorkerControl: Send + Sync + 'static {
    async fn probe(&self) -> Result<()>;
    /// Reversible drain before native StopWork/Recover closes admission or
    /// cancels the business. Owner Shutdown deliberately bypasses this check.
    async fn prepare_stop(&self, _deadline: tokio::time::Instant) -> Result<()> {
        Ok(())
    }
    fn stop_error_is_uncertain(&self, error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    }
    fn stop_prepare_budget(&self) -> Duration {
        self.shutdown_grace()
    }
    async fn shutdown(&self) -> Result<()>;
    /// Management initialization/cleanup readiness, never business readiness.
    fn ready(&self) -> bool {
        true
    }
    /// Existing engine cleanup budget, negotiated once on graceful shutdown.
    fn shutdown_grace(&self) -> Duration {
        Duration::from_secs(10)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Endpoint {
    pub generation: String,
    pub address: String,
    pub token: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Challenge {
    version: u32,
    generation: String,
    token: String,
    nonce: String,
    stop: bool,
}
#[derive(Serialize, Deserialize)]
struct Answer {
    generation: String,
    nonce: String,
    error: Option<String>,
    ready: bool,
    shutdown_grace_ms: u64,
}

pub(crate) struct Observation {
    pub ready: bool,
    pub shutdown_grace: Duration,
}

pub struct Worker {
    root: PathBuf,
    record: Generation,
}
impl Worker {
    /// Environment variables select a candidate; held execution authorization,
    /// generation identity, and a live supervisor must all agree before bypass.
    ///
    /// 启动观察（收据可见性，2026-09-28）：monitor/guardian 在 spawn 本进程
    /// 前数十毫秒内刚写收据与 supervisor.json，共享挂载上可能短暂不可见。
    /// 每轮 attempt 完整重查（resolve/收据/PID/scope/token/phase/锁/父身份/
    /// 准入），仅 NotFound 与准入锁短暂忙碌在单一预算内重试；身份不符立即
    /// 拒绝；不拼接两轮观察放行。Gate 用 try_acquire（无内建等待），避免
    /// 预算叠加。
    pub async fn from_env(scope: &Path) -> Result<Option<Self>> {
        let Some(path) = std::env::var_os(WORKER_ENV) else {
            return Ok(None);
        };
        let worker =
            process_utils::observe::observe("worker authorization", Duration::from_secs(3), || {
                let root_env = PathBuf::from(&path);
                let scope = scope.to_path_buf();
                async move {
                    let root = std::fs::canonicalize(&root_env).context("resolve worker scope")?;
                    let scope =
                        std::fs::canonicalize(&scope).context("resolve supervisor scope")?;
                    let record = record::generation(&root)?;
                    // The guardian persists the actual child PID immediately after
                    // spawn; until then the observation is not ready. A copied
                    // environment cannot authorize a second worker for this scope.
                    let Some(worker_pid) = record.worker_pid else {
                        return Ok(None);
                    };
                    ensure!(
                        worker_pid == std::process::id(),
                        "worker is not the guardian's authorized child"
                    );
                    ensure!(
                        record::work_root(&scope, &record.id)? == root,
                        "worker belongs to a different scope"
                    );
                    ensure!(
                        std::env::var(TOKEN_ENV).ok().as_deref() == Some(&record.token),
                        "invalid worker launch token"
                    );
                    ensure!(
                        record.phase == GenerationPhase::Running,
                        "worker launch is not active"
                    );
                    record::is_locked(&root.join("generation.lock"))?;
                    record::is_locked(&scope.join("owner.lock"))?;
                    let parent: control::Discovery = record::read(&scope.join("supervisor.json"))?;
                    ensure!(
                        parent.instance == record.supervisor
                            && parent.snapshot.generation.as_deref() == Some(&record.id),
                        "worker launch belongs to a retired supervisor"
                    );
                    // 准入：try_acquire（忙碌=瞬态）+ require_open（记录未可见=瞬态）；
                    // 每轮重建并释放，不携带跨轮句柄。
                    let gate = process_utils::command_authority::Gate::try_acquire(&root)?;
                    gate.require_open()?;
                    drop(gate);
                    Ok(Some(Self { root, record }))
                }
            })
            .await?;
        Ok(Some(worker))
    }
    pub fn generation(&self) -> &str {
        &self.record.id
    }
    pub fn supervisor_id(&self) -> &str {
        &self.record.supervisor
    }
    pub fn work_root(&self) -> &Path {
        &self.root
    }
    pub fn intent(&self) -> Intent {
        self.record.intent
    }

    /// Call after the public listener binds, before starting business work.
    /// Dropping the returned task aborts control publication explicitly via
    /// its guard; the parent observes loss and performs bounded recovery.
    pub async fn serve(self, adapter: Arc<dyn WorkerControl>) -> Result<WorkerServer> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = Endpoint {
            generation: self.record.id.clone(),
            address: listener.local_addr()?.to_string(),
            token: self.record.token.clone(),
        };
        record::save(&self.root.join("worker.json"), &endpoint)?;
        let task = tokio::spawn(async move {
            let slots = Arc::new(tokio::sync::Semaphore::new(8));
            loop {
                let (mut stream, _) = listener.accept().await?;
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let adapter = adapter.clone();
                let id = self.record.id.clone();
                let token = self.record.token.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let result = async {
                        let request: Challenge = control::receive(&mut stream).await?;
                        ensure!(
                            request.version == 1
                                && request.generation == id
                                && request.token == token,
                            "worker challenge identity mismatch"
                        );
                        let outcome = tokio::time::timeout(Duration::from_secs(3), async {
                            if request.stop {
                                adapter.shutdown().await
                            } else {
                                adapter.probe().await
                            }
                        })
                        .await
                        .context("worker control path did not respond")
                        .and_then(|r| r);
                        control::send(
                            &mut stream,
                            &Answer {
                                generation: id,
                                nonce: request.nonce,
                                error: outcome.err().map(|e| format!("{e:#}")),
                                ready: adapter.ready(),
                                shutdown_grace_ms: u64::try_from(
                                    adapter.shutdown_grace().as_millis(),
                                )
                                .unwrap_or(u64::MAX),
                            },
                        )
                        .await
                    }
                    .await;
                    if let Err(error) = result {
                        tracing::debug!(%error, "worker control connection ended");
                    }
                });
            }
            // loop 无 break：accept 循环即块尾表达式（never 类型收敛到 Result<()>），
            // accept 出错经 `?` 直接以 Err 结束本任务。
        });
        Ok(WorkerServer { task })
    }
}
pub struct WorkerServer {
    task: tokio::task::JoinHandle<Result<()>>,
}
impl Drop for WorkerServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn challenge(root: &Path, stop: bool) -> Result<Observation> {
    let generation = record::generation(root)?;
    let endpoint: Endpoint = record::read(&root.join("worker.json"))?;
    ensure!(
        endpoint.generation == generation.id && endpoint.token == generation.token,
        "worker endpoint identity mismatch"
    );
    let mut stream = control::connect(&endpoint.address).await?;
    let nonce = uuid::Uuid::new_v4().to_string();
    control::send(
        &mut stream,
        &Challenge {
            version: 1,
            generation: generation.id.clone(),
            token: endpoint.token,
            nonce: nonce.clone(),
            stop,
        },
    )
    .await?;
    let response: Answer = control::receive(&mut stream).await?;
    ensure!(
        response.generation == generation.id && response.nonce == nonce,
        "stale worker challenge reply"
    );
    if let Some(error) = response.error {
        anyhow::bail!("{error}");
    }
    Ok(Observation {
        ready: response.ready,
        shutdown_grace: Duration::from_millis(response.shutdown_grace_ms),
    })
}
