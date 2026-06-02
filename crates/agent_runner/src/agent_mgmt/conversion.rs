//! AgentManifest ↔ proto 转换 (P0-1)
//!
//! proto 类型由 `shared_types_grpc` 暴露;AgentManifest 存在 `agent_mgmt::installer`。

use shared_types::{
    AgentInfo, AgentInstallStatus as SharedStatus, InstallType as SharedInstallType,
};
use shared_types::{
    InstallAgentResponse as SharedInstallResp, AgentInstallStatus,
};
use shared_types_grpc::{
    AgentInfo as ProtoAgentInfo, AgentInstallStatus as ProtoStatus, InstallAgentResponse as ProtoInstallResp,
    InstallType as ProtoInstallType, SystemInfo as ProtoSystemInfo,
};

use crate::agent_mgmt::installer::AgentManifest;

/// AgentManifest → proto AgentInfo
pub fn manifest_to_proto_agent_info(m: &AgentManifest) -> ProtoAgentInfo {
    ProtoAgentInfo {
        agent_id: m.agent_id.clone(),
        install_type: install_type_to_proto(m.install_type) as i32,
        status: install_status_to_proto(infer_status(m)) as i32,
        version: m.version.clone(),
        binary_path: Some(m.binary_path.clone()),
        installed_at: Some(m.installed_at),
    }
}

/// AgentManifest → shared AgentInfo
pub fn manifest_to_shared_agent_info(m: &AgentManifest) -> AgentInfo {
    AgentInfo {
        agent_id: m.agent_id.clone(),
        install_type: m.install_type,
        status: infer_status(m),
        version: m.version.clone(),
        binary_path: Some(m.binary_path.clone()),
        installed_at: Some(m.installed_at),
    }
}

/// proto InstallType → shared InstallType
///
/// Proto `Unspecified(0)` 映射为 `Unknown`,其余按值映射。
/// 无效的 i32 值也映射为 `Unknown`(fail-safe)。
pub fn install_type_from_proto(p: i32) -> SharedInstallType {
    match ProtoInstallType::try_from(p) {
        Ok(ProtoInstallType::Builtin) => SharedInstallType::Builtin,
        Ok(ProtoInstallType::Binary) => SharedInstallType::Binary,
        Ok(ProtoInstallType::Npm) => SharedInstallType::Npm,
        Ok(ProtoInstallType::Url) => SharedInstallType::Url,
        _ => SharedInstallType::Unknown,
    }
}

/// shared InstallType → proto
pub fn install_type_to_proto(t: SharedInstallType) -> ProtoInstallType {
    match t {
        SharedInstallType::Builtin => ProtoInstallType::Builtin,
        SharedInstallType::Binary => ProtoInstallType::Binary,
        SharedInstallType::Npm => ProtoInstallType::Npm,
        SharedInstallType::Url => ProtoInstallType::Url,
        // Unknown 是 shared 端 fail-safe 值,不会主动从 agent_runner 发出
        SharedInstallType::Unknown => ProtoInstallType::Binary,
    }
}

fn install_status_to_proto(s: SharedStatus) -> ProtoStatus {
    match s {
        SharedStatus::Available => ProtoStatus::Available,
        SharedStatus::Broken => ProtoStatus::Broken,
        SharedStatus::NotInstalled => ProtoStatus::NotInstalled,
        // Unknown 是 shared 端 fail-safe 值
        SharedStatus::Unknown => ProtoStatus::NotInstalled,
    }
}

fn infer_status(m: &AgentManifest) -> SharedStatus {
    let path = std::path::Path::new(&m.binary_path);
    if path.exists() {
        SharedStatus::Available
    } else {
        SharedStatus::Broken
    }
}

/// shared AgentInfo → AgentManifest
#[allow(dead_code)]
pub fn shared_agent_info_to_manifest(info: &AgentInfo) -> AgentManifest {
    AgentManifest {
        agent_id: info.agent_id.clone(),
        install_type: info.install_type,
        command: info
            .binary_path
            .as_ref()
            .and_then(|p| std::path::Path::new(p).file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("agent")
            .to_string(),
        args: vec![],
        binary_path: info.binary_path.clone().unwrap_or_default(),
        source: None,
        version: info.version.clone(),
        file_size: 0,
        file_type: "executable".into(),
        installed_at: info.installed_at.unwrap_or(0),
    }
}

/// 包装 shared SystemInfo 为 proto(便于通过 tonic 直接序列化)
pub fn system_info_to_proto(s: &shared_types::SystemInfo) -> ProtoSystemInfo {
    ProtoSystemInfo {
        os: s.os.clone(),
        arch: s.arch.clone(),
        platform: s.platform.clone(),
    }
}

/// 包装 proto SystemInfo 为 shared
#[allow(dead_code)]
pub fn system_info_from_proto(p: &ProtoSystemInfo) -> shared_types::SystemInfo {
    shared_types::SystemInfo {
        os: p.os.clone(),
        arch: p.arch.clone(),
        platform: p.platform.clone(),
    }
}

/// proto InstallAgentResponse → shared InstallAgentResponse(供 HTTP 处理器序列化)
pub fn install_response_to_shared(p: &ProtoInstallResp) -> SharedInstallResp {
    SharedInstallResp {
        agent_id: p.agent_id.clone(),
        status: install_status_from_proto(p.status),
        binary_path: p.binary_path.clone(),
        file_type: p.file_type.clone(),
        file_count: p.file_count.map(|n| n as usize),
        file_size: p.file_size.max(0) as u64,
        version: p.version.clone(),
        source_url: p.source_url.clone(),
    }
}

fn install_status_from_proto(i: i32) -> AgentInstallStatus {
    match ProtoStatus::try_from(i) {
        Ok(ProtoStatus::Available) => AgentInstallStatus::Available,
        Ok(ProtoStatus::Broken) => AgentInstallStatus::Broken,
        Ok(ProtoStatus::NotInstalled) => AgentInstallStatus::NotInstalled,
        _ => AgentInstallStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::InstallType;

    #[test]
    fn install_type_roundtrip() {
        for t in [
            InstallType::Builtin,
            InstallType::Binary,
            InstallType::Npm,
            InstallType::Url,
        ] {
            let p = install_type_to_proto(t);
            let back = install_type_from_proto(p as i32);
            assert_eq!(t, back);
        }
    }

    #[test]
    fn manifest_to_proto_handles_missing_binary() {
        let m = AgentManifest::new(
            "ghost".into(),
            InstallType::Binary,
            "ghost".into(),
            vec![],
            "/nonexistent/ghost".into(),
            0,
            "executable".into(),
        );
        let p = manifest_to_proto_agent_info(&m);
        assert_eq!(p.agent_id, "ghost");
        assert_eq!(p.status, ProtoStatus::Broken as i32);
    }
}
