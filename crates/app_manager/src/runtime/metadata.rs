//! Authoritative userApp metadata projection. No process-local ownership cache.
use std::{collections::HashMap, sync::Arc};

use crate::models::{AppOperationError, AppResult};
use shared_types::{
    AppMetadataRecord, UserAppLifecycleRecord, UserAppLifecycleState, UserAppLifecycleStore,
};

#[cfg(test)]
use shared_types::UserAppMetadataPatch;

pub(crate) struct AppMetadataStore {
    pub(crate) store: Arc<dyn UserAppLifecycleStore>,
}

impl AppMetadataStore {
    /// Validate the caller's lifecycle token; never manufacture it from the
    /// current row for an external request. Admission still performs the final CAS.
    pub(crate) async fn validate_request_lifecycle(
        &self,
        app_id: &str,
        expected_lifecycle: Option<&str>,
    ) -> AppResult<()> {
        match self.store.get_application(app_id).await? {
            Some(app) => {
                if app.state != UserAppLifecycleState::Active
                    || expected_lifecycle.is_some_and(|expected| expected != app.lifecycle_id)
                    || (app.lifecycle_epoch > 1 && expected_lifecycle.is_none())
                {
                    return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
                }
                Ok(())
            }
            None if expected_lifecycle.is_some() => {
                Err(shared_types::UserAppStoreError::LifecycleConflict.into())
            }
            None => Ok(()),
        }
    }

    pub(crate) fn new(store: Arc<dyn UserAppLifecycleStore>) -> Self {
        Self { store }
    }

    /// None preserves a field. Explicit clear operations use UserAppMetadataPatch.
    #[cfg(test)]
    pub async fn record(
        &self,
        app_id: &str,
        name: Option<String>,
        tenant_id: Option<String>,
        space_id: Option<String>,
    ) -> AppResult<()> {
        let app = self.store.ensure_identity(app_id).await?;
        if name.is_none() && tenant_id.is_none() && space_id.is_none() {
            return Ok(());
        }
        self.store
            .patch_metadata(&UserAppMetadataPatch {
                app_id: app_id.into(),
                lifecycle_id: app.lifecycle_id,
                expected_revision: app.metadata_revision,
                name: name.map(Some),
                tenant_id: tenant_id.map(Some),
                space_id: space_id.map(Some),
            })
            .await?;
        Ok(())
    }

    pub async fn lookup(&self, app_id: &str) -> AppResult<Option<AppMetadataRecord>> {
        Ok(self
            .store
            .get_application(app_id)
            .await?
            .filter(|app| app.state != UserAppLifecycleState::Deleted)
            .map(project))
    }

    /// A bounded page per query, materialized before any runtime request or sort.
    pub async fn snapshot(&self) -> AppResult<HashMap<String, AppMetadataRecord>> {
        let mut result = HashMap::new();
        let mut cursor = None;
        loop {
            let page = self.store.list_applications(cursor.as_deref(), 256).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map(|app| app.app_id.clone());
            for app in page {
                if app.state != UserAppLifecycleState::Deleted {
                    result.insert(app.app_id.clone(), project(app));
                }
            }
        }
        Ok(result)
    }
}

fn project(app: UserAppLifecycleRecord) -> AppMetadataRecord {
    AppMetadataRecord {
        generation: app.lifecycle_id,
        app_id: app.app_id,
        name: app.name,
        tenant_id: app.tenant_id,
        space_id: app.space_id,
        created_at: app.created_at,
    }
}

impl crate::service::AppService {
    pub fn set_dev_cleanup(
        &self,
        cleanup: Arc<dyn shared_types::UserappDevCleanup>,
    ) -> AppResult<()> {
        *self.dev_cleanup.write().map_err(|_| {
            AppOperationError::Backend("Userapp dev cleanup lock poisoned".into())
        })? = Some(cleanup);
        Ok(())
    }
    pub fn set_dev_locator(
        &self,
        locator: Arc<dyn shared_types::UserappDevLocator>,
    ) -> AppResult<()> {
        *self.dev_locator.write().map_err(|_| {
            AppOperationError::Backend("Userapp dev locator lock poisoned".into())
        })? = Some(locator);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcoder_storage::userapp_lifecycle::SqliteUserAppStore;

    #[tokio::test]
    async fn independent_readers_observe_committed_metadata_and_noop_keeps_revision() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("userapp.sqlite3");
        let first = Arc::new(SqliteUserAppStore::open(&path).await.expect("first store"));
        let second = Arc::new(SqliteUserAppStore::open(&path).await.expect("second store"));
        let writer = AppMetadataStore::new(first.clone());
        let reader = AppMetadataStore::new(second.clone());
        writer
            .record("a", Some("alpha".into()), None, None)
            .await
            .expect("record");
        assert_eq!(
            reader
                .lookup("a")
                .await
                .expect("read")
                .expect("present")
                .name
                .as_deref(),
            Some("alpha")
        );
        let before = second
            .get_application("a")
            .await
            .expect("read")
            .expect("present");
        writer
            .record("a", Some("alpha".into()), None, None)
            .await
            .expect("noop");
        let after = second
            .get_application("a")
            .await
            .expect("read")
            .expect("present");
        assert_eq!(before, after);
        writer
            .record("a", Some("beta".into()), None, None)
            .await
            .expect("update");
        assert_eq!(
            reader.snapshot().await.expect("snapshot")["a"]
                .name
                .as_deref(),
            Some("beta")
        );
        first.close().await;
        second.close().await;
        let reopened = Arc::new(SqliteUserAppStore::open(&path).await.expect("reopen"));
        assert_eq!(
            AppMetadataStore::new(reopened.clone())
                .lookup("a")
                .await
                .expect("read")
                .expect("present")
                .name
                .as_deref(),
            Some("beta")
        );
        reopened.close().await;
    }

    #[tokio::test]
    async fn storage_failure_is_neither_absence_nor_successful_registration() {
        let directory = tempfile::tempdir().expect("directory");
        let store = Arc::new(
            SqliteUserAppStore::open(&directory.path().join("userapp.sqlite3"))
                .await
                .expect("store"),
        );
        let metadata = AppMetadataStore::new(store.clone());
        metadata
            .record("a", None, None, None)
            .await
            .expect("record");
        store.close().await;
        assert!(matches!(
            metadata.lookup("a").await,
            Err(AppOperationError::Backend(_))
        ));
        assert!(matches!(
            metadata.record("b", None, None, None).await,
            Err(AppOperationError::Backend(_))
        ));
        assert!(metadata.snapshot().await.is_err());
    }
}
