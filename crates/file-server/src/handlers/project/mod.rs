//! `/api/project` HTTP handlers (对齐 nuwax projectRoutes + codeRoutes)。
//!
//! 拆分: [`content`] (get-project-content / get-by-version) / [`crud`] (create / copy /
//! delete) / [`upload`] (upload-single / batch / attachment / project) /
//! [`skills`] (push-skills-to-workspace) / [`code`] (specified/all-files-update) /
//! [`version`] (backup / rollback / export)。本 mod.rs 仅提供跨组共享 helper。

use crate::workspace::ProjectContext;

pub(crate) mod code;
pub(crate) mod content;
pub(crate) mod crud;
pub(crate) mod skills;
pub(crate) mod upload;
pub(crate) mod version;

// ── 跨组共享 helper (子模块经 super:: 访问) ──────────────────────────────────────

fn ctx_from(
    project_id: &str,
    tenant: Option<String>,
    space: Option<String>,
    iso: Option<String>,
) -> ProjectContext {
    ProjectContext {
        project_id: project_id.to_string(),
        tenant_id: tenant,
        space_id: space,
        isolation_type: iso,
    }
}
