//! AgentManifest ↔ proto 转换 (P0-1)
//!
//! proto 类型由 `shared_types_grpc` 暴露;AgentManifest 存在 `agent_mgmt::installer`。

use shared_types::{
    AgentInfo, AgentInstallStatus as SharedStatus, InstallType as SharedInstallType,
};
use shared_types::{AgentInstallStatus, InstallAgentResponse as SharedInstallResp};
use shared_types_grpc::{
    AgentInfo as ProtoAgentInfo, AgentInstallStatus as ProtoStatus,
    InstallAgentResponse as ProtoInstallResp, InstallType as ProtoInstallType,
    SystemInfo as ProtoSystemInfo,
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
/// 集中实现在 `shared_types::agent::mgmt_proto`（HTTP 转发层共用）：
/// `Unspecified(0)` 与无效 i32 均为 `Unknown`（观察未知不伪装）。
pub fn install_type_from_proto(p: i32) -> SharedInstallType {
    SharedInstallType::from_proto_i32(p)
}

/// shared InstallType → proto
///
/// 集中实现：`Unknown` → `Unspecified(0)`。安装输入入口对 Unknown 的显式拒绝
/// 在 grpc.rs 保持不变。
pub fn install_type_to_proto(t: SharedInstallType) -> ProtoInstallType {
    ProtoInstallType::try_from(t.to_proto_i32()).unwrap_or(ProtoInstallType::Unspecified)
}

fn install_status_to_proto(s: SharedStatus) -> ProtoStatus {
    ProtoStatus::try_from(s.to_proto_i32()).unwrap_or(ProtoStatus::Unspecified)
}

fn infer_status(m: &AgentManifest) -> SharedStatus {
    let path = std::path::Path::new(&m.binary_path);
    if path.exists() {
        SharedStatus::Available
    } else {
        SharedStatus::Broken
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

/// proto InstallAgentResponse → shared InstallAgentResponse(供 HTTP 处理器序列化)
pub fn install_response_to_shared(p: &ProtoInstallResp) -> SharedInstallResp {
    use std::str::FromStr;
    SharedInstallResp {
        agent_id: p.agent_id.clone(),
        status: install_status_from_proto(p.status),
        binary_path: p.binary_path.clone(),
        file_type: p.file_type.clone(),
        file_count: p.file_count.map(|n| n as usize),
        file_size: p.file_size.max(0) as u64,
        version: p.version.clone(),
        source_url: p.source_url.clone(),
        action: shared_types::InstallAction::from_str(p.action.as_str()).ok(),
        installed: p.installed,
        previous_version: if p.previous_version.is_empty() {
            None
        } else {
            Some(p.previous_version.clone())
        },
        platform: if p.platform.is_empty() {
            None
        } else {
            Some(p.platform.clone())
        },
    }
}

fn install_status_from_proto(i: i32) -> AgentInstallStatus {
    AgentInstallStatus::from_proto_i32(i)
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

    /// R10 反例: SharedUnknown → ProtoBinary 把"观察到的未知"伪装成具体安装
    /// 方式; 应回 proto Unspecified(0), 双向未知保持未知。
    #[test]
    fn unknown_install_type_maps_to_proto_unspecified_both_ways() {
        assert_eq!(
            install_type_to_proto(InstallType::Unknown),
            ProtoInstallType::Unspecified
        );
        for v in [0, -1, 999] {
            assert_eq!(install_type_from_proto(v), InstallType::Unknown, "{v}");
        }
    }

    /// R10 反例: SharedUnknown 状态 → ProtoNotInstalled 把未知伪装成"未安装"。
    #[test]
    fn unknown_status_maps_to_proto_unspecified_both_ways() {
        assert_eq!(
            install_status_to_proto(SharedStatus::Unknown),
            ProtoStatus::Unspecified
        );
        for v in [0, -1, 999] {
            assert_eq!(install_status_from_proto(v), SharedStatus::Unknown, "{v}");
        }
    }

    /// R10: 持久 registry 的 install_type=unknown 经 gRPC 列表
    /// (manifest→proto→HTTP 转发)与直接 shared 列表两条查询路径展示一致。
    #[test]
    fn persisted_unknown_install_type_displays_consistently() {
        let m = AgentManifest::new(
            "legacy".into(),
            InstallType::Unknown,
            "legacy".into(),
            vec![],
            "/nonexistent/legacy".into(),
            0,
            "executable".into(),
        );
        let proto = manifest_to_proto_agent_info(&m);
        assert_eq!(proto.install_type, ProtoInstallType::Unspecified as i32);
        assert_eq!(
            install_type_from_proto(proto.install_type),
            InstallType::Unknown,
            "gRPC path must surface unknown, not a concrete install type"
        );
        assert_eq!(
            manifest_to_shared_agent_info(&m).install_type,
            InstallType::Unknown,
            "direct list path must surface unknown"
        );
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
