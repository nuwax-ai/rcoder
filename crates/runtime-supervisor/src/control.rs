use crate::record::{self, Intent};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{path::Path, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub component: String,
    pub resource: std::path::PathBuf,
}
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};
pub(crate) const CONTROL_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Status,
    Recover,
    StopWork,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub request_id: String,
    pub action: Action,
    pub expected_generation: Option<String>,
}
impl Request {
    pub fn new(action: Action) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            action,
            expected_generation: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Reconciling,
    Starting,
    Ready,
    Stopping,
    CleanupPending,
    RecoveryRequired,
    Stopped,
}
impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Reconciling => "reconciling",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::CleanupPending => "cleanup_pending",
            Self::RecoveryRequired => "recovery_required",
            Self::Stopped => "stopped",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    Busy,
    ProtocolMismatch,
    IdentityChanged,
    InvalidRequest,
    CleanupUnconfirmed,
    CleanupInProgress,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub code: FailureCode,
    pub message: String,
}
impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}
impl std::error::Error for Problem {}
impl Problem {
    pub(crate) fn from_error(error: &anyhow::Error) -> Self {
        error
            .downcast_ref::<Self>()
            .cloned()
            .unwrap_or_else(|| Self {
                code: FailureCode::Internal,
                message: format!("{error:#}"),
            })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub binding: Binding,
    pub supervisor_id: String,
    pub generation: Option<String>,
    pub phase: Phase,
    pub intent: Intent,
    pub operation_id: Option<String>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<Problem>,
}
impl Snapshot {
    pub fn recovery_error(self) -> anyhow::Error {
        RecoveryError {
            snapshot: Box::new(self),
        }
        .into()
    }
    /// Diagnostic context is also useful when the public business API is absent.
    pub fn diagnostic(&self) -> String {
        format!(
            "supervisor={} generation={} operation={} stage={}: {}",
            self.supervisor_id,
            self.generation.as_deref().unwrap_or("none"),
            self.operation_id.as_deref().unwrap_or("none"),
            self.phase,
            self.problem
                .as_ref()
                .map(|p| p.to_string())
                .or_else(|| self.error.clone())
                .unwrap_or_else(|| "management recovery in progress".into())
        )
    }
}
#[derive(Debug)]
pub struct RecoveryError {
    pub snapshot: Box<Snapshot>,
}
impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.snapshot.diagnostic())
    }
}
impl std::error::Error for RecoveryError {}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Discovery {
    pub version: u32,
    pub instance: String,
    pub address: String,
    pub token: String,
    pub snapshot: Snapshot,
    pub requests: Vec<(Request, Snapshot)>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub version: u32,
    pub instance: String,
    pub token: String,
    pub request: Request,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Reply {
    pub instance: String,
    pub snapshot: Snapshot,
    pub error: Option<Problem>,
}

pub(crate) async fn receive<T: DeserializeOwned>(stream: &mut TcpStream) -> Result<T> {
    let mut bytes = Vec::new();
    let mut reader = BufReader::new(stream.take(32 * 1024 + 1));
    tokio::time::timeout(Duration::from_secs(3), reader.read_until(b'\n', &mut bytes)).await??;
    ensure!(
        bytes.len() <= 32 * 1024 && bytes.last() == Some(&b'\n'),
        "invalid supervisor frame"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
pub(crate) async fn send<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() < 32 * 1024, "supervisor response too large");
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(3), stream.write_all(&bytes)).await??;
    Ok(())
}
pub(crate) async fn connect(address: &str) -> Result<TcpStream> {
    let addr: std::net::SocketAddr = address.parse()?;
    ensure!(
        addr.ip().is_loopback(),
        "supervisor control must use loopback"
    );
    Ok(tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await??)
}
pub async fn control(root: &Path, request: Request) -> Result<Snapshot> {
    let discovery: Discovery = record::read(&root.join("supervisor.json"))?;
    if discovery.version != CONTROL_VERSION {
        return Err(Problem {
            code: FailureCode::ProtocolMismatch,
            message: "unsupported supervisor control protocol; restart with matching CLI".into(),
        }
        .into());
    }
    record::is_locked(&root.join("owner.lock"))?;
    let mut stream = connect(&discovery.address)
        .await
        .context("connect independent supervisor")?;
    send(
        &mut stream,
        &Envelope {
            version: CONTROL_VERSION,
            instance: discovery.instance.clone(),
            token: discovery.token,
            request,
        },
    )
    .await?;
    let reply: Reply = receive(&mut stream).await?;
    ensure!(
        reply.instance == discovery.instance,
        "supervisor reply identity mismatch"
    );
    if let Some(error) = reply.error {
        if error.code == FailureCode::Busy {
            let mut snapshot = reply.snapshot;
            snapshot.problem = Some(error.clone());
            return Err(anyhow::Error::new(error).context(RecoveryError {
                snapshot: Box::new(snapshot),
            }));
        }
        return Err(error.into());
    }
    Ok(reply.snapshot)
}

/// Durable observation only. Callers must separately verify process cleanup
/// before interpreting an offline snapshot as a completed shutdown.
pub fn last_snapshot(root: &Path) -> Result<Snapshot> {
    let value: Discovery = record::read(&root.join("supervisor.json"))?;
    ensure!(
        matches!(value.version, 1 | CONTROL_VERSION),
        "unsupported supervisor receipt"
    );
    Ok(value.snapshot)
}
