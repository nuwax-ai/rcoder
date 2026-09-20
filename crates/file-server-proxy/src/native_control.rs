//! Standalone owner protocol. PID is never an authorization credential.
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

const VERSION: u32 = 1;
const MAX_FRAME: u64 = 8192;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    version: u32,
    instance_id: String,
    token: String,
    control_address: String,
    pub address: String,
    pub phase: String,
    #[serde(default)]
    supervisor_id: Option<String>,
    #[serde(default)]
    retirement_requested: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    instance_id: String,
    token: String,
    action: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    version: u32,
    instance_id: String,
    phase: String,
    address: String,
    error: Option<String>,
}

pub fn directory(host: &str, port: u16) -> Result<PathBuf, String> {
    let root = std::env::var_os("FILE_SERVER_PROXY_STATE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .filter(|v| !v.is_empty())
                .map(|p| PathBuf::from(p).join(".file-server-proxy"))
        })
        .ok_or("native control requires a stable state directory")?;
    let host_key: String = host.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    Ok(root.join(format!("native-{host_key}-{port}")))
}
fn read(root: &Path) -> Result<Option<Receipt>, String> {
    let bytes = match std::fs::read(root.join("owner.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read owner receipt: {error}")),
    };
    let receipt: Receipt = serde_json::from_slice(&bytes)
        .map_err(|e| format!("invalid owner receipt (preserved): {e}"))?;
    if receipt.version != VERSION
        || receipt.instance_id.is_empty()
        || receipt.token.len() < 32
        || !matches!(
            receipt.phase.as_str(),
            "Starting" | "Running" | "Stopping" | "Stopped"
        )
    {
        return Err("incompatible owner receipt (preserved)".into());
    }
    let address: std::net::SocketAddr = receipt
        .control_address
        .parse()
        .map_err(|_| "invalid owner control address")?;
    if !address.ip().is_loopback() {
        return Err("owner control address is not loopback".into());
    }
    Ok(Some(receipt))
}
fn write(root: &Path, receipt: &Receipt) -> Result<(), String> {
    use std::io::Write;
    let mut file =
        tempfile::NamedTempFile::new_in(root).map_err(|e| format!("create owner receipt: {e}"))?;
    serde_json::to_writer(&mut file, receipt).map_err(|e| format!("encode owner receipt: {e}"))?;
    file.flush()
        .and_then(|()| file.as_file().sync_all())
        .map_err(|e| format!("sync owner receipt: {e}"))?;
    file.persist(root.join("owner.json"))
        .map_err(|e| format!("publish owner receipt: {e}"))?;
    #[cfg(unix)]
    File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("sync owner directory: {e}"))?;
    Ok(())
}
async fn frame(stream: &mut tokio::net::TcpStream) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let mut reader = BufReader::new(stream.take(MAX_FRAME + 1));
    tokio::time::timeout(Duration::from_secs(5), reader.read_until(b'\n', &mut bytes))
        .await
        .map_err(|_| "control frame deadline exceeded")?
        .map_err(|e| format!("read control frame: {e}"))?;
    if bytes.len() as u64 > MAX_FRAME || bytes.last() != Some(&b'\n') {
        return Err("invalid control frame length".into());
    }
    Ok(bytes)
}
async fn send<T: Serialize>(stream: &mut tokio::net::TcpStream, value: &T) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| format!("encode control reply: {e}"))?;
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), stream.write_all(&bytes))
        .await
        .map_err(|_| "control write deadline exceeded")?
        .map_err(|e| format!("write control reply: {e}"))
}

pub struct Owner {
    _lock: File,
    root: PathBuf,
    receipt: Receipt,
    listener: tokio::net::TcpListener,
    ts_child: Option<process_utils::guardian::OwnedChild>,
    #[cfg(feature = "embed-file-server")]
    embedded: Option<file_server_userapp::EmbeddedRuntimeHandle>,
}
impl Owner {
    pub async fn acquire(root: PathBuf) -> Result<Self, String> {
        process_utils::command_context::create_durable_directory(&root)
            .map_err(|e| format!("create owner directory: {e}"))?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("owner.lock"))
            .map_err(|e| format!("open owner lock: {e}"))?;
        lock.try_lock()
            .map_err(|e| format!("native owner already held or inaccessible: {e}"))?;
        if read(&root)?.is_some_and(|r| r.phase != "Stopped") {
            return Err("previous owner outcome unknown; receipt preserved, explicit reconciliation required".into());
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind native control: {e}"))?;
        let receipt = Receipt {
            version: VERSION,
            instance_id: match std::env::var("FILE_SERVER_PROXY_LAUNCH_ID") {
                Ok(value) => uuid::Uuid::parse_str(&value)
                    .map_err(|_| "invalid native launch identity")?
                    .to_string(),
                Err(std::env::VarError::NotPresent) => uuid::Uuid::new_v4().to_string(),
                Err(error) => return Err(format!("read launch identity: {error}")),
            },
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            control_address: listener
                .local_addr()
                .map_err(|e| e.to_string())?
                .to_string(),
            address: String::new(),
            phase: "Starting".into(),
            supervisor_id: std::env::var("FILE_SERVER_PROXY_OWNER_SUPERVISOR").ok(),
            retirement_requested: false,
        };
        if let Some(id) = &receipt.supervisor_id {
            crate::native_supervisor::verify_live(&root, id, &receipt.instance_id)?;
        }
        write(&root, &receipt)?;
        Ok(Self {
            _lock: lock,
            root,
            receipt,
            listener,
            ts_child: None,
            #[cfg(feature = "embed-file-server")]
            embedded: None,
        })
    }
    pub fn work_root(&self) -> PathBuf {
        self.root.join("work").join(&self.receipt.instance_id)
    }
    #[cfg(feature = "embed-file-server")]
    pub fn set_embedded(&mut self, handle: file_server_userapp::EmbeddedRuntimeHandle) {
        self.embedded = Some(handle);
    }
    pub async fn start_ts(&mut self, node: &Path, entry: &Path) -> Result<u16, String> {
        if !node.is_absolute() || !entry.is_absolute() || !node.is_file() || !entry.is_file() {
            return Err("managed TS requires existing absolute Node and server entry paths".into());
        }
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let port = reservation.local_addr().map_err(|e| e.to_string())?.port();
        let mut command = tokio::process::Command::new(node);
        command
            .arg(entry)
            .env("PORT", port.to_string())
            .env("NODE_ENV", "production");
        command.stdin(std::process::Stdio::null());
        drop(reservation);
        self.ts_child = Some(
            if self.receipt.supervisor_id.is_some() {
                process_utils::guardian::spawn_guarded(command, &self.work_root(), None, false)
                    .await
            } else {
                process_utils::managed_tree::spawn_managed(command)
                    .map(process_utils::guardian::OwnedChild::Direct)
            }
            .map_err(|e| format!("spawn owned TS: {e}"))?,
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            let child = self.ts_child.as_mut().ok_or("owned TS handle missing")?;
            if child.try_wait_root().map_err(|e| e.to_string())?.is_some() {
                return Err("owned TS exited before readiness".into());
            }
            let probe = async {
                let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
                stream
                    .write_all(
                        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
                let mut line = String::new();
                use tokio::io::AsyncReadExt;
                BufReader::new(stream.take(1024))
                    .read_line(&mut line)
                    .await?;
                Ok::<bool, std::io::Error>(line.split_whitespace().nth(1) == Some("200"))
            };
            if matches!(
                tokio::time::timeout(Duration::from_secs(1), probe).await,
                Ok(Ok(true))
            ) {
                // A listener is readiness evidence only; ownership remains the
                // retained child tree handle, never this port or TS's PID file.
                if child.try_wait_root().map_err(|e| e.to_string())?.is_none() {
                    return Ok(port);
                }
                return Err("owned TS exited during readiness".into());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("owned TS readiness deadline exceeded".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    pub fn started(&mut self, address: String) -> Result<(), String> {
        self.receipt.address = address;
        self.receipt.phase = "Running".into();
        write(&self.root, &self.receipt)
    }
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<(), String> {
        let mut shutdown_observed = false;
        let mut fatal_error: Option<String> = None;
        let mut cleanup_retry = tokio::time::interval(Duration::from_secs(1));
        let mut accept_after = tokio::time::Instant::now();
        let mut observation = tokio::time::interval(Duration::from_millis(200));
        loop {
            tokio::select! {
                _ = cleanup_retry.tick(), if fatal_error.is_some() => {
                    match self.stop().await {
                        Ok(()) => return Err(fatal_error.take().unwrap_or_default()),
                        Err(error) => tracing::error!(%error, "native failure cleanup unconfirmed; retaining owner and retrying"),
                    }
                }
                _ = observation.tick(), if fatal_error.is_none() => {
                    if let Some(child) = &mut self.ts_child {
                        match child.try_wait_root() {
                            Ok(None) => {}
                            result => {
                                fatal_error = Some(format!("owned TS stopped unexpectedly: {result:?}"));
                                // Do not drop the retained child tree when cleanup fails.
                                // The retry branch owns the same owner and handles.
                                self.receipt.phase = "Stopping".into();
                            }
                        }
                    }
                }
                () = shutdown.cancelled(), if !shutdown_observed => {
                    shutdown_observed = true;
                    match self.stop().await {
                        Ok(()) => return Ok(()),
                        Err(error) => tracing::error!(%error, "native shutdown remains protected; retaining owner for explicit retry"),
                    }
                }
                connection = async {
                    tokio::time::sleep_until(accept_after).await;
                    self.listener.accept().await
                } => {
                    let (mut stream, _) = match connection {
                        Ok(connection) => connection,
                        Err(error) => {
                            fatal_error.get_or_insert_with(|| format!("accept control: {error}"));
                            accept_after = tokio::time::Instant::now() + Duration::from_secs(1);
                            continue;
                        }
                    };
                    let request = match frame(&mut stream).await.and_then(|b| serde_json::from_slice::<Request>(&b).map_err(|e| e.to_string())) {
                        Ok(request) => request,
                        Err(_) => continue,
                    };
                    if request.version != VERSION || request.instance_id != self.receipt.instance_id || request.token != self.receipt.token {
                        continue;
                    }
                    let result = match request.action.as_str() {
                        "status" => Ok(()),
                        "stop" => self.stop().await,
                        "retire" => self.prepare_retirement(),
                        _ => Err("unsupported owner command".into()),
                    };
                    let stopped = request.action == "stop" && result.is_ok();
                    let retiring = request.action == "retire" && result.is_ok();
                    let reply = Reply { version: VERSION, instance_id: self.receipt.instance_id.clone(),
                        phase: if retiring { "RetirementAccepted".into() } else { self.receipt.phase.clone() }, address: self.receipt.address.clone(), error: result.err() };
                    // A lost response does not undo the durable completion receipt.
                    let _delivery = send(&mut stream, &reply).await;
                    if retiring {
                        let _flushed = tokio::time::timeout(Duration::from_secs(5), stream.shutdown()).await;
                        // Intentionally no Stopped/worker completion write. Only
                        // the outer supervisor's actual wait proves this exit.
                        std::process::exit(75);
                    }
                    if stopped { return Ok(()); }
                }
            }
        }
    }
    fn prepare_retirement(&mut self) -> Result<(), String> {
        let supervisor = self
            .receipt
            .supervisor_id
            .as_deref()
            .ok_or("retirement requires a real owner supervisor")?;
        crate::native_supervisor::verify_live(&self.root, supervisor, &self.receipt.instance_id)?;
        #[cfg(feature = "embed-file-server")]
        if let Some(handle) = &self.embedded {
            handle.close()?;
        }
        self.receipt.phase = "Stopping".into();
        self.receipt.retirement_requested = true;
        write(&self.root, &self.receipt)
    }

    pub async fn stop(&mut self) -> Result<(), String> {
        self.receipt.phase = "Stopping".into();
        let persistence = write(&self.root, &self.receipt);
        #[cfg(feature = "embed-file-server")]
        let admission = self
            .embedded
            .as_ref()
            .map(|handle| handle.close())
            .transpose();
        let proxy = file_server_proxy::stop().await;
        let result = match (persistence, proxy) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(first), Err(second)) => Err(format!("{first}; {second}")),
            (Err(error), _) | (_, Err(error)) => Err(error),
        };
        #[cfg(feature = "embed-file-server")]
        let result = match (result, admission) {
            (Ok(()), Ok(_)) => Ok(()),
            (Err(a), Err(b)) => Err(format!("{a}; {b}")),
            (Err(error), _) | (_, Err(error)) => Err(error),
        };
        self.finish_stop(result).await
    }

    async fn finish_stop(&mut self, proxy_result: Result<(), String>) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Err(error) = proxy_result {
            errors.push(error);
        }
        #[cfg(feature = "embed-file-server")]
        if let Some(handle) = &self.embedded
            && let Err(error) = handle
                .shutdown(tokio::time::Instant::now() + Duration::from_secs(30))
                .await
        {
            errors.push(error);
        }
        if let Some(child) = &mut self.ts_child {
            if matches!(
                child.stop(Duration::from_secs(3)).await,
                process_utils::managed_tree::StopOutcome::Unconfirmed
            ) {
                errors.push("owned TS process tree shutdown unconfirmed".into());
            } else {
                self.ts_child.take();
            }
        }
        if !errors.is_empty() {
            self.receipt.phase = "Stopping".into();
            if let Err(error) = write(&self.root, &self.receipt) {
                errors.push(error);
            }
            return Err(errors.join("; "));
        }
        self.receipt.phase = "Stopped".into();
        write(&self.root, &self.receipt)
    }
}

async fn confirm_stopped(root: &Path, instance_id: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("owner.lock"))
            .map_err(|e| format!("open completion lock: {e}"))?;
        match lock.try_lock() {
            Ok(()) => {
                let current = read(root)?.ok_or("completed owner receipt disappeared")?;
                if current.instance_id != instance_id || current.phase != "Stopped" {
                    return Err(
                        "completed owner has been replaced; refusing stale completion".into(),
                    );
                }
                return Ok(());
            }
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(format!("owner release unconfirmed: {error}")),
        }
    }
}

fn recover(root: &Path, expected_instance: Option<&str>) -> Result<String, String> {
    let expected = expected_instance
        .ok_or("recover requires --instance-id from the protected original owner")?;
    let lock = File::options()
        .read(true)
        .write(true)
        .open(root.join("owner.lock"))
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|e| format!("owner remains active or inaccessible: {e}"))?;
    let mut receipt = read(root)?.ok_or("original owner receipt missing")?;
    if receipt.instance_id != expected {
        return Err("recover identity differs from original owner".into());
    }
    if receipt.phase == "Stopped" {
        return serde_json::to_string(&Reply {
            version: VERSION,
            instance_id: receipt.instance_id,
            phase: receipt.phase,
            address: receipt.address,
            error: None,
        })
        .map_err(|e| e.to_string());
    }
    let supervisor = receipt
        .supervisor_id
        .as_deref()
        .ok_or("original owner lacks a real exit witness; recovery remains protected")?;
    crate::native_supervisor::verify_exited(root, supervisor, expected)?;
    let work = root.join("work").join(expected);
    process_utils::guardian::recover(&work).map_err(|e| format!("guardian recovery: {e:#}"))?;
    process_utils::command_context::require_quiescent(&work.join("commands"))
        .map_err(|e| e.to_string())?;
    // OwnerExited is a wait() receipt for this exact process, so its blocking
    // and async workers cannot still be executing. Preserve each record and
    // identity, marking interruption rather than inventing business success.
    let workers = work.join("workers");
    if workers.try_exists().map_err(|e| e.to_string())? {
        for entry in std::fs::read_dir(&workers).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if value["version"] != 1
                || !matches!(
                    value["phase"].as_str(),
                    Some("SpawnPending" | "Running" | "Quiescent")
                )
            {
                return Err("worker receipt damaged/unknown; recovery protected".into());
            }
            if value["phase"] != "Quiescent" {
                value["phase"] = "Quiescent".into();
                value["termination"] = "OwnerExitedInterrupted".into();
                value["owner_instance_id"] = expected.into();
                value["supervisor_id"] = supervisor.into();
                use std::io::Write;
                let mut temp =
                    tempfile::NamedTempFile::new_in(&workers).map_err(|e| e.to_string())?;
                serde_json::to_writer(&mut temp, &value).map_err(|e| e.to_string())?;
                temp.flush()
                    .and_then(|()| temp.as_file().sync_all())
                    .map_err(|e| e.to_string())?;
                temp.persist(path).map_err(|e| e.to_string())?;
            }
        }
        #[cfg(unix)]
        File::open(&workers)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    receipt.phase = "Stopped".into();
    write(root, &receipt)?;
    serde_json::to_string(&Reply {
        version: VERSION,
        instance_id: receipt.instance_id,
        phase: receipt.phase,
        address: receipt.address,
        error: None,
    })
    .map_err(|e| e.to_string())
}

async fn retire_control(root: &Path, expected: Option<&str>) -> Result<String, String> {
    let expected = expected.ok_or("retire requires --instance-id of the original owner")?;
    let original = read(root)?.ok_or("original owner receipt missing")?;
    if original.instance_id != expected {
        return Err("retire identity differs from original owner".into());
    }
    let supervisor = original
        .supervisor_id
        .as_deref()
        .ok_or("original owner has no exit witness")?;
    let observe = || crate::native_supervisor::verify_exited(root, supervisor, expected);
    let already_exited = original.retirement_requested && observe().is_ok();
    if !already_exited {
        let request = async {
            let mut stream = tokio::net::TcpStream::connect(&original.control_address)
                .await
                .map_err(|e| e.to_string())?;
            send(
                &mut stream,
                &Request {
                    version: VERSION,
                    instance_id: expected.into(),
                    token: original.token.clone(),
                    action: "retire".into(),
                },
            )
            .await?;
            let reply: Reply =
                serde_json::from_slice(&frame(&mut stream).await?).map_err(|e| e.to_string())?;
            if reply.version != VERSION || reply.instance_id != expected {
                return Err("retire reply identity mismatch".into());
            }
            if let Some(error) = reply.error {
                return Err(error);
            }
            if reply.phase != "RetirementAccepted" {
                return Err("unexpected retire response".into());
            }
            Ok::<(), String>(())
        };
        // Delivery may be lost after durable acceptance. Inspect only the same
        // original retirement receipt and actual exit witness in that case.
        let delivery = tokio::time::timeout(Duration::from_secs(10), request).await;
        let current = read(root)?.ok_or("original retirement receipt missing")?;
        if current.instance_id != expected || !current.retirement_requested {
            return Err(format!("retirement not durably accepted: {delivery:?}"));
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if observe().is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("original owner exit unknown; retirement preserved".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    serde_json::to_string(&Reply {
        version: VERSION,
        instance_id: expected.into(),
        phase: "OwnerExited".into(),
        address: original.address,
        error: None,
    })
    .map_err(|e| e.to_string())
}

pub async fn control(
    root: &Path,
    action: &str,
    expected_instance: Option<&str>,
) -> Result<String, String> {
    if action == "recover" {
        return recover(root, expected_instance);
    }
    if action == "retire" {
        return retire_control(root, expected_instance).await;
    }

    if !matches!(action, "status" | "stop") {
        return Err("unsupported owner command".into());
    }
    let Some(receipt) = read(root)? else {
        if expected_instance.is_some() {
            return Err("expected owner receipt is missing; outcome unknown".into());
        }
        match std::fs::metadata(root.join("owner.lock")) {
            Ok(_) => return Err("owner lock exists without a receipt; outcome unknown".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect owner scope: {error}")),
        }
        return Ok("{\"phase\":\"Absent\"}".into());
    };
    if expected_instance.is_some_and(|expected| expected != receipt.instance_id) {
        return Err("owner identity differs from expected launch; refusing control".into());
    }
    if receipt.phase == "Stopped" {
        confirm_stopped(root, &receipt.instance_id).await?;
        return serde_json::to_string(&Reply {
            version: VERSION,
            instance_id: receipt.instance_id,
            phase: receipt.phase,
            address: receipt.address,
            error: None,
        })
        .map_err(|e| e.to_string());
    }
    let result = async {
        let mut stream = tokio::net::TcpStream::connect(&receipt.control_address)
            .await
            .map_err(|e| format!("owner unavailable; receipt preserved: {e}"))?;
        send(
            &mut stream,
            &Request {
                version: VERSION,
                instance_id: receipt.instance_id.clone(),
                token: receipt.token.clone(),
                action: action.into(),
            },
        )
        .await?;
        // stop may include the full existing proxy drain budget.
        let mut bytes = Vec::new();
        use tokio::io::AsyncReadExt;
        BufReader::new((&mut stream).take(MAX_FRAME + 1))
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_FRAME {
            return Err("oversized owner response".into());
        }
        let reply: Reply =
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid owner response: {e}"))?;
        if reply.version != VERSION || reply.instance_id != receipt.instance_id {
            return Err("owner response identity mismatch".into());
        }
        if let Some(error) = &reply.error {
            return Err(error.clone());
        }
        if action == "stop" && reply.phase != "Stopped" {
            return Err("owner shutdown unconfirmed".into());
        }
        if action == "stop" {
            confirm_stopped(root, &receipt.instance_id).await?;
        }
        serde_json::to_string(&reply).map_err(|e| e.to_string())
    };
    tokio::time::timeout(Duration::from_secs(60), result)
        .await
        .map_err(|_| "owner response unknown; receipt preserved".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn held_owner_and_unconfirmed_dead_owner_cannot_be_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Owner::acquire(dir.path().into()).await.unwrap();
        let before = std::fs::read(dir.path().join("owner.json")).unwrap();
        assert!(Owner::acquire(dir.path().into()).await.is_err());
        drop(owner);
        assert!(Owner::acquire(dir.path().into()).await.is_err());
        assert_eq!(
            before,
            std::fs::read(dir.path().join("owner.json")).unwrap()
        );
        assert!(control(dir.path(), "stop", None).await.is_err());
        assert_eq!(
            before,
            std::fs::read(dir.path().join("owner.json")).unwrap()
        );
    }

    #[tokio::test]
    async fn corrupt_and_incompatible_receipts_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        for bytes in [b"broken".as_slice(), br#"{"version":999}"#.as_slice()] {
            std::fs::write(dir.path().join("owner.json"), bytes).unwrap();
            assert!(Owner::acquire(dir.path().into()).await.is_err());
            assert!(control(dir.path(), "status", None).await.is_err());
            assert_eq!(std::fs::read(dir.path().join("owner.json")).unwrap(), bytes);
        }
    }

    #[tokio::test]
    async fn real_control_checks_identity_then_persists_stopped_before_reply() {
        let dir = tempfile::tempdir().unwrap();
        let mut owner = Owner::acquire(dir.path().into()).await.unwrap();
        owner.started("127.0.0.1:12345".into()).unwrap();
        let identity = owner.receipt.clone();
        let task = tokio::spawn(owner.run(CancellationToken::new()));
        for (instance_id, token) in [
            ("wrong".to_owned(), identity.token.clone()),
            (identity.instance_id.clone(), "wrong".to_owned()),
        ] {
            let mut client = tokio::net::TcpStream::connect(&identity.control_address)
                .await
                .unwrap();
            send(
                &mut client,
                &Request {
                    version: VERSION,
                    instance_id,
                    token,
                    action: "stop".into(),
                },
            )
            .await
            .unwrap();
            assert!(frame(&mut client).await.is_err());
            assert_eq!(read(dir.path()).unwrap().unwrap().phase, "Running");
        }
        assert!(
            control(dir.path(), "stop", Some("previous-owner-instance"))
                .await
                .is_err()
        );
        assert_eq!(read(dir.path()).unwrap().unwrap().phase, "Running");
        let status: serde_json::Value =
            serde_json::from_str(&control(dir.path(), "status", None).await.unwrap()).unwrap();
        assert_eq!(status["instance_id"], identity.instance_id);
        assert_eq!(status["phase"], "Running");
        let stopped: serde_json::Value =
            serde_json::from_str(&control(dir.path(), "stop", None).await.unwrap()).unwrap();
        assert_eq!(stopped["phase"], "Stopped");
        assert_eq!(read(dir.path()).unwrap().unwrap().phase, "Stopped");
        task.await.unwrap().unwrap();
        let next = Owner::acquire(dir.path().into()).await.unwrap();
        assert_ne!(next.receipt.instance_id, identity.instance_id);
        let mut next = next;
        next.started("127.0.0.1:23456".into()).unwrap();
        let successor = tokio::spawn(next.run(CancellationToken::new()));
        assert!(
            control(dir.path(), "stop", Some(&identity.instance_id))
                .await
                .is_err()
        );
        assert_eq!(read(dir.path()).unwrap().unwrap().phase, "Running");
        control(dir.path(), "stop", None).await.unwrap();
        successor.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn unexpected_ts_exit_keeps_owner_until_failed_cleanup_can_be_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        let mut owner = Owner::acquire(dir.path().into()).await.unwrap();
        owner.started("127.0.0.1:12345".into()).unwrap();
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command.arg("--list").stdout(std::process::Stdio::null());
        let mut child = process_utils::managed_tree::spawn_managed(command).unwrap();
        child.wait_root().await.unwrap();
        owner.ts_child = Some(process_utils::guardian::OwnedChild::Direct(child));
        // Make the actual durable stopping/completion write fail, after a real
        // owned root has exited. The owner must not return and drop its lock.
        std::fs::remove_file(dir.path().join("owner.json")).unwrap();
        std::fs::create_dir(dir.path().join("owner.json")).unwrap();
        let task = tokio::spawn(owner.run(CancellationToken::new()));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!task.is_finished(), "cleanup failure must retain the owner");
        let contender = File::options()
            .read(true)
            .write(true)
            .open(dir.path().join("owner.lock"))
            .unwrap();
        assert!(contender.try_lock().is_err());
        std::fs::remove_dir(dir.path().join("owner.json")).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(result.contains("owned TS stopped unexpectedly"));
        assert_eq!(read(dir.path()).unwrap().unwrap().phase, "Stopped");
        contender.try_lock().unwrap();
    }

    #[tokio::test]
    async fn managed_ts_child_tree_is_part_of_native_stop_receipt() {
        let node = std::process::Command::new("node")
            .args(["-p", "process.execPath"])
            .output()
            .unwrap();
        assert!(
            node.status.success(),
            "Node is required for npm compatibility contract"
        );
        let node = PathBuf::from(String::from_utf8(node.stdout).unwrap().trim());
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("server.js");
        std::fs::write(
            &script,
            r#"
const http = require('node:http');
const {spawn} = require('node:child_process');
spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], {stdio:'ignore'});
http.createServer((req,res)=>{res.writeHead(200);res.end('ok');})
.listen(Number(process.env.PORT), '127.0.0.1');
"#,
        )
        .unwrap();
        let mut owner = Owner::acquire(dir.path().join("state")).await.unwrap();
        let port = owner.start_ts(&node, &script).await.unwrap();
        let child_pid = owner.ts_child.as_ref().unwrap().id().unwrap();
        owner.started(format!("127.0.0.1:{port}")).unwrap();
        // Inject the exact result boundary of a failed proxy drain. The same
        // production cleanup must still reap this owned TS root and descendant.
        let error = owner
            .finish_stop(Err("injected proxy drain failure".into()))
            .await
            .unwrap_err();
        assert!(error.contains("proxy drain failure"));
        assert!(owner.ts_child.is_none());
        assert_eq!(
            read(&dir.path().join("state")).unwrap().unwrap().phase,
            "Stopping"
        );
        #[cfg(unix)]
        assert!(!process_utils::process_group_exists(child_pid).unwrap());
        let task = tokio::spawn(owner.run(CancellationToken::new()));
        let stopped = control(&dir.path().join("state"), "stop", None)
            .await
            .unwrap();
        assert!(stopped.contains("Stopped"));
        task.await.unwrap().unwrap();
        #[cfg(unix)]
        assert!(!process_utils::process_group_exists(child_pid).unwrap());
        #[cfg(windows)]
        let _ = child_pid; // successful shared JobObject stop proves whole-job drain.
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
        );
    }
}
