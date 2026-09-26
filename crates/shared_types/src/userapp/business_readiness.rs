//! UserApp 业务就绪查询契约（rcoder ↔ app-cli 单一事实源）。
//!
//! `GET /api/v1/userapp/{app_id}/{app_stage}/readiness`（rcoder）最终消费
//! `GET /v1/app/readiness`（app-cli 管理 API）返回的 [`UserAppBusinessReadiness`]。
//! 两端共用本模块类型（app-cli 经 path 依赖引用，锁 wire 形态 snake_case，
//! 与日志域 `app_cli_logs` 同款模式）；app_manager 只补充 app_id、app_stage、
//! 平台操作身份与计算资源事实。
//!
//! 语义边界（Spec 只读不变量）：`ready=true` 是带时间戳的一次观察，不是
//! 探针、不是准入、不承诺随后永不故障；观察不确定只返回 `unknown`，
//! 不加恢复围栏、不清锁、不推进控制操作。
//!
//! 与既有健康面的区别：
//! - `3010 /health`、`/ready` 是容器编排探针（进程活着/状态机就绪），保持原样；
//! - 本契约是**业务**就绪：参与汇总的服务 HTTP 健康契约 + Pingap 入口/生效配置。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::app_stage::UserappStage;

/// app-cli 能力位（`GET /v1/deploy/status` 的 capabilities 与 identity 均透出）。
/// 旧运行时无此能力/端点 → rcoder 侧归一为 `unsupported/RUNTIME_UPGRADE_REQUIRED`。
pub const BUSINESS_READINESS_CAPABILITY: &str = "business-readiness-v1";

/// Pingap 自产错误来源契约标识（`proxy.error_origin_contract` 的取值）。
///
/// 含义：当前实例实际生效的 Pingap 配置由平台生成（managed/extend），且
/// 生成配置包含「普通响应移除 `X-Pingap-EType` 头」的 response_headers 规则
/// （自产 `fail_to_proxy` 错误不走响应插件链，头保留）。仅当 admin 确认
/// 实际生效 config hash 与期望一致时才可返回——不能仅凭二进制版本推导。
/// 该标记只用于故障响应呈现，不是控制操作身份或鉴权凭据。
pub const PINGAP_ETYPE_ORIGIN_CONTRACT: &str = "pingap_etype_v1";

/// Pingap 自产错误标记头（锁定版本 `fail_to_proxy` 恒写入；应用同名响应头
/// 经生成配置的 response_headers 规则移除）。
pub const X_PINGAP_ETYPE_HEADER: &str = "x-pingap-etype";

/// 业务就绪状态（顶层与服务/代理共用同一词表）。
///
/// 判定顺序（app-cli 侧纯函数集中派生，见 Plan §4.3）：stopping/stopped/
/// not_deployed → ready → starting → failed → degraded → unknown/unsupported。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserAppReadinessStatus {
    /// 已有应用，目标环境尚未部署/运行应用版本
    NotDeployed,
    /// 当前计算资源或业务编排正在启动（有对应状态证据）
    Starting,
    /// 停止意图已受理且尚未收束
    Stopping,
    /// 已确认计算资源或业务服务处于停止状态
    Stopped,
    /// 参与汇总的服务及代理均满足声明的健康契约
    Ready,
    /// 有当前服务/代理不健康，但不能据此断言启动操作已失败
    Degraded,
    /// 当前版本启动有明确失败证据，且没有仍可用的旧服务集合
    Failed,
    /// 管理接口不可达、身份换代、观测缺失等导致不能确认
    Unknown,
    /// 旧运行时没有新接口，或自定义路由无法自动证明整体就绪
    Unsupported,
}

impl UserAppReadinessStatus {
    /// 除字面量外的公共谓词：该状态是否表示业务可服务。
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// 结构化原因码（客户端分支依据，不解析 message 文本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UserAppReadinessReason {
    // ---- 顶层 / 传输 ----
    /// app-cli 管理 API 连接拒绝/超时（计算资源在运行）
    AdminUnreachable,
    /// app-cli 返回 200 但 JSON 非法/契约缺字段（协议错误，不当成 ready）
    AdminProtocolInvalid,
    /// 观察期间物理实例/控制意图换代，本次观察作废
    InstanceChanged,
    /// 旧 app-cli 无新接口/能力位（需升级运行时镜像）
    RuntimeUpgradeRequired,
    /// 无参与汇总的 proxied web 服务（如纯 worker 应用）
    NoProxiedWebServices,
    /// custom 代理路由无法建立「声明服务 ↔ 实际路由」映射，不敢宣称整体就绪
    CustomRouteUnverified,
    /// 查询预算耗尽，部分服务探测未完成（不折算成失败）
    ObserveIncomplete,

    // ---- 服务级 ----
    /// 服务进程编排进行中（尚未达启动完成证据）
    ServiceStarting,
    /// 健康端点返回非 2xx
    HealthHttpFailure,
    /// 静态托管健康 200 但声明的静态入口不可服务（如缺 index.html）
    StaticEntryUnavailable,
    /// 单项探测超预算
    ProbeTimeout,
    /// 服务自身 HTTP 通过，但映射的 Pingap upstream 无健康 backend
    UpstreamUnhealthy,

    // ---- 代理级 ----
    /// Pingap 未启动/业务监听端口不可达
    ProxyNotStarted,
    /// Pingap 实际生效 config hash 与本实例期望不一致
    ProxyConfigMismatch,

    // ---- 阶段性证据（配合 starting/stopping/stopped/failed/degraded 顶层态）----
    /// 停止意图已受理（顶层 stopping）
    StopAccepted,
    /// 健康运行后退化（顶层 degraded）
    HealthDegraded,
    /// 当前版本编排有明确失败证据（顶层 failed）
    OrchestrationFailed,
}

/// 单个参与汇总服务的观察结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserAppServiceReadiness {
    /// manifest 服务标识（两个同类型前端按 service_id 区分）
    pub service_id: String,
    /// 该服务是否满足声明的 HTTP 健康契约（含静态入口与 upstream 可用性）
    pub ready: bool,
    /// 服务级状态：not_deployed / starting / stopping / stopped / ready / degraded / failed / unknown / unsupported
    pub status: UserAppReadinessStatus,
    /// 结构化原因：ADMIN_UNREACHABLE / ADMIN_PROTOCOL_INVALID / INSTANCE_CHANGED / RUNTIME_UPGRADE_REQUIRED / NO_PROXIED_WEB_SERVICES / CUSTOM_ROUTE_UNVERIFIED / OBSERVE_INCOMPLETE / SERVICE_STARTING / HEALTH_HTTP_FAILURE / STATIC_ENTRY_UNAVAILABLE / PROBE_TIMEOUT / UPSTREAM_UNHEALTHY / PROXY_NOT_STARTED / PROXY_CONFIG_MISMATCH / STOP_ACCEPTED / HEALTH_DEGRADED / ORCHESTRATION_FAILED
    pub reason_code: Option<UserAppReadinessReason>,
}

/// Pingap 入口观察（只描述入口监听与生效配置；各 upstream 健康归各服务结果，
/// 一个后端失败不能把健康前端一并判为不可访问）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserAppProxyReadiness {
    /// 入口监听 + 生效配置 hash 匹配
    pub ready: bool,
    /// 代理入口状态：not_deployed / starting / stopping / stopped / ready / degraded / failed / unknown / unsupported
    pub status: UserAppReadinessStatus,
    /// 结构化原因：ADMIN_UNREACHABLE / ADMIN_PROTOCOL_INVALID / INSTANCE_CHANGED / RUNTIME_UPGRADE_REQUIRED / NO_PROXIED_WEB_SERVICES / CUSTOM_ROUTE_UNVERIFIED / OBSERVE_INCOMPLETE / SERVICE_STARTING / HEALTH_HTTP_FAILURE / STATIC_ENTRY_UNAVAILABLE / PROBE_TIMEOUT / UPSTREAM_UNHEALTHY / PROXY_NOT_STARTED / PROXY_CONFIG_MISMATCH / STOP_ACCEPTED / HEALTH_DEGRADED / ORCHESTRATION_FAILED
    pub reason_code: Option<UserAppReadinessReason>,
    /// 自产错误来源契约（首版 `pingap_etype_v1`）；custom/旧运行配置/hash
    /// 未确认时为 None（故障页据此决定是否信任 `X-Pingap-EType` 标记）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_origin_contract: Option<String>,
}

/// app-cli 业务就绪快照（`/v1/app/readiness` 的 data 载荷，rcoder 原样消费）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UserAppBusinessReadiness {
    /// 顶层汇总 = 非空参与集合全部通过 AND proxy.ready
    pub ready: bool,
    /// 顶层业务状态：not_deployed / starting / stopping / stopped / ready / degraded / failed / unknown / unsupported
    pub status: UserAppReadinessStatus,
    /// 结构化原因（全集：ADMIN_UNREACHABLE / ADMIN_PROTOCOL_INVALID / INSTANCE_CHANGED /
    /// RUNTIME_UPGRADE_REQUIRED / NO_PROXIED_WEB_SERVICES / CUSTOM_ROUTE_UNVERIFIED /
    /// OBSERVE_INCOMPLETE / SERVICE_STARTING / HEALTH_HTTP_FAILURE / STATIC_ENTRY_UNAVAILABLE /
    /// PROBE_TIMEOUT / UPSTREAM_UNHEALTHY / PROXY_NOT_STARTED / PROXY_CONFIG_MISMATCH /
    /// STOP_ACCEPTED / HEALTH_DEGRADED / ORCHESTRATION_FAILED）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<UserAppReadinessReason>,
    /// 实际观察时间（RFC3339 UTC；不是缓存重新包装的时间）
    pub checked_at: String,
    /// RuntimeKernel 进程身份（每次启动重新生成；legacy run 无内核为 None，
    /// 调用方不得把 None 当成跨进程身份凭证）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_instance_id: Option<String>,
    /// 当前仍在提供服务的 release
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_release_id: Option<String>,
    /// 正在准备的 release（准备期间旧版本仍可完整服务）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_release_id: Option<String>,
    /// app-cli 侧当前操作 ID（与平台操作 ID 不同源，分字段透出）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// 同一观察器进程内单调递增的观察序号（换代检测用；不落库、不作 CAS 令牌）
    pub observation_revision: u64,
    pub proxy: UserAppProxyReadiness,
    /// 各服务明细（含被排除出汇总的 worker/disabled 之外的参与集合）
    #[serde(default)]
    pub services: Vec<UserAppServiceReadiness>,
}

/// rcoder 就绪查询响应（app_manager 合并业务快照与平台事实后的最终 data）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserAppReadinessResponse {
    pub app_id: String,
    /// `dev` / `prod`
    pub app_stage: String,
    pub ready: bool,
    /// 顶层业务状态：not_deployed / starting / stopping / stopped / ready / degraded / failed / unknown / unsupported
    pub status: UserAppReadinessStatus,
    /// 结构化原因（全集：ADMIN_UNREACHABLE / ADMIN_PROTOCOL_INVALID / INSTANCE_CHANGED /
    /// RUNTIME_UPGRADE_REQUIRED / NO_PROXIED_WEB_SERVICES / CUSTOM_ROUTE_UNVERIFIED /
    /// OBSERVE_INCOMPLETE / SERVICE_STARTING / HEALTH_HTTP_FAILURE / STATIC_ENTRY_UNAVAILABLE /
    /// PROBE_TIMEOUT / UPSTREAM_UNHEALTHY / PROXY_NOT_STARTED / PROXY_CONFIG_MISMATCH /
    /// STOP_ACCEPTED / HEALTH_DEGRADED / ORCHESTRATION_FAILED）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<UserAppReadinessReason>,
    pub checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_release_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_release_id: Option<String>,
    /// 平台侧操作 ID（UserAppOperationRecord；缺最近操作时 None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// app-cli 侧操作 ID（与平台操作不同源）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_operation_id: Option<String>,
    #[serde(default)]
    pub observation_revision: u64,
    pub proxy: UserAppProxyReadiness,
    #[serde(default)]
    pub services: Vec<UserAppServiceReadiness>,
}

/// 观察使用的物理通道（仅状态透出，非控制面语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserAppReadinessChannel {
    /// 容器/Pod IP 直连管理端口
    Direct,
    /// 容器内固定只读命令（`app-cli readiness --json --admin-addr ...`）
    Exec,
}

/// 本次观察绑定的物理实例事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserAppReadinessPhysical {
    /// 容器 ID / Pod UID（换代复核的比对基准）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// 实际使用的管理地址（直连通道）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub channel: UserAppReadinessChannel,
}

/// rcoder-engine 注入 app_manager 的只读观察结果。
///
/// Err（String）= 查询系统本身失败（runtime API/RBAC/存储），app_manager
/// 映射 5xx 结构化错误，不伪装 stopped/failed。
#[derive(Debug, Clone)]
pub enum UserAppReadinessObservation {
    /// 目标 scope 没有运行中的计算资源（未部署/已停止/完全缺失）。
    /// detail 说明是哪一种（供 starting/stopped/not_deployed 与控制态合并）。
    NoCompute {
        /// `stopped`（资源保留但停）/ `missing`（无部署）等判别说明
        detail: Option<String>,
    },
    /// 计算资源在运行，app-cli 返回了业务快照。
    Snapshot {
        physical: UserAppReadinessPhysical,
        snapshot: UserAppBusinessReadiness,
    },
    /// 计算资源在运行，但管理接口传输失败（拒绝/超时）→ unknown/ADMIN_UNREACHABLE。
    AdminUnreachable { physical: UserAppReadinessPhysical },
    /// 计算资源在运行，但 app-cli 过旧不支持新接口 → unsupported/RUNTIME_UPGRADE_REQUIRED。
    UnsupportedRuntime { physical: UserAppReadinessPhysical },
    /// 观察期间物理实例换代且预算内重读仍不一致 → unknown/INSTANCE_CHANGED。
    InstanceChanged,
}

/// 只读业务观察回调契约（rcoder-engine 实现、app_manager 消费）。
///
/// 实现约束（Spec §4）：不得 ensure/wake/start/stop/adopt/reconcile，不申请
/// 业务锁，不刷新闲置计时，不写期望运行态；观察锁不得跨网络 I/O 持有；
/// Stop/Restart 照常运行不受查询拖累。
#[async_trait::async_trait]
pub trait UserAppReadinessReader: Send + Sync {
    /// 观察指定 app+stage 当前物理实例内的业务就绪。
    ///
    /// `budget` 是本次查询剩余预算（含 rcoder 侧复核）；实现不得超预算
    /// 等待，预算不足优先返回 `NoCompute`/`AdminUnreachable` 而非挂起。
    async fn observe(
        &self,
        app_id: &str,
        stage: UserappStage,
        budget: std::time::Duration,
    ) -> Result<UserAppReadinessObservation, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 锁：状态/原因码序列化形态（snake_case / SCREAMING_SNAKE_CASE）。
    #[test]
    fn enum_wire_forms_are_locked() {
        assert_eq!(
            serde_json::to_string(&UserAppReadinessStatus::NotDeployed).unwrap(),
            "\"not_deployed\""
        );
        assert_eq!(
            serde_json::to_string(&UserAppReadinessStatus::Unknown).unwrap(),
            "\"unknown\""
        );
        assert_eq!(
            serde_json::to_string(&UserAppReadinessReason::AdminUnreachable).unwrap(),
            "\"ADMIN_UNREACHABLE\""
        );
        assert_eq!(
            serde_json::to_string(&UserAppReadinessReason::RuntimeUpgradeRequired).unwrap(),
            "\"RUNTIME_UPGRADE_REQUIRED\""
        );
    }

    /// wire 锁：快照 JSON 缺省字段（Option skip）与必填字段共存。
    #[test]
    fn snapshot_roundtrip_omits_optional_fields() {
        let snapshot = UserAppBusinessReadiness {
            ready: false,
            status: UserAppReadinessStatus::Starting,
            reason_code: Some(UserAppReadinessReason::ServiceStarting),
            checked_at: "2026-09-26T08:00:00Z".into(),
            runtime_instance_id: None,
            serving_release_id: None,
            target_release_id: Some("rel-2".into()),
            operation_id: None,
            observation_revision: 7,
            proxy: UserAppProxyReadiness {
                ready: false,
                status: UserAppReadinessStatus::Starting,
                reason_code: Some(UserAppReadinessReason::ProxyNotStarted),
                error_origin_contract: None,
            },
            services: vec![UserAppServiceReadiness {
                service_id: "frontend".into(),
                ready: true,
                status: UserAppReadinessStatus::Ready,
                reason_code: None,
            }],
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        assert!(value.get("runtime_instance_id").is_none());
        assert_eq!(value["target_release_id"], "rel-2");
        assert_eq!(value["observation_revision"], 7);
        // 双端共用类型：反序列化必须无损
        let back: UserAppBusinessReadiness = serde_json::from_value(value).unwrap();
        assert_eq!(back, snapshot);
    }

    #[test]
    fn proxy_error_origin_contract_constant_matches_plan() {
        assert_eq!(PINGAP_ETYPE_ORIGIN_CONTRACT, "pingap_etype_v1");
        assert_eq!(X_PINGAP_ETYPE_HEADER, "x-pingap-etype");
    }
}
