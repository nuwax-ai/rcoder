//! Keep a project's migration receipt location stable. app-cli reads all known
//! locations and serializes migration execution across them; choosing a directory
//! by counts or by its pending/completed contents can hide another receipt.
use std::path::Path;

use super::external_store::MigrationLocationRecord;
use super::types::DevServerManager;
use crate::error::{AppError, AppResult};

impl DevServerManager {
    pub(super) fn apply_migration_receipts_binding(
        &self,
        project_id: &str,
        workspace: &Path,
        explicit_root: Option<&Path>,
        env_extra: &mut Vec<(String, String)>,
    ) -> AppResult<()> {
        let result = (|| -> anyhow::Result<_> {
            let origin = runtime_state_layout::resolve_project_origin(workspace)?;
            let default = match explicit_root {
                Some(root) => root.join("migration-receipts"),
                None => workspace
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("workspace has no parent"))?
                    .join("migration-receipts"),
            };
            self.external_transaction(|state| {
                let location = state
                    .migration_locations
                    .get(project_id)
                    .filter(|record| {
                        record.version == 1
                            && record.origin == origin.to_string_lossy()
                            && !record.receipts_dir.is_empty()
                    })
                    .map(|record| record.receipts_dir.clone())
                    .unwrap_or_else(|| default.display().to_string());
                state.migration_locations.insert(
                    project_id.into(),
                    MigrationLocationRecord {
                        version: 1,
                        origin: origin.display().to_string(),
                        receipts_dir: location.clone(),
                    },
                );
                Ok(location)
            })
        })()
        .map_err(|error| AppError::owner_error("bind migration receipt location", error))?;
        env_extra.push(("APP_CLI_MIGRATION_RECEIPTS_DIR".into(), result));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn binding_survives_a_new_state_root_without_reassigning_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = crate::Config {
            log_base_dir: dir.path().join("logs"),
            ..Default::default()
        };
        let manager = DevServerManager::new(Arc::new(config.clone()));
        let old_root = dir.path().join("old");
        let new_root = dir.path().join("new");
        let mut before = Vec::new();
        manager
            .apply_migration_receipts_binding("project", &workspace, Some(&old_root), &mut before)
            .unwrap();
        drop(manager);
        let restored = DevServerManager::new(Arc::new(config));
        let mut after = Vec::new();
        restored
            .apply_migration_receipts_binding("project", &workspace, Some(&new_root), &mut after)
            .unwrap();
        assert_eq!(
            before, after,
            "restarting file-server must retain the recorded history location"
        );
        assert_eq!(
            after[0].1,
            old_root.join("migration-receipts").display().to_string()
        );
    }
}
