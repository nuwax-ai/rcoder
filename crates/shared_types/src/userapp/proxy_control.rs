//! Proxy confirmation produced by app-cli, independent of Pingap private APIs.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Advertised by owners that can consume the official foreground-restart contract.
pub const OFFICIAL_PROXY_RESTART_CAPABILITY: &str = "official-proxy-restart-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProxyConfirmationContract {
    /// An owned foreground process was started from a frozen configuration and
    /// verified over HTTP. This is not a Pingap internal Applied receipt.
    OfficialRestartHttpV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProxyPhase {
    Absent,
    Preparing,
    Stopping,
    Starting,
    Verifying,
    Serving,
    Standby,
    Failed,
    RecoveryRequired,
}

impl ProxyPhase {
    pub const fn is_transitioning(self) -> bool {
        match self {
            Self::Preparing | Self::Stopping | Self::Starting | Self::Verifying => true,
            Self::Absent
            | Self::Serving
            | Self::Standby
            | Self::Failed
            | Self::RecoveryRequired => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProxyConfirmation {
    /// 确认契约：official_restart_http_v1（app-cli 的受控进程与 HTTP 验证）。
    pub contract: ProxyConfirmationContract,
    /// 是否完成当前实例的确认；过渡阶段和观察不完整时为 false。
    pub confirmed: bool,
    /// 代理阶段：absent / preparing / stopping / starting / verifying / serving / standby / failed / recovery_required。
    pub phase: ProxyPhase,
    /// 本次控制请求身份，或无父请求时的独立代理发布身份。
    pub operation_id: String,
    /// app-cli 持有的代理实例身份；不是 Pingap 内部 Applied 标识。
    pub proxy_instance_id: Option<String>,
    /// 不可变候选配置的 SHA256 摘要。
    pub config_digest: Option<String>,
}

impl ProxyConfirmation {
    /// Progress classification only; this does not authorize process mutation.
    pub const fn is_transitioning(&self) -> bool {
        self.phase.is_transitioning()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProxyReloadState {
    Pending,
    Running,
    Succeeded,
    Failed,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProxyReloadOperation {
    /// reload 原请求身份；重试不会创建新操作。
    pub operation_id: String,
    /// 操作状态：pending / running / succeeded / failed / recovery_required。
    pub state: ProxyReloadState,
    /// 代理阶段：absent / preparing / stopping / starting / verifying / serving / standby / failed / recovery_required。
    pub phase: ProxyPhase,
    /// 失败或恢复保护的稳定错误码。
    pub error_code: Option<String>,
    /// 当前操作的具体失败原因；回退成功不改变本次 Failed 结果。
    pub error_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_confirmation_wire_contract_is_explicit() {
        let confirmation = ProxyConfirmation {
            contract: ProxyConfirmationContract::OfficialRestartHttpV1,
            confirmed: true,
            phase: ProxyPhase::Serving,
            operation_id: "operation-1".into(),
            proxy_instance_id: Some("process-1".into()),
            config_digest: Some("digest-1".into()),
        };
        let value = serde_json::to_value(&confirmation).unwrap();
        assert_eq!(value["contract"], "official_restart_http_v1");
        assert_eq!(value["phase"], "serving");
        assert_eq!(value["operation_id"], "operation-1");
        assert_eq!(
            serde_json::from_value::<ProxyConfirmation>(value).unwrap(),
            confirmation
        );
    }
}
