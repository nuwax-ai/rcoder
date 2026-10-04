//! P4.1: native owner 持久化协议——receipt/请求/回复类型、目录布局、原子
//! 读写与 status 投影（PX-04 的 bound address 在 supervisor_reply 内）。
//! 与 owner 运行时（acquire/run/stop, native_control.rs）分文件维护;
//! 公开路径与 wire 不因搬移改变。

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::fs::File;
use std::path::{Path, PathBuf};

pub(super) const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub(super) version: u32,
    pub(super) instance_id: String,
    pub(super) token: String,
    pub(super) control_address: String,
    pub address: String,
    pub phase: String,
    #[serde(default)]
    pub(super) supervisor_id: Option<String>,
    #[serde(default)]
    pub(super) retirement_requested: bool,
    /// PX-01: 本次启动请求的关联 ID（npm launch UUID）——仅请求关联, 不是
    /// 执行身份; instance_id 恒为监督器/owner 的真实 generation。npm 用它
    /// 区分"本次新建"与"复用已存在 owner"。
    #[serde(default)]
    pub(super) launch_request_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub(super) version: u32,
    pub(super) instance_id: String,
    pub(super) token: String,
    pub(super) action: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub(super) version: u32,
    pub(super) instance_id: String,
    pub(super) phase: String,
    pub(super) address: String,
    pub(super) error: Option<String>,
    /// PX-01: 请求关联（本次创建该 owner 的 launch UUID; 旧回执/复用为 null）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) launch_request_id: Option<String>,
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
pub(super) fn read(root: &Path) -> Result<Option<Receipt>, String> {
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
pub(super) fn write(root: &Path, receipt: &Receipt) -> Result<(), String> {
    use std::io::Write;
    let mut file =
        tempfile::NamedTempFile::new_in(root).map_err(|e| format!("create owner receipt: {e}"))?;
    serde_json::to_writer(&mut file, receipt).map_err(|e| format!("encode owner receipt: {e}"))?;
    file.flush()
        .and_then(|()| file.as_file().sync_all())
        .map_err(|e| format!("sync owner receipt: {e}"))?;
    process_utils::atomic_file::persist(file, &root.join("owner.json"))
        .map_err(|e| format!("publish owner receipt: {e}"))?;
    #[cfg(unix)]
    File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("sync owner directory: {e}"))?;
    Ok(())
}

pub(super) fn supervisor_reply(
    snapshot: runtime_supervisor::Snapshot,
    root: &Path,
) -> Result<String, String> {
    let phase = match snapshot.phase {
        runtime_supervisor::Phase::Ready => "Running",
        runtime_supervisor::Phase::Stopped => "Stopped",
        runtime_supervisor::Phase::Starting => "Starting",
        runtime_supervisor::Phase::Stopping | runtime_supervisor::Phase::CleanupPending => {
            "Stopping"
        }
        _ => "RecoveryRequired",
    };
    // Ready requires matching worker evidence and a real bound address. A
    // missing, damaged, or foreign receipt must not become successful Running
    // with an empty endpoint. Other management phases, especially Stopped,
    // remain queryable without consuming a damaged business receipt.
    let (bound_address, launch_request_id) = if snapshot.phase == runtime_supervisor::Phase::Ready {
        let receipt =
            read(root)?.ok_or("ready owner receipt is unavailable; readiness is unconfirmed")?;
        if snapshot.generation.as_deref() != Some(receipt.instance_id.as_str())
            || receipt.supervisor_id.as_deref() != Some(snapshot.supervisor_id.as_str())
        {
            return Err(
                "ready owner receipt identity differs from supervisor; readiness is unconfirmed"
                    .into(),
            );
        }
        if receipt.phase != "Running" {
            return Err(format!(
                "ready owner receipt is {}; readiness is unconfirmed",
                receipt.phase
            ));
        }
        let address: std::net::SocketAddr = receipt.address.parse().map_err(
            |_| "ready owner receipt has no valid bound address; readiness is unconfirmed",
        )?;
        if address.port() == 0 {
            return Err("ready owner receipt address is unbound; readiness is unconfirmed".into());
        }
        (receipt.address, receipt.launch_request_id)
    } else {
        (String::new(), None)
    };
    serde_json::to_string(
        &serde_json::json!({"version":2,"instance_id":snapshot.generation,
        "supervisor_id":snapshot.supervisor_id,"phase":phase,"stage":snapshot.phase,
        "operation_id":snapshot.operation_id,"error":snapshot.error,"problem":snapshot.problem,
        "address":bound_address,"launch_request_id":launch_request_id}),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_snapshot(root: &Path) -> runtime_supervisor::Snapshot {
        runtime_supervisor::Snapshot {
            version: 1,
            binding: runtime_supervisor::Binding {
                component: "file-server-proxy".into(),
                resource: root.to_owned(),
            },
            supervisor_id: uuid::Uuid::new_v4().to_string(),
            generation: Some(uuid::Uuid::new_v4().to_string()),
            phase: runtime_supervisor::Phase::Ready,
            intent: runtime_supervisor::Intent::Run,
            operation_id: None,
            error: None,
            problem: None,
        }
    }

    fn matching_receipt(snapshot: &runtime_supervisor::Snapshot) -> Receipt {
        Receipt {
            version: VERSION,
            instance_id: snapshot.generation.clone().unwrap(),
            token: "private-control-token-at-least-32-bytes".into(),
            control_address: "127.0.0.1:12345".into(),
            address: "127.0.0.1:54321".into(),
            phase: "Running".into(),
            supervisor_id: Some(snapshot.supervisor_id.clone()),
            retirement_requested: false,
            launch_request_id: Some(uuid::Uuid::new_v4().to_string()),
        }
    }

    #[test]
    fn ready_status_rejects_missing_corrupt_foreign_or_unbound_receipt() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = ready_snapshot(root.path());
        assert!(
            supervisor_reply(snapshot.clone(), root.path()).is_err(),
            "missing receipt cannot establish Running"
        );
        std::fs::write(root.path().join("owner.json"), b"{bad json").unwrap();
        assert!(
            supervisor_reply(snapshot.clone(), root.path()).is_err(),
            "damaged receipt must be reported"
        );
        for case in ["generation", "supervisor", "phase", "address"] {
            let mut receipt = matching_receipt(&snapshot);
            match case {
                "generation" => receipt.instance_id = uuid::Uuid::new_v4().to_string(),
                "supervisor" => receipt.supervisor_id = Some(uuid::Uuid::new_v4().to_string()),
                "phase" => receipt.phase = "Starting".into(),
                "address" => receipt.address.clear(),
                _ => unreachable!(),
            }
            write(root.path(), &receipt).unwrap();
            assert!(
                supervisor_reply(snapshot.clone(), root.path()).is_err(),
                "invalid {case} cannot establish Running"
            );
        }
        let receipt = matching_receipt(&snapshot);
        write(root.path(), &receipt).unwrap();
        let reply: serde_json::Value =
            serde_json::from_str(&supervisor_reply(snapshot, root.path()).unwrap()).unwrap();
        assert_eq!(reply["phase"], "Running");
        assert_eq!(reply["address"], "127.0.0.1:54321");
        assert_eq!(
            reply["launch_request_id"],
            receipt.launch_request_id.unwrap()
        );
    }

    #[test]
    fn stopped_status_remains_available_with_damaged_worker_receipt() {
        let root = tempfile::tempdir().unwrap();
        let mut snapshot = ready_snapshot(root.path());
        snapshot.phase = runtime_supervisor::Phase::Stopped;
        std::fs::write(root.path().join("owner.json"), b"{bad json").unwrap();
        let reply: serde_json::Value =
            serde_json::from_str(&supervisor_reply(snapshot, root.path()).unwrap()).unwrap();
        assert_eq!(reply["phase"], "Stopped");
        assert_eq!(reply["address"], "");
    }
}
