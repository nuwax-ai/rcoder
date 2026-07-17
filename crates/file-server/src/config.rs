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

fn env_port(key: &str, default: u16) -> u16 {
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
        ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".txt", ".md", ".png", ".jpg",
        ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico", ".avif", ".zip", ".rar", ".7z", ".tar",
        ".gz", ".mp4", ".mov", ".avi", ".mp3", ".wav",
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

    // —— Dev server 进程管理 (对齐 nuwax processManager) ——
    pub deployment_mode: String,
    pub fast_restart_enabled: bool,
    pub computer_log_dir: PathBuf,
    pub template_cache_dir: PathBuf,
    pub node_modules_local_dir: PathBuf,
    pub bash_path: String,
    /// 端口池范围 [start, end] (对齐 nuwax portPool 硬编码 4000-55000)。
    pub dev_port_range_start: u16,
    pub dev_port_range_end: u16,
    /// 端口池保留区 (跳过, 对齐 nuwax 8000-9000)。
    pub dev_port_reserved_start: u16,
    pub dev_port_reserved_end: u16,
    /// 存活探活单次超时 ms (对齐 nuwax 1500)。
    pub dev_alive_check_timeout_ms: u64,
    /// 存活轮询上限 ms (对齐 nuwax 30000; 超时仍返回成功)。
    pub dev_alive_max_wait_ms: u64,
    /// stop 后等进程退出轮询间隔 ms (对齐 nuwax 100)。
    pub dev_stop_check_interval_ms: u64,
    /// stop 后最大轮询次数 (对齐 nuwax 50, 合计 5s)。
    pub dev_stop_max_attempts: u32,
    /// build/install 命令超时秒 (对齐 nuwax 10min)。
    pub dev_command_timeout_secs: u64,
    /// build 全局并发上限 (对齐 nuwax MAX_BUILD_CONCURRENCY, 默认 20)。
    pub max_build_concurrency: usize,
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
            git_default_author_email: env_str("GIT_DEFAULT_AUTHOR_EMAIL", "git@nuwax.local"),
            init_project_name_react: env_str("INIT_PROJECT_NAME_REACT", "react-vite-template"),
            init_project_name_vue3: env_str("INIT_PROJECT_NAME_VUE3", "vue3-vite-template"),
            deployment_mode: env_str("DEPLOYMENT_MODE", "docker-compose"),
            fast_restart_enabled: env_bool("FAST_RESTART_ENABLED", false),
            computer_log_dir: PathBuf::from(env_str("COMPUTER_LOG_DIR", "/app/logs/computer_logs")),
            template_cache_dir: PathBuf::from(env_str(
                "TEMPLATE_CACHE_DIR",
                "/local-cache/templates",
            )),
            node_modules_local_dir: PathBuf::from(env_str(
                "NODE_MODULES_LOCAL_DIR",
                "/local-cache/node-modules",
            )),
            bash_path: env_str("BASH_PATH", ""),
            dev_port_range_start: env_port("DEV_PORT_RANGE_START", 4000),
            dev_port_range_end: env_port("DEV_PORT_RANGE_END", 55000),
            dev_port_reserved_start: env_port("DEV_PORT_RESERVED_START", 8000),
            dev_port_reserved_end: env_port("DEV_PORT_RESERVED_END", 9000),
            dev_alive_check_timeout_ms: env_u64("DEV_ALIVE_CHECK_TIMEOUT_MS", 1500),
            dev_alive_max_wait_ms: env_u64("DEV_ALIVE_MAX_WAIT_MS", 30000),
            dev_stop_check_interval_ms: env_u64("DEV_STOP_CHECK_INTERVAL_MS", 100),
            dev_stop_max_attempts: env_u64("DEV_STOP_MAX_ATTEMPTS", 50) as u32,
            dev_command_timeout_secs: env_u64("DEV_COMMAND_TIMEOUT_SECS", 600),
            max_build_concurrency: env_u64("MAX_BUILD_CONCURRENCY", 20) as usize,
        }
    }

    /// 扩展名是否在白名单 (大小写不敏感)。
    pub fn ext_allowed(&self, list: &[String], ext: &str) -> bool {
        let lower = ext.to_lowercase();
        list.iter().any(|e| e.to_lowercase() == lower)
    }
}
