//! 持久存储管理模型（v2 §5.4——删应用默认保留数据，由这组接口显式管理残留）

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 销毁 PVC 请求（高危·不可逆；强制 `confirm == app_id` 二次确认）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DestroyStorageRequest {
    /// 必须等于 path 的 `app_id`（防误调 / 防脚本批量误删 / 防重放）
    pub confirm: String,
}

/// 存储查询请求（**强制分页，无全量模式**——扫存储后端代价高）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueryStorageRequest {
    /// 页码（必填，从 1 开始）
    pub page: u32,
    /// 每页数量（必填，上限 100）
    pub page_size: u32,
    /// 过滤条件
    pub filters: Option<StorageFilters>,
}

/// 存储过滤条件
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageFilters {
    /// `true` = 只返回"有数据、无对应运行应用"的孤儿存储
    pub orphan_only: Option<bool>,
    /// 按 app_id 精确过滤（最省扫描）
    pub app_ids: Option<Vec<String>>,
    /// 按租户过滤
    pub tenant_id: Option<String>,
    /// 按空间过滤
    pub space_id: Option<String>,
}

/// 存储信息（**不含 `size_bytes`**——CephFS 上不能用 du，见设计文档 §5.4）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfo {
    /// 应用 ID
    pub app_id: String,
    /// 目录是否存在
    pub exists: bool,
    /// app 根路径（rcoder 视角）
    pub path: String,
    /// 最近修改时间（RFC3339）
    pub modified_at: Option<String>,
    /// 是否孤儿（无对应运行应用）
    pub is_orphan: bool,
}
