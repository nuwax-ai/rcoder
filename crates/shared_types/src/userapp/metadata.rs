//! Userapp 应用业务元数据（trait）
//!
//! 支撑 `POST /apps/query` 的 name/created_at 过滤（PG 模式）：这两个字段是业务元数据，
//! **集群不持有**（rcoder 无状态读路径拿不到），故由本契约持久化到 PG `userapp_metadata` 表。
//!
//! 设计边界（与现有 5 张 PG 表的哲学一致）：
//! - 只存集群确实没有的字段；desired 运行字段（image/env/resources/recycle 注解等）
//!   以 K8s/Docker 集群为事实源，本契约**不镜像**，避免三事实源漂移。
//! - 删除对齐 Userapp 三档语义：`delete`（默认）/`purge:true` **保留**行（误删找回后元数据仍在），
//!   仅 `storage/destroy`（PVC 不可逆销毁）时删行。

/// Userapp 业务元数据行（app_manager 产出/消费 ↔ 存储后端的数据载体）
#[derive(Debug, Clone)]
pub struct AppMetadataRecord {
    /// Userapp 应用 ID（app- 前缀，与集群资源名一致）
    pub app_id: String,
    /// 业务名称（仅元数据，集群不持有）
    pub name: Option<String>,
    /// 归属用户 ID（仅元数据，集群不持有；部署访问 URL `/proxy/userapp/prod/{user_id}/...`
    /// 与未来"我的应用"过滤/归属校验的数据源。存量行可空）
    pub user_id: Option<String>,
    /// 租户 ID（冗余自资源 label，便于查询过滤）
    pub tenant_id: Option<String>,
    /// 空间 ID（同上）
    pub space_id: Option<String>,
    /// 业务首次创建时间（upsert 不更新；集群 creationTimestamp 同 app_id 重建会刷新，此列不会）
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 应用元数据的持久化契约（跨 crate：app_manager 产出/消费，rcoder-storage 实现）
///
/// create_app/update_app 成功后 upsert（低频同步写，失败仅 warn 不阻塞业务）；
/// 启动时全量加载回内存缓存供 query_apps join。实现须保证幂等（upsert）。
#[async_trait::async_trait]
pub trait AppMetadataPersistence: Send + Sync {
    /// upsert 元数据行（ON CONFLICT 不更新 created_at——业务首次创建时间不可变）
    async fn upsert(&self, record: &AppMetadataRecord) -> anyhow::Result<()>;

    /// 全量加载（启动时调用；空表返回空 Vec）
    async fn load_all(&self) -> anyhow::Result<Vec<AppMetadataRecord>>;

    /// 删除单行（storage/destroy 后调用；delete/purge 保留行）
    async fn delete(&self, app_id: &str) -> anyhow::Result<()>;
}
