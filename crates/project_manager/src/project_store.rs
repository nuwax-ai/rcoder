use anyhow::{Context, Result};
use shared_types::Project;
use sqlx::{SqlitePool, Row};
use tracing::{debug, info};
use uuid::Uuid;

pub struct ProjectStore {
    pool: SqlitePool,
}

impl ProjectStore {
    pub fn new(pool: &SqlitePool) -> Self {
        Self {
            pool: pool.clone(),
        }
    }

    pub async fn create_project(&self, project: &Project) -> Result<()> {
        info!("Creating project in database: {}", project.name);

        sqlx::query(
            r#"
            INSERT INTO projects (id, name, path, description, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(project.id)
        .bind(&project.name)
        .bind(project.path.to_string_lossy().as_ref())
        .bind(&project.description)
        .bind(project.created_at)
        .bind(project.updated_at)
        .execute(&self.pool)
        .await
        .context("Failed to create project in database")?;

        debug!("Project created in database: {}", project.id);
        Ok(())
    }

    pub async fn get_project(&self, project_id: Uuid) -> Result<Option<Project>> {
        debug!("Getting project from database: {}", project_id);

        let row = sqlx::query(
            r#"
            SELECT id, name, path, description, created_at, updated_at
            FROM projects
            WHERE id = ?
            "#,
        )
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get project from database")?;

        match row {
            Some(row) => {
                let project = Project {
                    id: row.get("id"),
                    name: row.get("name"),
                    path: row.get::<String, _>("path").into(),
                    description: row.get("description"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                };
                Ok(Some(project))
            }
            None => Ok(None),
        }
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        debug!("Listing all projects from database");

        let rows = sqlx::query(
            r#"
            SELECT id, name, path, description, created_at, updated_at
            FROM projects
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list projects from database")?;

        let projects: Vec<Project> = rows
            .into_iter()
            .map(|row| Project {
                id: row.get("id"),
                name: row.get("name"),
                path: row.get::<String, _>("path").into(),
                description: row.get("description"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect();

        debug!("Listed {} projects from database", projects.len());
        Ok(projects)
    }

    pub async fn update_project(&self, project: &Project) -> Result<()> {
        info!("Updating project in database: {}", project.id);

        sqlx::query(
            r#"
            UPDATE projects
            SET name = ?, path = ?, description = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(&project.name)
        .bind(project.path.to_string_lossy().as_ref())
        .bind(&project.description)
        .bind(project.updated_at)
        .bind(project.id)
        .execute(&self.pool)
        .await
        .context("Failed to update project in database")?;

        debug!("Project updated in database: {}", project.id);
        Ok(())
    }

    pub async fn delete_project(&self, project_id: Uuid) -> Result<()> {
        info!("Deleting project from database: {}", project_id);

        // Manual cascade delete - delete related records first
        // Delete project sessions
        sqlx::query(
            "DELETE FROM project_sessions WHERE project_id = ?"
        )
        .bind(project_id)
        .execute(&self.pool)
        .await
        .context("Failed to delete project sessions")?;

        // Delete file changes
        sqlx::query(
            "DELETE FROM file_changes WHERE project_id = ?"
        )
        .bind(project_id)
        .execute(&self.pool)
        .await
        .context("Failed to delete file changes")?;

        // Delete prompts
        sqlx::query(
            "DELETE FROM prompts WHERE project_id = ?"
        )
        .bind(project_id)
        .execute(&self.pool)
        .await
        .context("Failed to delete prompts")?;

        // Delete the project itself
        sqlx::query(
            "DELETE FROM projects WHERE id = ?"
        )
        .bind(project_id)
        .execute(&self.pool)
        .await
        .context("Failed to delete project from database")?;

        debug!("Project deleted from database: {}", project_id);
        Ok(())
    }

    pub async fn get_project_by_name(&self, name: &str) -> Result<Option<Project>> {
        debug!("Getting project by name from database: {}", name);

        let row = sqlx::query(
            r#"
            SELECT id, name, path, description, created_at, updated_at
            FROM projects
            WHERE name = ?
            "#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get project by name from database")?;

        match row {
            Some(row) => {
                let project = Project {
                    id: row.get("id"),
                    name: row.get("name"),
                    path: row.get::<String, _>("path").into(),
                    description: row.get("description"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                };
                Ok(Some(project))
            }
            None => Ok(None),
        }
    }

    pub async fn get_projects_by_path(&self, path: &std::path::Path) -> Result<Vec<Project>> {
        debug!("Getting projects by path from database: {:?}", path);

        let path_str = path.to_string_lossy();
        let rows = sqlx::query(
            r#"
            SELECT id, name, path, description, created_at, updated_at
            FROM projects
            WHERE path = ?
            ORDER BY created_at DESC
            "#,
        )
        .bind(path_str.as_ref())
        .fetch_all(&self.pool)
        .await
        .context("Failed to get projects by path from database")?;

        let projects: Vec<Project> = rows
            .into_iter()
            .map(|row| Project {
                id: row.get("id"),
                name: row.get("name"),
                path: row.get::<String, _>("path").into(),
                description: row.get("description"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect();

        debug!("Found {} projects by path", projects.len());
        Ok(projects)
    }

    pub async fn search_projects(&self, query: &str) -> Result<Vec<Project>> {
        debug!("Searching projects in database: {}", query);

        let search_pattern = format!("%{}%", query);
        let rows = sqlx::query(
            r#"
            SELECT id, name, path, description, created_at, updated_at
            FROM projects
            WHERE name LIKE ? OR description LIKE ?
            ORDER BY created_at DESC
            "#,
        )
        .bind(&search_pattern)
        .bind(&search_pattern)
        .fetch_all(&self.pool)
        .await
        .context("Failed to search projects in database")?;

        let projects: Vec<Project> = rows
            .into_iter()
            .map(|row| Project {
                id: row.get("id"),
                name: row.get("name"),
                path: row.get::<String, _>("path").into(),
                description: row.get("description"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect();

        debug!("Found {} projects matching '{}'", projects.len(), query);
        Ok(projects)
    }

    // Helper method to check if a project exists before creating related records
    pub async fn project_exists(&self, project_id: Uuid) -> Result<bool> {
        debug!("Checking if project exists: {}", project_id);

        let result = sqlx::query(
            "SELECT COUNT(*) as count FROM projects WHERE id = ?"
        )
        .bind(project_id)
        .fetch_one(&self.pool)
        .await
        .context("Failed to check project existence")?;

        let count: i64 = result.get("count");
        Ok(count > 0)
    }

    // Helper method to clean up orphaned records (can be run periodically)
    pub async fn cleanup_orphaned_records(&self) -> Result<u32> {
        info!("Cleaning up orphaned records");

        let mut total_deleted = 0;

        // Clean up orphaned project sessions
        let result = sqlx::query(
            r#"
            DELETE FROM project_sessions
            WHERE project_id NOT IN (SELECT id FROM projects)
            "#
        )
        .execute(&self.pool)
        .await
        .context("Failed to clean up orphaned project sessions")?;

        total_deleted += result.rows_affected();

        // Clean up orphaned file changes
        let result = sqlx::query(
            r#"
            DELETE FROM file_changes
            WHERE project_id NOT IN (SELECT id FROM projects)
            "#
        )
        .execute(&self.pool)
        .await
        .context("Failed to clean up orphaned file changes")?;

        total_deleted += result.rows_affected();

        // Clean up orphaned prompts
        let result = sqlx::query(
            r#"
            DELETE FROM prompts
            WHERE project_id NOT IN (SELECT id FROM projects)
            "#
        )
        .execute(&self.pool)
        .await
        .context("Failed to clean up orphaned prompts")?;

        total_deleted += result.rows_affected();

        info!("Cleaned up {} orphaned records", total_deleted);
        Ok(total_deleted as u32)
    }
}