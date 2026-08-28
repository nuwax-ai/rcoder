//! 容器运行状态枚举与映射。

use serde::{Deserialize, Serialize};

/// 容器状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContainerStatus {
    /// 创建中
    Creating,
    /// 运行中
    Running,
    /// 已停止
    Stopped,
    /// 已暂停
    Paused,
    /// 重启中
    Restarting,
    /// 移除中
    Removing,
    /// 已退出
    Exited,
    /// 已死亡
    Dead,
    /// 未知状态
    Unknown(String),
}

impl From<String> for ContainerStatus {
    fn from(status: String) -> Self {
        match status.to_lowercase().as_str() {
            "created" => ContainerStatus::Creating,
            "running" => ContainerStatus::Running,
            "stopped" => ContainerStatus::Stopped,
            "paused" => ContainerStatus::Paused,
            "restarting" => ContainerStatus::Restarting,
            "removing" => ContainerStatus::Removing,
            "exited" => ContainerStatus::Exited,
            "dead" => ContainerStatus::Dead,
            _ => ContainerStatus::Unknown(status),
        }
    }
}

impl ContainerStatus {
    /// 是否处于运行中（替代各处的 `status == "running"` 字符串比较）
    pub fn is_running(&self) -> bool {
        matches!(self, ContainerStatus::Running)
    }
}

impl std::fmt::Display for ContainerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerStatus::Creating => f.write_str("created"),
            ContainerStatus::Running => f.write_str("running"),
            ContainerStatus::Stopped => f.write_str("stopped"),
            ContainerStatus::Paused => f.write_str("paused"),
            ContainerStatus::Restarting => f.write_str("restarting"),
            ContainerStatus::Removing => f.write_str("removing"),
            ContainerStatus::Exited => f.write_str("exited"),
            ContainerStatus::Dead => f.write_str("dead"),
            ContainerStatus::Unknown(s) => f.write_str(s),
        }
    }
}
