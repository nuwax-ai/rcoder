//! UserApp 应用业务元数据缓存 + 影子持久化（extension-impl）。
//!
//! PG 模式下 `POST /apps/query` 的 name/created_at 过滤数据源：集群不持有这两个字段
//! （rcoder 无状态），由 PG `userapp_metadata` 表补充。内存 cache 全量镜像（启动
//! load_all），写路径低频同步 upsert（create_app/update_app 成功后，失败仅 warn）。
//! 删除对齐三档语义：delete/purge 保留行（误删找回），storage/destroy 删行。
//!
//! 纯内存模式（Docker Compose）：cache 为空、persistence None——query 的
//! name/created_at 过滤维持忽略 + warn（旧行为）。

use std::sync::Arc;

use dashmap::DashMap;
use shared_types::AppMetadataRecord;
use tokio::sync::OnceCell;
use tracing::warn;

/// 应用元数据缓存（AppService 持有；进程内单例语义，跨副本由 PG 承载）。
#[derive(Default)]
pub(crate) struct AppMetadataStore {
    cache: DashMap<String, AppMetadataRecord>,
    persistence: OnceCell<Arc<dyn shared_types::AppMetadataPersistence>>,
}

impl AppMetadataStore {
    /// 注入影子持久化（PG 模式 main 在 AppService 构造后调用）。
    pub fn set_persistence(&self, p: Arc<dyn shared_types::AppMetadataPersistence>) {
        if self.persistence.set(p).is_err() {
            warn!("[APP_METADATA] set_persistence called twice; keeping existing");
        }
    }

    /// 已注入的持久化（None=纯内存模式，name/created_at 过滤不可用）
    pub fn persistence(&self) -> Option<Arc<dyn shared_types::AppMetadataPersistence>> {
        self.persistence.get().cloned()
    }

    /// 启动恢复：PG 全量加载写入内存 cache。
    pub fn apply_loaded(&self, rows: Vec<AppMetadataRecord>) {
        for row in rows {
            self.cache.insert(row.app_id.clone(), row);
        }
    }

    /// create/update 成功后记录元数据（cache 写 + PG upsert 失败仅 warn 不阻塞业务）。
    ///
    /// `created_at`：PG 侧 ON CONFLICT 不更新该列（仅首次 insert 生效）；内存 cache
    /// 此前每次用 now() 整行覆盖——改一次名 cache 里的创建时间就被刷新，query 的
    /// created_at 过滤随之漂移、重启后又回退为 PG 原值。现与 PG 对齐：cache 命中
    /// 旧记录时回填原 created_at，仅首次插入用 now()。
    pub async fn record(
        &self,
        app_id: &str,
        name: Option<String>,
        user_id: Option<String>,
        tenant_id: Option<String>,
        space_id: Option<String>,
    ) {
        let created_at = self
            .cache
            .get(app_id)
            .map(|existing| existing.created_at)
            .unwrap_or_else(chrono::Utc::now);
        let row = AppMetadataRecord {
            app_id: app_id.to_string(),
            name,
            user_id,
            tenant_id,
            space_id,
            created_at,
        };
        if let Some(p) = self.persistence()
            && let Err(e) = p.upsert(&row).await
        {
            warn!("[APP_METADATA] upsert failed app_id={app_id} (query filters may be stale): {e}");
        }
        self.cache.insert(app_id.to_string(), row);
    }

    /// storage/destroy 后删行（cache 移除 + PG delete 失败仅 warn）。
    pub async fn record_deleted(&self, app_id: &str) {
        if let Some(p) = self.persistence()
            && let Err(e) = p.delete(app_id).await
        {
            warn!("[APP_METADATA] delete failed app_id={app_id}: {e}");
        }
        self.cache.remove(app_id);
    }

    /// query join：按 app_id 取元数据（cache miss = 该 app 无业务元数据记录）。
    pub fn lookup(&self, app_id: &str) -> Option<AppMetadataRecord> {
        self.cache.get(app_id).map(|r| r.clone())
    }
}

impl crate::service::AppService {
    /// 注入元数据持久化（PG 模式 main 在 AppService 构造后调用）。
    pub fn set_metadata_persistence(&self, p: Arc<dyn shared_types::AppMetadataPersistence>) {
        self.metadata.set_persistence(p);
    }

    /// 启动恢复：PG 全量加载元数据进内存镜像（query join 数据源）。
    pub fn apply_metadata_loaded(&self, rows: Vec<AppMetadataRecord>) {
        let count = rows.len();
        self.metadata.apply_loaded(rows);
        tracing::info!("[APP_METADATA] userapp_metadata loaded: {count} rows");
    }

    /// 注入开发资源回收回调（宿主 rcoder 装配时调用；purge 回收 UserAppBuilder
    /// 开发容器与 per-app PVC，app_manager 自身 runtime 视图无 agent 能力）。
    pub fn set_dev_cleanup(&self, cleanup: Arc<dyn shared_types::UserappDevCleanup>) {
        *self.dev_cleanup.write().expect("dev_cleanup lock") = Some(cleanup);
    }

    /// 注入开发容器定位回调（宿主 rcoder 装配时调用；文件/存储接口 `env=dev`
    /// 分支经此幂等 ensure UserAppBuilder 并解析其 file-server 地址）。
    pub fn set_dev_locator(&self, locator: Arc<dyn shared_types::UserappDevLocator>) {
        *self.dev_locator.write().expect("dev_locator lock") = Some(locator);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::InMemoryMetadataPersistence;
    use shared_types::AppMetadataPersistence as _;

    #[tokio::test]
    async fn record_upserts_cache_and_persistence() {
        let persistence = InMemoryMetadataPersistence::new(vec![]);
        let store = AppMetadataStore::default();
        store.set_persistence(persistence.clone());
        store
            .record(
                "app-a",
                Some("alpha".into()),
                Some("u1".into()),
                Some("t1".into()),
                None,
            )
            .await;
        let meta = store.lookup("app-a").expect("cached after record");
        assert_eq!(meta.name.as_deref(), Some("alpha"));
        assert_eq!(meta.tenant_id.as_deref(), Some("t1"));
        assert!(meta.space_id.is_none());
        let rows = persistence.load_all().await.expect("persisted");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].app_id, "app-a");
    }

    /// update 不刷新 created_at：cache 命中旧记录回填原值（与 PG ON CONFLICT 语义
    /// 对齐——此前内存整行覆盖，改一次名创建时间就漂移）。
    #[tokio::test]
    async fn record_keeps_original_created_at_on_update() {
        let store = AppMetadataStore::default();
        store
            .record("app-ts", Some("first".into()), None, None, None)
            .await;
        let original = store.lookup("app-ts").expect("cached").created_at;

        // 让时间走一点，确保 now() 不同（chrono 精度足够分辨本测试的间隔）
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        store
            .record("app-ts", Some("renamed".into()), None, None, None)
            .await;
        let updated = store.lookup("app-ts").expect("still cached");
        assert_eq!(updated.name.as_deref(), Some("renamed"));
        assert_eq!(
            updated.created_at, original,
            "created_at must not be refreshed by update"
        );
    }

    #[tokio::test]
    async fn record_deleted_removes_cache_and_persists_delete() {
        let persistence = InMemoryMetadataPersistence::new(vec![AppMetadataRecord {
            app_id: "app-b".into(),
            name: Some("beta".into()),
            user_id: None,
            tenant_id: None,
            space_id: None,
            created_at: chrono::Utc::now(),
        }]);
        let store = AppMetadataStore::default();
        store.set_persistence(persistence.clone());
        store.record_deleted("app-b").await;
        assert!(store.lookup("app-b").is_none(), "cache cleared");
        let rows = persistence.load_all().await.expect("persisted");
        assert!(rows.iter().all(|r| r.app_id != "app-b"));
    }

    #[test]
    fn apply_loaded_populates_cache() {
        let store = AppMetadataStore::default();
        store.apply_loaded(vec![AppMetadataRecord {
            app_id: "app-c".into(),
            name: Some("gamma".into()),
            user_id: None,
            tenant_id: None,
            space_id: None,
            created_at: chrono::Utc::now(),
        }]);
        assert_eq!(
            store.lookup("app-c").and_then(|m| m.name),
            Some("gamma".into())
        );
        assert!(store.lookup("app-x").is_none());
    }
}
