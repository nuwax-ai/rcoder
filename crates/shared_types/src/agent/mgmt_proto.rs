//! `InstallType` / `AgentInstallStatus` 与 proto i32 的集中双向转换。
//!
//! 语义（R10 收口）：
//! - proto `Unspecified(0)` 与未识别整数 → shared `Unknown`：观察到的未知不得
//!   伪装成具体安装方式或"未安装"；
//! - shared `Unknown` → proto `Unspecified(0)`；
//! - 已知值穷尽映射，proto 新增变体会因 `try_from` 落入未知分支而非漏配。
//!
//! 安装**输入**入口对 `Unknown` 的拒绝（agent_runner grpc.rs）不受本模块影响：
//! 查询结果的未知展示与安装请求的有效性是两个语义。

use shared_types_grpc::{
    AgentInstallStatus as ProtoAgentInstallStatus, InstallType as ProtoInstallType,
};

use crate::{AgentInstallStatus, InstallType};

impl InstallType {
    /// proto i32 → shared。`Unspecified(0)`/未识别值 → `Unknown`。
    pub fn from_proto_i32(v: i32) -> Self {
        match ProtoInstallType::try_from(v) {
            Ok(ProtoInstallType::Builtin) => InstallType::Builtin,
            Ok(ProtoInstallType::Binary) => InstallType::Binary,
            Ok(ProtoInstallType::Npm) => InstallType::Npm,
            Ok(ProtoInstallType::Url) => InstallType::Url,
            Ok(ProtoInstallType::Unspecified) | Err(_) => InstallType::Unknown,
        }
    }

    /// shared → proto i32。`Unknown` → `Unspecified(0)`。
    pub fn to_proto_i32(self) -> i32 {
        match self {
            InstallType::Builtin => ProtoInstallType::Builtin as i32,
            InstallType::Binary => ProtoInstallType::Binary as i32,
            InstallType::Npm => ProtoInstallType::Npm as i32,
            InstallType::Url => ProtoInstallType::Url as i32,
            InstallType::Unknown => ProtoInstallType::Unspecified as i32,
        }
    }
}

impl AgentInstallStatus {
    /// proto i32 → shared。`Unspecified(0)`/未识别值 → `Unknown`。
    pub fn from_proto_i32(v: i32) -> Self {
        match ProtoAgentInstallStatus::try_from(v) {
            Ok(ProtoAgentInstallStatus::Available) => AgentInstallStatus::Available,
            Ok(ProtoAgentInstallStatus::Broken) => AgentInstallStatus::Broken,
            Ok(ProtoAgentInstallStatus::NotInstalled) => AgentInstallStatus::NotInstalled,
            Ok(ProtoAgentInstallStatus::Unspecified) | Err(_) => AgentInstallStatus::Unknown,
        }
    }

    /// shared → proto i32。`Unknown` → `Unspecified(0)`。
    pub fn to_proto_i32(self) -> i32 {
        match self {
            AgentInstallStatus::Available => ProtoAgentInstallStatus::Available as i32,
            AgentInstallStatus::Broken => ProtoAgentInstallStatus::Broken as i32,
            AgentInstallStatus::NotInstalled => ProtoAgentInstallStatus::NotInstalled as i32,
            AgentInstallStatus::Unknown => ProtoAgentInstallStatus::Unspecified as i32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_roundtrip_both_ways() {
        for (shared, proto) in [
            (InstallType::Builtin, ProtoInstallType::Builtin as i32),
            (InstallType::Binary, ProtoInstallType::Binary as i32),
            (InstallType::Npm, ProtoInstallType::Npm as i32),
            (InstallType::Url, ProtoInstallType::Url as i32),
        ] {
            assert_eq!(
                InstallType::from_proto_i32(proto),
                shared,
                "install {proto}"
            );
            assert_eq!(shared.to_proto_i32(), proto);
        }
        for (shared, proto) in [
            (
                AgentInstallStatus::Available,
                ProtoAgentInstallStatus::Available as i32,
            ),
            (
                AgentInstallStatus::Broken,
                ProtoAgentInstallStatus::Broken as i32,
            ),
            (
                AgentInstallStatus::NotInstalled,
                ProtoAgentInstallStatus::NotInstalled as i32,
            ),
        ] {
            assert_eq!(
                AgentInstallStatus::from_proto_i32(proto),
                shared,
                "status {proto}"
            );
            assert_eq!(shared.to_proto_i32(), proto);
        }
    }

    #[test]
    fn unknown_stays_unknown_in_both_directions() {
        for v in [0, -1, 999] {
            assert_eq!(InstallType::from_proto_i32(v), InstallType::Unknown, "{v}");
            assert_eq!(
                AgentInstallStatus::from_proto_i32(v),
                AgentInstallStatus::Unknown,
                "{v}"
            );
        }
        assert_eq!(
            InstallType::Unknown.to_proto_i32(),
            ProtoInstallType::Unspecified as i32
        );
        assert_eq!(
            AgentInstallStatus::Unknown.to_proto_i32(),
            ProtoAgentInstallStatus::Unspecified as i32
        );
    }
}
