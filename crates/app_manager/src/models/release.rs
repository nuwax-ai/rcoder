//! 应用发布模型（prepare/activate/confirm/abort 请求与 release 记录）

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
    /// 等待新版本就绪的超时秒数（可选；超时按失败处理走回滚）
    #[schema(example = 120)]
    pub readiness_timeout_seconds: Option<u64>,
}

/// 确认发布健康结果请求
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmReleaseRequest {
    /// 健康结论：true=提交（转 Active）；false=回滚到上一 Active 并置 Failed
    #[schema(example = true)]
    pub healthy: bool,
    /// 附加说明（可选；false 时作为 failure_message 记录）
    #[schema(example = "健康检查通过")]
    pub message: Option<String>,
}

/// 中止 pending 发布请求（运维自救）
#[derive(Debug, Clone, Deserialize, Default, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AbortReleaseRequest {
    /// 中止原因（可选；记入 failure_message，缺省 "release aborted"）
    #[schema(example = "confirm 回滚失败后手动中止")]
    pub message: Option<String>,
}

/// 发布状态机
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum ReleaseStatus {
    /// 已下载校验入库，未激活
    Prepared,
    /// 激活中，等待 confirm 健康确认
    PendingStart,
    /// 当前生效版本
    Active,
    /// 失败或已中止
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
    /// 发布状态（Prepared/PendingStart/Active/Failed）
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
    pub pending_release_id: Option<String>,
    pub previous_release_id: Option<String>,
    pub retention: u16,
    pub releases: Vec<ReleaseInfo>,
}

/// release 列表响应（读 releases/index.json）
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseListResponse {
    /// 当前生效 release ID（无则 null）
    pub active_release_id: Option<String>,
    /// 待确认（pending）release ID（无则 null）
    pub pending_release_id: Option<String>,
    /// 当前保留份数策略
    pub retention: u16,
    /// 保留策略内的 release 列表（新→旧）
    pub releases: Vec<ReleaseInfo>,
}
