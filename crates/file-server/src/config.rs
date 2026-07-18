//! 配置 (对齐 nuwax `appConfig/index.js`): 从环境变量读取 + 默认值。
//!
//! 仅含业务路径与开关; 工作区根 (project/computer) 由 [`crate::workspace::WorkspaceResolver`] 负责。

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};

fn env_str(key: &str, default: &str) -> Result<String> {
    match std::env::var(key) {
        Ok(value) => Ok(value),
        Err(std::env::VarError::NotPresent) => Ok(default.to_string()),
        Err(error) => Err(anyhow!(error)).context(format!("read environment variable {key}")),
    }
}

fn env_bool(key: &str, default: bool) -> Result<bool> {
    match std::env::var(key) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            _ => Err(anyhow!(
                "environment variable {key} must be true/false, 1/0, or yes/no"
            )),
        },
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow!(error)).context(format!("read environment variable {key}")),
    }
}

fn env_parse<T>(key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(value) => value
            .trim()
            .parse()
            .map_err(|error| anyhow!("invalid environment variable {key}: {error}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow!(error)).context(format!("read environment variable {key}")),
    }
}

fn parse_byte_size(value: &str) -> Option<u64> {
    let normalized = value.trim().to_ascii_lowercase();
    let (number, multiplier) = [
        ("gb", 1024_u64.pow(3)),
        ("mb", 1024_u64.pow(2)),
        ("kb", 1024_u64),
        ("b", 1_u64),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        normalized
            .strip_suffix(suffix)
            .map(|number| (number.trim(), multiplier))
    })
    .unwrap_or((normalized.as_str(), 1));
    number
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
}

fn request_body_max_bytes(default: u64) -> Result<u64> {
    for key in ["REQUEST_BODY_MAX_BYTES", "REQUEST_BODY_LIMIT"] {
        match std::env::var(key) {
            Ok(value) => {
                return parse_byte_size(&value)
                    .ok_or_else(|| anyhow!("invalid byte-size environment variable {key}"));
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(error) => {
                return Err(anyhow!(error)).context(format!("read environment variable {key}"));
            }
        }
    }
    Ok(default)
}

fn env_list(key: &str, default: &str) -> Result<Vec<String>> {
    Ok(env_str(key, default)?
        .split(',')
        .map(str::trim)
        .map(ToOwned::to_owned)
        .filter(|s| !s.is_empty())
        .collect())
}

/// 附件上传白名单 (对齐 nuwax `projectRoutes.js` ATTACHMENT_ALLOWED_EXTENSIONS, 硬编码)。
fn default_attachment_extensions() -> Vec<String> {
    [
        ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".txt", ".md", ".png", ".jpg",
        ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico", ".avif", ".zip", ".rar", ".7z", ".tar",
        ".gz", ".csv", ".json", ".xml", ".mp4", ".mov", ".avi", ".wmv", ".flv", ".mp3", ".wav",
        ".ogg", ".m4a",
    ]
    .into_iter()
    .map(str::to_owned)
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
    /// Axum 请求体上限；默认对齐 nuwax express.json 2000mb。
    pub request_body_max_bytes: u64,

    // —— 日志缓存 (对齐 nuwax logCacheManager) ——
    pub log_cache_enabled: bool,
    pub log_cache_duration_ms: u64,
    pub log_cache_max_entries: usize,
    pub log_cache_max_file_size_bytes: u64,

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
    pub fn from_env() -> Result<Self> {
        let config = Self {
            init_project_dir: PathBuf::from(env_str("INIT_PROJECT_DIR", "/app/project_init")?),
            upload_project_dir: PathBuf::from(env_str("UPLOAD_PROJECT_DIR", "/app/project_zips")?),
            dist_target_dir: PathBuf::from(env_str("DIST_TARGET_DIR", "/app/project_nginx")?),
            log_base_dir: PathBuf::from(env_str("LOG_BASE_DIR", "/app/logs/project_logs")?),
            max_inline_file_size_bytes: env_parse("MAX_INLINE_FILE_SIZE_BYTES", 1_048_576)?,
            upload_max_file_size_bytes: env_parse("UPLOAD_MAX_FILE_SIZE_BYTES", 1_048_576_000)?,
            upload_single_file_size_bytes: env_parse(
                "UPLOAD_SINGLE_FILE_SIZE_BYTES",
                1_048_576_000,
            )?,
            download_max_file_size_bytes: env_parse("DOWNLOAD_MAX_FILE_SIZE_BYTES", 104_857_600)?,
            request_body_max_bytes: request_body_max_bytes(2_097_152_000)?,
            log_cache_enabled: env_bool("LOG_CACHE_ENABLED", true)?,
            log_cache_duration_ms: env_parse("LOG_CACHE_DURATION", 180_000)?,
            log_cache_max_entries: env_parse("LOG_CACHE_MAX_ENTRIES", 100)?,
            log_cache_max_file_size_bytes: env_parse("LOG_CACHE_MAX_FILE_SIZE", 2_097_152)?,
            upload_allowed_extensions: env_list("UPLOAD_ALLOWED_EXTENSIONS", ".zip")?,
            attachment_allowed_extensions: default_attachment_extensions(),
            traverse_exclude_dirs: env_list(
                "TRAVERSE_EXCLUDE_DIRS",
                "dist,node_modules,.pnpm-store,__MACOSX,.attachments,.git,.agents,.codex,.opencode,.logs",
            )?,
            backup_traverse_exclude_files: env_list(
                "BACKUP_TRAVERSE_EXCLUDE_FILES",
                "pnpm-lock.yaml,yarn.lock,package-lock.json",
            )?,
            content_traverse_exclude_files: env_list(
                "CONTENT_TRAVERSE_EXCLUDE_FILES",
                "pnpm-lock.yaml,yarn.lock,package-lock.json",
            )?,
            inline_image_extensions: env_list(
                "INLINE_IMAGE_EXTENSIONS",
                ".png,.jpg,.jpeg,.gif,.bmp,.svg,.ico,.webp,.avif",
            )?,
            zip_workspace_exclude: env_list(
                "ZIP_WORKSPACE_EXCLUDE",
                ".git,.tmp,.claude,.agents,.codex,.opencode,.logs,.npmrc,__pycache__,node_modules,dist,pnpm-lock.yaml,yarn.lock,package-lock.json",
            )?,
            git_enabled: env_bool("GIT_ENABLED", false)?,
            git_default_author_name: env_str("GIT_DEFAULT_AUTHOR_NAME", "Nuwax File Server")?,
            git_default_author_email: env_str("GIT_DEFAULT_AUTHOR_EMAIL", "git@nuwax.com")?,
            init_project_name_react: env_str("INIT_PROJECT_NAME_REACT", "react-vite-template")?,
            init_project_name_vue3: env_str("INIT_PROJECT_NAME_VUE3", "vue3-vite-template")?,
            deployment_mode: env_str("DEPLOYMENT_MODE", "docker-compose")?,
            fast_restart_enabled: env_bool("FAST_RESTART_ENABLED", false)?,
            computer_log_dir: PathBuf::from(env_str(
                "COMPUTER_LOG_DIR",
                "/app/logs/computer_logs",
            )?),
            template_cache_dir: PathBuf::from(env_str(
                "TEMPLATE_CACHE_DIR",
                "/local-cache/templates",
            )?),
            node_modules_local_dir: PathBuf::from(env_str(
                "NODE_MODULES_LOCAL_DIR",
                "/local-cache/node-modules",
            )?),
            bash_path: env_str("BASH_PATH", "")?,
            dev_port_range_start: env_parse("DEV_PORT_RANGE_START", 4000)?,
            dev_port_range_end: env_parse("DEV_PORT_RANGE_END", 55000)?,
            dev_port_reserved_start: env_parse("DEV_PORT_RESERVED_START", 8000)?,
            dev_port_reserved_end: env_parse("DEV_PORT_RESERVED_END", 9000)?,
            dev_alive_check_timeout_ms: env_parse("DEV_ALIVE_CHECK_TIMEOUT_MS", 1500)?,
            dev_alive_max_wait_ms: env_parse("DEV_ALIVE_MAX_WAIT_MS", 30000)?,
            dev_stop_check_interval_ms: env_parse("DEV_STOP_CHECK_INTERVAL_MS", 100)?,
            dev_stop_max_attempts: env_parse("DEV_STOP_MAX_ATTEMPTS", 50)?,
            dev_command_timeout_secs: env_parse("DEV_COMMAND_TIMEOUT_SECS", 600)?,
            max_build_concurrency: env_parse("MAX_BUILD_CONCURRENCY", 20)?,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        for (name, path) in [
            ("INIT_PROJECT_DIR", &self.init_project_dir),
            ("UPLOAD_PROJECT_DIR", &self.upload_project_dir),
            ("DIST_TARGET_DIR", &self.dist_target_dir),
            ("LOG_BASE_DIR", &self.log_base_dir),
            ("COMPUTER_LOG_DIR", &self.computer_log_dir),
            ("TEMPLATE_CACHE_DIR", &self.template_cache_dir),
            ("NODE_MODULES_LOCAL_DIR", &self.node_modules_local_dir),
        ] {
            if path.as_os_str().is_empty() {
                return Err(anyhow!("{name} must not be empty"));
            }
        }
        if self.dev_port_range_start == 0 {
            return Err(anyhow!("DEV_PORT_RANGE_START must be greater than zero"));
        }
        if self.dev_port_range_start > self.dev_port_range_end {
            return Err(anyhow!(
                "DEV_PORT_RANGE_START must not exceed DEV_PORT_RANGE_END"
            ));
        }
        if self.dev_port_reserved_start > self.dev_port_reserved_end {
            return Err(anyhow!(
                "DEV_PORT_RESERVED_START must not exceed DEV_PORT_RESERVED_END"
            ));
        }
        if self.max_build_concurrency == 0 {
            return Err(anyhow!("MAX_BUILD_CONCURRENCY must be greater than zero"));
        }
        for (name, value) in [
            (
                "DEV_ALIVE_CHECK_TIMEOUT_MS",
                self.dev_alive_check_timeout_ms,
            ),
            ("DEV_ALIVE_MAX_WAIT_MS", self.dev_alive_max_wait_ms),
            (
                "DEV_STOP_CHECK_INTERVAL_MS",
                self.dev_stop_check_interval_ms,
            ),
            ("DEV_COMMAND_TIMEOUT_SECS", self.dev_command_timeout_secs),
        ] {
            if value == 0 {
                return Err(anyhow!("{name} must be greater than zero"));
            }
        }
        if self.dev_stop_max_attempts == 0 {
            return Err(anyhow!("DEV_STOP_MAX_ATTEMPTS must be greater than zero"));
        }
        Ok(())
    }

    /// 扩展名是否在白名单 (大小写不敏感)。
    pub fn ext_allowed(&self, list: &[String], ext: &str) -> bool {
        list.iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_byte_size;

    #[test]
    fn parses_legacy_request_body_limit_units() {
        assert_eq!(parse_byte_size("2000mb"), Some(2_097_152_000));
        assert_eq!(parse_byte_size("2 GB"), Some(2_147_483_648));
        assert_eq!(parse_byte_size("1024"), Some(1024));
        assert_eq!(parse_byte_size("invalid"), None);
    }
}
