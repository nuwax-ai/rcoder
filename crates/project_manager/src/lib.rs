use anyhow::{Context, Result};
use shared_types::{Project, CreateProjectRequest, FileChange, FileChangeType};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use tokio::fs;
use tracing::{debug, info};
use uuid::Uuid;

pub mod database;
pub mod project_store;

pub use database::DatabaseManager;
pub use project_store::ProjectStore;

pub struct ProjectManager {
    db: DatabaseManager,
    project_store: ProjectStore,
    projects: dashmap::DashMap<Uuid, Project>,
}

impl ProjectManager {
    pub async fn new(database_url: &str) -> Result<Self> {
        info!("Initializing Project Manager");

        let db = DatabaseManager::new(database_url).await?;
        let project_store = ProjectStore::new(db.pool());

        // Run database migrations
        db.run_migrations().await?;

        let manager = Self {
            db,
            project_store,
            projects: dashmap::DashMap::new(),
        };

        // Load existing projects from database
        manager.load_projects().await?;

        info!("Project Manager initialized successfully");
        Ok(manager)
    }

    async fn load_projects(&self) -> Result<()> {
        info!("Loading existing projects from database");

        let projects = self.project_store.list_projects().await?;

        for project in projects {
            self.projects.insert(project.id, project.clone());
            debug!("Loaded project: {} ({})", project.name, project.id);
        }

        info!("Loaded {} projects", self.projects.len());
        Ok(())
    }

    pub async fn create_project(&self, request: CreateProjectRequest) -> Result<Project> {
        info!("Creating project: {}", request.name);

        let project_id = Uuid::new_v4();
        let project_path = self.resolve_project_path(&request.name, request.path.as_ref())?;

        // Verify project path doesn't exist
        if project_path.exists() {
            return Err(anyhow::anyhow!("Project path already exists: {:?}", project_path));
        }

        // Create project directory
        fs::create_dir_all(&project_path).await
            .context("Failed to create project directory")?;

        // Create project record
        let project = Project {
            id: project_id,
            name: request.name.clone(),
            path: project_path.clone(),
            description: request.description.clone(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        // Save to database
        self.project_store.create_project(&project).await?;

        // Cache in memory
        self.projects.insert(project_id, project.clone());

        // File watching removed - no longer needed

        info!("Project created successfully: {} ({})", project.name, project.id);
        Ok(project)
    }

    pub async fn get_project(&self, project_id: Uuid) -> Option<Project> {
        self.projects.get(&project_id).map(|p| p.clone())
    }

    pub async fn list_projects(&self) -> Vec<Project> {
        self.projects.iter().map(|p| p.clone()).collect()
    }

    pub async fn update_project(&self, project_id: Uuid, updates: &ProjectUpdate) -> Result<Project> {
        info!("Updating project: {}", project_id);

        let project = self.projects.get(&project_id)
            .ok_or_else(|| anyhow::anyhow!("Project not found: {}", project_id))?;

        let mut updated_project = project.clone();
        let mut updated = false;

        if let Some(name) = &updates.name {
            if name != &updated_project.name {
                updated_project.name = name.clone();
                updated = true;
            }
        }

        if let Some(description) = &updates.description {
            if updated_project.description.as_ref() != Some(description) {
                updated_project.description = Some(description.clone());
                updated = true;
            }
        }

        if updated {
            updated_project.updated_at = chrono::Utc::now();
            self.project_store.update_project(&updated_project).await?;
            self.projects.insert(project_id, updated_project.clone());
            info!("Project updated: {}", project_id);
        }

        Ok(updated_project)
    }

    pub async fn delete_project(&self, project_id: Uuid) -> Result<()> {
        info!("Deleting project: {}", project_id);

        let (_, project) = self.projects.remove(&project_id)
            .ok_or_else(|| anyhow::anyhow!("Project not found: {}", project_id))?;

        // File watching removed - no longer needed

        // Delete project files
        if project.path.exists() {
            fs::remove_dir_all(&project.path).await
                .context("Failed to remove project directory")?;
        }

        // Delete from database
        self.project_store.delete_project(project_id).await?;

        info!("Project deleted successfully: {}", project_id);
        Ok(())
    }

    pub async fn apply_file_changes(&self, project_id: Uuid, changes: Vec<FileChange>) -> Result<()> {
        info!("Applying {} file changes to project {}", changes.len(), project_id);

        let project = self.projects.get(&project_id)
            .ok_or_else(|| anyhow::anyhow!("Project not found: {}", project_id))?;

        for change in changes {
            self.apply_single_file_change(&project, change).await?;
        }

        // Update project timestamp
        let mut updated_project = project.clone();
        updated_project.updated_at = chrono::Utc::now();
        self.project_store.update_project(&updated_project).await?;
        self.projects.insert(project_id, updated_project);

        info!("File changes applied successfully to project {}", project_id);
        Ok(())
    }

    async fn apply_single_file_change(&self, project: &Project, change: FileChange) -> Result<()> {
        let file_path = project.path.join(&change.path);

        match change.change_type {
            FileChangeType::Created => {
                if let Some(content) = change.content {
                    // Create parent directories if needed
                    if let Some(parent) = file_path.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    fs::write(&file_path, content).await
                        .context("Failed to create file")?;
                    debug!("Created file: {:?}", file_path);
                }
            }
            FileChangeType::Modified => {
                if let Some(content) = change.content {
                    fs::write(&file_path, content).await
                        .context("Failed to modify file")?;
                    debug!("Modified file: {:?}", file_path);
                }
            }
            FileChangeType::Deleted => {
                if file_path.exists() {
                    fs::remove_file(&file_path).await
                        .context("Failed to delete file")?;
                    debug!("Deleted file: {:?}", file_path);
                }
            }
        }

        Ok(())
    }

    pub async fn get_project_files(&self, project_id: Uuid) -> Result<Vec<PathBuf>> {
        let project = self.projects.get(&project_id)
            .ok_or_else(|| anyhow::anyhow!("Project not found: {}", project_id))?;

        self.scan_project_files(&project.path).await
    }

    async fn scan_project_files(&self, project_path: &Path) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();

        let mut entries = fs::read_dir(project_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            if path.is_dir() {
                // Skip hidden directories and common build directories
                if path.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.') || ["target", "node_modules", "dist", "build"].contains(&n))
                    .unwrap_or(false)
                {
                    continue;
                }

                // Recursively scan subdirectories
                let sub_files = Box::pin(self.scan_project_files(&path)).await?;
                files.extend(sub_files);
            } else {
                // Only include text files
                if path.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| {
                        matches!(ext,
                            "rs" | "js" | "ts" | "py" | "json" | "toml" | "yaml" | "yml" |
                            "md" | "txt" | "html" | "css" | "sql" | "sh" | "dockerfile"
                        )
                    })
                    .unwrap_or(false)
                {
                    files.push(path);
                }
            }
        }

        Ok(files)
    }

    fn resolve_project_path(&self, name: &str, custom_path: Option<&PathBuf>) -> Result<PathBuf> {
        if let Some(path) = custom_path {
            Ok(path.clone())
        } else {
            // Use a fixed projects directory for auto-created projects
            let base_path = std::env::current_dir()?.join("projects");
            std::fs::create_dir_all(&base_path)?;
            let sanitized_name = name.to_lowercase()
                .replace(|c: char| !c.is_alphanumeric() && c != '_' && c != '-', "_");
            Ok(base_path.join(sanitized_name))
        }
    }

    pub async fn get_project_stats(&self, project_id: Uuid) -> Result<ProjectStats> {
        let project = self.projects.get(&project_id)
            .ok_or_else(|| anyhow::anyhow!("Project not found: {}", project_id))?;

        let files = self.get_project_files(project_id).await?;
        let total_files = files.len();
        let mut file_types = HashMap::new();

        for file_path in files {
            if let Some(ext) = file_path.extension().and_then(|e| e.to_str()) {
                *file_types.entry(ext.to_string()).or_insert(0) += 1;
            }
        }

        let project_size = self.calculate_project_size(&project.path).await?;

        Ok(ProjectStats {
            total_files,
            file_types,
            project_size,
            created_at: project.created_at,
            updated_at: project.updated_at,
        })
    }

    async fn calculate_project_size(&self, project_path: &Path) -> Result<u64> {
        let mut total_size = 0;

        let mut entries = fs::read_dir(project_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = fs::metadata(&path).await?;

            if metadata.is_file() {
                total_size += metadata.len();
            } else if metadata.is_dir() {
                total_size += Box::pin(self.calculate_directory_size(&path)).await?;
            }
        }

        Ok(total_size)
    }

    async fn calculate_directory_size(&self, dir_path: &Path) -> Result<u64> {
        let mut total_size = 0;

        let mut entries = fs::read_dir(dir_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = fs::metadata(&path).await?;

            if metadata.is_file() {
                total_size += metadata.len();
            } else if metadata.is_dir() {
                total_size += Box::pin(self.calculate_directory_size(&path)).await?;
            }
        }

        Ok(total_size)
    }
}

#[derive(Debug)]
pub struct ProjectUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct ProjectStats {
    pub total_files: usize,
    pub file_types: HashMap<String, usize>,
    pub project_size: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}