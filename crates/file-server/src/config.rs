//! 配置 (对齐 nuwax `appConfig/index.js`): 从环境变量读取 + 默认值。
//!
//! 仅含业务路径与开关; 工作区根 (project/computer) 由 [`crate::workspace::WorkspaceResolver`] 负责。

use std::path::PathBuf;

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"),
        Err(_) => default,
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_list(key: &str, default: &str) -> Vec<String> {
    std::env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 附件上传白名单 (对齐 nuwax `projectRoutes.js` ATTACHMENT_ALLOWED_EXTENSIONS, 硬编码)。
fn default_attachment_extensions() -> Vec<String> {
    [
        ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".txt", ".md",
        ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico", ".avif",
        ".zip", ".rar", ".7z", ".tar", ".gz", ".mp4", ".mov", ".avi", ".mp3", ".wav",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 全局配置 (启动时构造一次, 经 AppState 共享)。
#[derive(Clone, Debug)]
pub struct Config {
    // —— 业务目录 ——
    pub init_project_dir: PathBuf,
    pub upload_project_dir: PathBuf,
    pub dist_target_dir: PathBuf,
    pub log_base_dir: PathBuf,

    // —— 文件大小上限 ——
    pub max_inline_file_size_bytes: u64,
    pub upload_max_file_size_bytes: u64,
    pub upload_single_file_size_bytes: u64,
    pub download_max_file_size_bytes: u64,

    // —— 扩展名白名单 ——
    pub upload_allowed_extensions: Vec<String>,
    pub attachment_allowed_extensions: Vec<String>,

    // —— 遍历排除规则 ——
    pub traverse_exclude_dirs: Vec<String>,
    pub backup_traverse_exclude_files: Vec<String>,
    pub content_traverse_exclude_files: Vec<String>,
    pub inline_image_extensions: Vec<String>,
    pub zip_workspace_exclude: Vec<String>,

    // —— Git ——
    pub git_enabled: bool,
    pub git_default_author_name: String,
    pub git_default_author_email: String,

    // —— 模板 ——
    pub init_project_name_react: String,
    pub init_project_name_vue3: String,
}

impl Config {
    /// 从环境变量构造 (env 优先, 缺省用硬编码默认值)。
    pub fn from_env() -> Self {
        Self {
            init_project_dir: PathBuf::from(env_str("INIT_PROJECT_DIR", "/app/project_init")),
            upload_project_dir: PathBuf::from(env_str("UPLOAD_PROJECT_DIR", "/app/project_zips")),
            dist_target_dir: PathBuf::from(env_str("DIST_TARGET_DIR", "/app/project_nginx")),
            log_base_dir: PathBuf::from(env_str("LOG_BASE_DIR", "/app/logs/project_logs")),
            max_inline_file_size_bytes: env_u64("MAX_INLINE_FILE_SIZE_BYTES", 1_048_576),
            upload_max_file_size_bytes: env_u64("UPLOAD_MAX_FILE_SIZE_BYTES", 1_048_576_000),
            upload_single_file_size_bytes: env_u64("UPLOAD_SINGLE_FILE_SIZE_BYTES", 1_048_576_000),
            download_max_file_size_bytes: env_u64("DOWNLOAD_MAX_FILE_SIZE_BYTES", 104_857_600),
            upload_allowed_extensions: env_list("UPLOAD_ALLOWED_EXTENSIONS", ".zip"),
            attachment_allowed_extensions: default_attachment_extensions(),
            traverse_exclude_dirs: env_list(
                "TRAVERSE_EXCLUDE_DIRS",
                "dist,node_modules,.pnpm-store,__MACOSX,.attachments,.git,.agents,.codex,.opencode,.logs",
            ),
            backup_traverse_exclude_files: env_list(
                "BACKUP_TRAVERSE_EXCLUDE_FILES",
                "pnpm-lock.yaml,yarn.lock,package-lock.json",
            ),
            content_traverse_exclude_files: env_list(
                "CONTENT_TRAVERSE_EXCLUDE_FILES",
                "pnpm-lock.yaml,yarn.lock,package-lock.json",
            ),
            inline_image_extensions: env_list(
                "INLINE_IMAGE_EXTENSIONS",
                ".png,.jpg,.jpeg,.gif,.bmp,.svg,.ico,.webp,.avif",
            ),
            zip_workspace_exclude: env_list(
                "ZIP_WORKSPACE_EXCLUDE",
                ".git,.tmp,.claude,.agents,.codex,.opencode,.logs,.npmrc,__pycache__,node_modules,dist,pnpm-lock.yaml,yarn.lock,package-lock.json",
            ),
            git_enabled: env_bool("GIT_ENABLED", false),
            git_default_author_name: env_str("GIT_DEFAULT_AUTHOR_NAME", "Nuwax File Server"),
            git_default_author_email: env_str("GIT_DEFAULT_AUTHOR_EMAIL", "git@nuwax.com"),
            init_project_name_react: env_str("INIT_PROJECT_NAME_REACT", "react-vite-template"),
            init_project_name_vue3: env_str("INIT_PROJECT_NAME_VUE3", "vue3-vite-template"),
        }
    }

    /// 扩展名是否在白名单 (大小写不敏感)。
    pub fn ext_allowed(&self, list: &[String], ext: &str) -> bool {
        let lower = ext.to_lowercase();
        list.iter().any(|e| e.to_lowercase() == lower)
    }
}
