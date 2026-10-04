//! P4.1: native owner 持久化协议——receipt/请求/回复类型、目录布局、原子
//! 读写与 status 投影（PX-04 的 bound address 在 supervisor_reply 内）。
//! 与 owner 运行时（acquire/run/stop, native_control.rs）分文件维护;
//! 公开路径与 wire 不因搬移改变。

use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

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
    // PX-04: 同代核验后的 bound address——receipt 只在与 snapshot 同代且 Running
    // 时提供（旧 owner 残留 receipt 不与新 snapshot 拼接）; --port 0 的真实
    // 文件 API 地址经结构化 status 发布, 不依赖日志/PID。
    let (bound_address, launch_request_id) = read(root)
        .ok()
        .flatten()
        .filter(|receipt| {
            snapshot.generation.as_deref() == Some(receipt.instance_id.as_str())
                && receipt.phase == "Running"
        })
        .map(|receipt| (receipt.address, receipt.launch_request_id))
        .unwrap_or((String::new(), None));
    serde_json::to_string(
        &serde_json::json!({"version":2,"instance_id":snapshot.generation,
        "supervisor_id":snapshot.supervisor_id,"phase":phase,"stage":snapshot.phase,
        "operation_id":snapshot.operation_id,"error":snapshot.error,"problem":snapshot.problem,
        "address":bound_address,"launch_request_id":launch_request_id}),
    )
    .map_err(|e| e.to_string())
}
