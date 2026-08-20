//! 应用发布模型（prepare/activate/rollback 请求与 release 记录）

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 预备发布请求（下载制品包并校验入库，不切流）
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrepareReleaseRequest {
    /// 发布 ID（调用方生成，幂等键之一：同 id + 同 sha256 + 同 size 重复 prepare 直接返回既有记录）
    #[schema(example = "rel-20260814-001")]
    pub release_id: String,
    /// 制品 zip 下载地址（HTTP/HTTPS，由 rcoder 服务端下载）
    #[schema(example = "https://registry.example.com/app-order-svc/rel-001.zip")]
    pub url: String,
    /// 制品 sha256（64 位十六进制，小写；下载后校验一致性）
    #[schema(example = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")]
    pub sha256: String,
    /// 制品字节数（与实际下载大小比对）
    #[schema(example = 10485760)]
    pub size_bytes: u64,
    /// 保留份数（可选；缺省取环境配置默认值，须在允许区间内）
    #[schema(example = 3)]
    pub retention: Option<u16>,
}

/// 激活发布请求
#[derive(Debug, Clone, Deserialize, Default, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivateReleaseRequest {
    /// 等待新版本就绪的超时秒数（可选；默认 300，范围 5..=1800——Java 等慢启动应用可调大）。
    /// 就绪=status Running 且 health 非 Unhealthy；超时/进入 Error → 置 Failed **保留现场**
    /// （code/rollback 快照/制品包均不动，供排查；恢复用 rollback 接口）。
    /// 注意：等待期间本请求同步阻塞，调用方 HTTP 读超时须 ≥ 此值 + 余量。
    #[schema(example = 300)]
    pub readiness_timeout_seconds: Option<u64>,
}

/// 回滚发布请求（恢复最近一次成功版本）
#[derive(Debug, Clone, Deserialize, Default, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RollbackReleaseRequest {
    /// 回滚原因（可选；记入失败版 failure_message 供追溯）
    #[schema(example = "排查后放弃 v1.2.0，回退")]
    pub message: Option<String>,
}

/// 发布状态机
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
/// 序列化值小写（与 userapp_publish 任务状态口径一致：active/failed/completed…）；
/// variant `alias` 兼容读存量 index.json 的 PascalCase 旧值（开发阶段数据，升级不
/// 要求清理），写出一律小写。
#[serde(rename_all = "lowercase")]
pub enum ReleaseStatus {
    /// 已下载校验入库，未激活
    #[serde(alias = "Prepared")]
    Prepared,
    /// 兼容读旧 index 的遗留值（confirm 两段式时代的"待确认"态）；读时归一化为 Failed，
    /// 新代码不再产生。
    #[serde(alias = "PendingStart")]
    PendingStart,
    /// 当前生效版本
    #[serde(alias = "Active")]
    Active,
    /// 激活失败（现场保留：code=失败版、.rollback=上一版、制品包保留）
    #[serde(alias = "Failed")]
    Failed,
}

/// 单个 release 记录
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    /// 发布 ID
    #[schema(example = "rel-20260814-001")]
    pub release_id: String,
    /// 制品 sha256（64 位十六进制）
    pub sha256: String,
    /// 制品字节数
    pub size_bytes: u64,
    /// 发布状态（Prepared/Active/Failed）
    pub status: ReleaseStatus,
    /// 入库时间（RFC3339）
    #[schema(example = "2026-08-14T10:30:00+08:00")]
    pub created_at: String,
    /// 转正（Active）时间（RFC3339；未激活为 null）
    pub activated_at: Option<String>,
    /// 失败/中止原因（非 Failed 为 null）
    pub failure_message: Option<String>,
}

/// release 索引持久化结构（releases/index.json，仅内部使用不入 OpenAPI）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseIndex {
    pub active_release_id: Option<String>,
    /// 兼容字段：confirm 两段式时代的"待确认 release"；新代码恒写 None（读旧 index
    /// 经 [`ReleaseIndex::normalize_legacy_pending`] 归一化）。
    pub pending_release_id: Option<String>,
    pub previous_release_id: Option<String>,
    pub retention: u16,
    pub releases: Vec<ReleaseInfo>,
}

impl ReleaseIndex {
    /// 旧 index 兼容：PendingStart 行归一化为 Failed（confirm 语义已内化进 activate，
    /// 升级前卡在待确认的发布按"失败保留现场"处理，rollback 接口可恢复其 `.rollback` 快照）。
    /// 纯内存变换，不写文件（下次写 index 自然持久化归一化结果）。
    pub fn normalize_legacy_pending(&mut self) {
        for release in &mut self.releases {
            if release.status == ReleaseStatus::PendingStart {
                release.status = ReleaseStatus::Failed;
                release.failure_message =
                    Some("legacy PendingStart normalized to Failed (confirm flow removed)".into());
            }
        }
        self.pending_release_id = None;
    }
}

/// release 列表响应（读 releases/index.json）
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseListResponse {
    /// 当前生效 release ID（无则 null）
    pub active_release_id: Option<String>,
    /// 最近一次激活失败的 release ID（无则 null；恢复走 rollback 接口或重新 activate）
    pub last_failed_release_id: Option<String>,
    /// 当前保留份数策略
    pub retention: u16,
    /// 保留策略内的 release 列表（新→旧）
    pub releases: Vec<ReleaseInfo>,
}
