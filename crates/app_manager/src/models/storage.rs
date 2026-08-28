//! 持久存储管理模型（v2 §5.4——删应用默认保留数据，由这组接口显式管理残留）

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 清空应用持久存储请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ClearStorageRequest {
    /// 归属用户 ID（必填，白名单校验；dev 分支经容器清 workspace 时开发容器
    /// 懒创建的宿主树 `dev/{user_id}/{app_id}` 分区依据）
    pub user_id: String,
}

/// 销毁 PVC 请求（高危·不可逆；强制 `confirm == app_id` 二次确认）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DestroyStorageRequest {
    /// 归属用户 ID（必填，白名单校验；Docker compose 部署下销毁宿主树
    /// `prod/{user_id}/` 该 app 四目录的定位与审计依据——K8s 走 PVC 对象、
    /// Docker 走 prod/*/ 通配扫描兜底，显式值用于对账与未来精确直删）
    pub user_id: String,
    /// 必须等于 path 的 `app_id`（防误调 / 防脚本批量误删 / 防重放）
    pub confirm: String,
}

/// 存储查询请求（**强制分页，无全量模式**——扫存储后端代价高）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueryStorageRequest {
    /// 归属用户 ID（必填；审计留痕——存储清单按 owner 归属审计）
    pub user_id: String,
    /// 页码（必填，从 1 开始）
    pub page: u32,
    /// 每页数量（必填，上限 100）
    pub page_size: u32,
    /// 过滤条件
    pub filters: Option<StorageFilters>,
}

/// 存储过滤条件
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 契约：clear/destroy body 的 user_id 必填——缺字段即拒。
    #[test]
    fn storage_requests_require_user_id() {
        let clear: ClearStorageRequest =
            serde_json::from_value(serde_json::json!({"user_id": "u1"})).expect("clear body");
        assert_eq!(clear.user_id, "u1");
        assert!(
            serde_json::from_value::<ClearStorageRequest>(serde_json::json!({})).is_err(),
            "clear 缺 user_id 应拒"
        );

        let destroy: DestroyStorageRequest = serde_json::from_value(serde_json::json!({
            "user_id": "u1", "confirm": "app-1",
        }))
        .expect("destroy body");
        assert_eq!(destroy.user_id, "u1");
        assert!(
            serde_json::from_value::<DestroyStorageRequest>(serde_json::json!({
                "confirm": "app-1",
            }))
            .is_err(),
            "destroy 缺 user_id 应拒"
        );
    }
}
