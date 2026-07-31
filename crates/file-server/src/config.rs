//! 配置 (对齐 nuwax `appConfig/index.js`): 从环境变量读取 + 默认值。
//!
//! 仅含业务路径与开关; 工作区根 (project/computer) 由 [`crate::workspace::WorkspaceResolver`] 负责。

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use shared_types::paths::{COMPUTER_WORKSPACE_ROOT, WORKSPACE_ROOT};

/// 所有客户端上传文件共享的硬上限：1 GiB。
pub const MAX_UPLOAD_FILE_SIZE_BYTES: u64 = 1024 * 1024 * 1024;

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

fn validate_upload_limit(value: u64) -> Result<()> {
    if value == 0 || value > MAX_UPLOAD_FILE_SIZE_BYTES {
        return Err(anyhow!(
            "UPLOAD_MAX_FILE_SIZE_BYTES must be between 1 and {MAX_UPLOAD_FILE_SIZE_BYTES} bytes (1 GiB)"
        ));
    }
    Ok(())
}

fn validate_request_body_limit(value: u64) -> Result<()> {
    if value == 0 || value > MAX_UPLOAD_FILE_SIZE_BYTES {
        return Err(anyhow!(
            "REQUEST_BODY_MAX_BYTES/REQUEST_BODY_LIMIT must be between 1 and {MAX_UPLOAD_FILE_SIZE_BYTES} bytes (1 GiB)"
        ));
    }
    Ok(())
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
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    // —— 服务运行参数 ——
    pub listen_host: String,
    pub port: u16,
    pub project_source_dir: PathBuf,
    pub computer_workspace_dir: PathBuf,
    pub service_log_dir: PathBuf,
    pub service_log_retention_days: usize,

    // —— 业务目录 ——
    pub init_project_dir: PathBuf,
    pub upload_project_dir: PathBuf,
    pub dist_target_dir: PathBuf,
    pub log_base_dir: PathBuf,

    // —— 文件大小上限 ——
    pub max_inline_file_size_bytes: u64,
    pub upload_max_file_size_bytes: u64,
    /// 单个 skill URL 响应上限；远小于通用项目上传上限，避免累计磁盘压力。
    pub skill_download_max_bytes: u64,
    pub skill_download_connect_timeout_secs: u64,
    pub skill_download_timeout_secs: u64,
    pub skill_download_max_redirects: usize,
    pub skill_url_max_count: usize,
    pub skill_url_allow_http: bool,
    pub skill_url_allow_private_networks: bool,
    pub skill_url_allowed_hosts: Vec<String>,
    pub download_max_file_size_bytes: u64,
    /// Axum 全局请求体上限；默认且硬上限为 1 GiB。
    pub request_body_max_bytes: u64,

    // —— 日志缓存 (对齐 nuwax logCacheManager) ——
    pub log_cache_enabled: bool,
    pub log_cache_duration_ms: u64,
    pub log_cache_max_entries: usize,
    pub log_cache_max_file_size_bytes: u64,
    /// 单次读取日志响应允许载入内存的最大字节数。
    pub log_read_max_bytes: u64,

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
    pub git_diff_max_file_size_bytes: u64,
    pub git_diff_max_total_bytes: u64,
    pub git_diff_max_output_bytes: u64,
    pub git_file_content_max_bytes: u64,

    // —— 模板 ——
    pub init_project_name_react: String,
    pub init_project_name_vue3: String,

    // —— Dev server 进程管理 (对齐 nuwax processManager) ——
    pub deployment_mode: String,
    pub fast_restart_enabled: bool,
    pub computer_log_dir: PathBuf,
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
    /// 存活轮询间隔 ms (默认 300：vite 冷启动 ~300-500ms 即 ready，原 1s 粒度会白等；
    /// 测试可设极小值如 10 以消除调度噪声、使就绪探测定时断言确定化)。
    pub dev_alive_poll_interval_ms: u64,
    /// stop 后等进程退出轮询间隔 ms (对齐 nuwax 100)。
    pub dev_stop_check_interval_ms: u64,
    /// stop 后最大轮询次数 (对齐 nuwax 50, 合计 5s)。
    pub dev_stop_max_attempts: u32,
    /// build/install 命令超时秒 (对齐 nuwax 10min)。
    pub dev_command_timeout_secs: u64,
    /// build 全局并发上限 (对齐 nuwax MAX_BUILD_CONCURRENCY, 默认 20)。
    pub max_build_concurrency: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_host: "0.0.0.0".to_string(),
            port: 60_000,
            project_source_dir: PathBuf::from(WORKSPACE_ROOT),
            computer_workspace_dir: PathBuf::from(COMPUTER_WORKSPACE_ROOT),
            service_log_dir: PathBuf::from("/app/logs/file-server"),
            service_log_retention_days: 7,
            init_project_dir: PathBuf::from("/app/project_init"),
            upload_project_dir: PathBuf::from("/app/project_zips"),
            dist_target_dir: PathBuf::from("/app/project_nginx"),
            log_base_dir: PathBuf::from("/app/logs/project_logs"),
            max_inline_file_size_bytes: 1_048_576,
            upload_max_file_size_bytes: MAX_UPLOAD_FILE_SIZE_BYTES,
            skill_download_max_bytes: 104_857_600,
            skill_download_connect_timeout_secs: 10,
            skill_download_timeout_secs: 60,
            skill_download_max_redirects: 3,
            skill_url_max_count: 20,
            skill_url_allow_http: true,
            // 私有化部署默认放行私网/保留地址: skill 通常托管在内网 (10.x/192.168.x/容器域名)。
            // 公网/SaaS 部署可用 SKILL_URL_ALLOW_PRIVATE_NETWORKS=false 重新锁紧。
            skill_url_allow_private_networks: true,
            skill_url_allowed_hosts: Vec::new(),
            download_max_file_size_bytes: 104_857_600,
            request_body_max_bytes: MAX_UPLOAD_FILE_SIZE_BYTES,
            log_cache_enabled: true,
            log_cache_duration_ms: 180_000,
            log_cache_max_entries: 100,
            log_cache_max_file_size_bytes: 2_097_152,
            log_read_max_bytes: 64 * 1024 * 1024,
            upload_allowed_extensions: vec![".zip".to_string()],
            attachment_allowed_extensions: default_attachment_extensions(),
            traverse_exclude_dirs: split_default(
                "dist,node_modules,.pnpm-store,__MACOSX,.attachments,.git,.agents,.codex,.opencode,.grok,.pi,.logs",
            ),
            backup_traverse_exclude_files: split_default(
                "pnpm-lock.yaml,yarn.lock,package-lock.json",
            ),
            content_traverse_exclude_files: split_default(
                "pnpm-lock.yaml,yarn.lock,package-lock.json",
            ),
            inline_image_extensions: split_default(
                ".png,.jpg,.jpeg,.gif,.bmp,.svg,.ico,.webp,.avif",
            ),
            zip_workspace_exclude: split_default(
                ".git,.tmp,.claude,.agents,.codex,.opencode,.grok,.pi,.logs,.npmrc,__pycache__,node_modules,dist,pnpm-lock.yaml,yarn.lock,package-lock.json",
            ),
            git_enabled: false,
            git_default_author_name: "Nuwax File Server".to_string(),
            git_default_author_email: "git@nuwax.com".to_string(),
            git_diff_max_file_size_bytes: 16 * 1024 * 1024,
            git_diff_max_total_bytes: 64 * 1024 * 1024,
            git_diff_max_output_bytes: 64 * 1024 * 1024,
            git_file_content_max_bytes: 64 * 1024 * 1024,
            init_project_name_react: "react-vite-template".to_string(),
            init_project_name_vue3: "vue3-vite-template".to_string(),
            deployment_mode: "docker-compose".to_string(),
            fast_restart_enabled: false,
            computer_log_dir: PathBuf::from("/app/logs/computer_logs"),
            bash_path: String::new(),
            dev_port_range_start: 4000,
            dev_port_range_end: 55_000,
            dev_port_reserved_start: 8000,
            dev_port_reserved_end: 9000,
            dev_alive_check_timeout_ms: 1500,
            dev_alive_max_wait_ms: 30_000,
            dev_alive_poll_interval_ms: 300,
            dev_stop_check_interval_ms: 100,
            dev_stop_max_attempts: 50,
            dev_command_timeout_secs: 600,
            max_build_concurrency: 20,
        }
    }
}

fn split_default(value: &str) -> Vec<String> {
    value.split(',').map(str::to_owned).collect()
}

impl Config {
    /// 二进制默认加载入口：设置 `FILE_SERVER_CONFIG` 时读取 YAML/TOML/JSON，
    /// 未设置时读取环境变量；两者都以 [`Config::default`] 补齐缺省值。
    pub fn load() -> Result<Self> {
        match std::env::var("FILE_SERVER_CONFIG") {
            Ok(path) => Self::from_file(path)?.with_env_overrides(),
            Err(std::env::VarError::NotPresent) => Self::from_env(),
            Err(error) => Err(anyhow!(error)).context("read FILE_SERVER_CONFIG"),
        }
    }

    /// 从 YAML、YML、TOML 或 JSON 配置文件加载；配置文件允许只写需要覆盖的字段。
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read file-server config {}", path.display()))?;
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let config: Self = match extension.as_str() {
            "yaml" | "yml" => serde_yaml::from_str(&content)
                .with_context(|| format!("parse YAML config {}", path.display()))?,
            "toml" => toml::from_str(&content)
                .with_context(|| format!("parse TOML config {}", path.display()))?,
            "json" => serde_json::from_str(&content)
                .with_context(|| format!("parse JSON config {}", path.display()))?,
            _ => {
                return Err(anyhow!(
                    "unsupported config extension for {}; expected yaml, yml, toml, or json",
                    path.display()
                ));
            }
        };
        config.validate()?;
        Ok(config)
    }

    /// 在已有默认值或文件配置上叠加 shell/K8s 环境变量。
    pub fn with_env_overrides(mut self) -> Result<Self> {
        macro_rules! parse {
            ($field:ident, $key:literal) => {
                self.$field = env_parse($key, self.$field)?;
            };
        }
        macro_rules! string {
            ($field:ident, $key:literal) => {
                self.$field = env_str($key, &self.$field)?;
            };
        }
        macro_rules! path {
            ($field:ident, $key:literal) => {
                self.$field = PathBuf::from(env_str($key, &self.$field.to_string_lossy())?);
            };
        }
        macro_rules! boolean {
            ($field:ident, $key:literal) => {
                self.$field = env_bool($key, self.$field)?;
            };
        }
        macro_rules! list {
            ($field:ident, $key:literal) => {
                self.$field = env_list($key, &self.$field.join(","))?;
            };
        }

        string!(listen_host, "FILE_SERVER_HOST");
        self.port = match std::env::var("FILE_SERVER_PORT").or_else(|_| std::env::var("PORT")) {
            Ok(value) => value
                .parse()
                .map_err(|error| anyhow!("invalid FILE_SERVER_PORT/PORT: {error}"))?,
            Err(std::env::VarError::NotPresent) => self.port,
            Err(error) => return Err(anyhow!(error)).context("read FILE_SERVER_PORT/PORT"),
        };
        path!(project_source_dir, "PROJECT_SOURCE_DIR");
        path!(computer_workspace_dir, "COMPUTER_WORKSPACE_DIR");
        path!(service_log_dir, "FILE_SERVER_LOG_DIR");
        parse!(service_log_retention_days, "FILE_SERVER_LOG_RETENTION_DAYS");
        path!(init_project_dir, "INIT_PROJECT_DIR");
        path!(upload_project_dir, "UPLOAD_PROJECT_DIR");
        path!(dist_target_dir, "DIST_TARGET_DIR");
        path!(log_base_dir, "LOG_BASE_DIR");
        parse!(max_inline_file_size_bytes, "MAX_INLINE_FILE_SIZE_BYTES");
        parse!(upload_max_file_size_bytes, "UPLOAD_MAX_FILE_SIZE_BYTES");
        parse!(skill_download_max_bytes, "SKILL_DOWNLOAD_MAX_BYTES");
        parse!(
            skill_download_connect_timeout_secs,
            "SKILL_DOWNLOAD_CONNECT_TIMEOUT_SECS"
        );
        parse!(skill_download_timeout_secs, "SKILL_DOWNLOAD_TIMEOUT_SECS");
        parse!(skill_download_max_redirects, "SKILL_DOWNLOAD_MAX_REDIRECTS");
        parse!(skill_url_max_count, "SKILL_URL_MAX_COUNT");
        boolean!(skill_url_allow_http, "SKILL_URL_ALLOW_HTTP");
        boolean!(
            skill_url_allow_private_networks,
            "SKILL_URL_ALLOW_PRIVATE_NETWORKS"
        );
        list!(skill_url_allowed_hosts, "SKILL_URL_ALLOWED_HOSTS");
        parse!(download_max_file_size_bytes, "DOWNLOAD_MAX_FILE_SIZE_BYTES");
        self.request_body_max_bytes = request_body_max_bytes(self.request_body_max_bytes)?;
        boolean!(log_cache_enabled, "LOG_CACHE_ENABLED");
        parse!(log_cache_duration_ms, "LOG_CACHE_DURATION");
        parse!(log_cache_max_entries, "LOG_CACHE_MAX_ENTRIES");
        parse!(log_cache_max_file_size_bytes, "LOG_CACHE_MAX_FILE_SIZE");
        parse!(log_read_max_bytes, "LOG_READ_MAX_BYTES");
        list!(upload_allowed_extensions, "UPLOAD_ALLOWED_EXTENSIONS");
        list!(traverse_exclude_dirs, "TRAVERSE_EXCLUDE_DIRS");
        list!(
            backup_traverse_exclude_files,
            "BACKUP_TRAVERSE_EXCLUDE_FILES"
        );
        list!(
            content_traverse_exclude_files,
            "CONTENT_TRAVERSE_EXCLUDE_FILES"
        );
        list!(inline_image_extensions, "INLINE_IMAGE_EXTENSIONS");
        list!(zip_workspace_exclude, "ZIP_WORKSPACE_EXCLUDE");
        boolean!(git_enabled, "GIT_ENABLED");
        string!(git_default_author_name, "GIT_DEFAULT_AUTHOR_NAME");
        string!(git_default_author_email, "GIT_DEFAULT_AUTHOR_EMAIL");
        parse!(git_diff_max_file_size_bytes, "GIT_DIFF_MAX_FILE_SIZE_BYTES");
        parse!(git_diff_max_total_bytes, "GIT_DIFF_MAX_TOTAL_BYTES");
        parse!(git_diff_max_output_bytes, "GIT_DIFF_MAX_OUTPUT_BYTES");
        parse!(git_file_content_max_bytes, "GIT_FILE_CONTENT_MAX_BYTES");
        string!(init_project_name_react, "INIT_PROJECT_NAME_REACT");
        string!(init_project_name_vue3, "INIT_PROJECT_NAME_VUE3");
        string!(deployment_mode, "DEPLOYMENT_MODE");
        boolean!(fast_restart_enabled, "FAST_RESTART_ENABLED");
        path!(computer_log_dir, "COMPUTER_LOG_DIR");
        string!(bash_path, "BASH_PATH");
        parse!(dev_port_range_start, "DEV_PORT_RANGE_START");
        parse!(dev_port_range_end, "DEV_PORT_RANGE_END");
        parse!(dev_port_reserved_start, "DEV_PORT_RESERVED_START");
        parse!(dev_port_reserved_end, "DEV_PORT_RESERVED_END");
        parse!(dev_alive_check_timeout_ms, "DEV_ALIVE_CHECK_TIMEOUT_MS");
        parse!(dev_alive_max_wait_ms, "DEV_ALIVE_MAX_WAIT_MS");
        parse!(dev_alive_poll_interval_ms, "DEV_ALIVE_POLL_INTERVAL_MS");
        parse!(dev_stop_check_interval_ms, "DEV_STOP_CHECK_INTERVAL_MS");
        parse!(dev_stop_max_attempts, "DEV_STOP_MAX_ATTEMPTS");
        parse!(dev_command_timeout_secs, "DEV_COMMAND_TIMEOUT_SECS");
        parse!(max_build_concurrency, "MAX_BUILD_CONCURRENCY");
        self.validate()?;
        Ok(self)
    }

    /// 从环境变量构造 (env 优先, 缺省用硬编码默认值)。
    pub fn from_env() -> Result<Self> {
        let config = Self {
            listen_host: env_str("FILE_SERVER_HOST", "0.0.0.0")?,
            port: match std::env::var("FILE_SERVER_PORT").or_else(|_| std::env::var("PORT")) {
                Ok(value) => value
                    .parse()
                    .map_err(|error| anyhow!("invalid FILE_SERVER_PORT/PORT: {error}"))?,
                Err(std::env::VarError::NotPresent) => 60_000,
                Err(error) => return Err(anyhow!(error)).context("read FILE_SERVER_PORT/PORT"),
            },
            project_source_dir: PathBuf::from(env_str("PROJECT_SOURCE_DIR", WORKSPACE_ROOT)?),
            computer_workspace_dir: PathBuf::from(env_str(
                "COMPUTER_WORKSPACE_DIR",
                COMPUTER_WORKSPACE_ROOT,
            )?),
            service_log_dir: PathBuf::from(env_str(
                "FILE_SERVER_LOG_DIR",
                "/app/logs/file-server",
            )?),
            service_log_retention_days: env_parse("FILE_SERVER_LOG_RETENTION_DAYS", 7)?,
            init_project_dir: PathBuf::from(env_str("INIT_PROJECT_DIR", "/app/project_init")?),
            upload_project_dir: PathBuf::from(env_str("UPLOAD_PROJECT_DIR", "/app/project_zips")?),
            dist_target_dir: PathBuf::from(env_str("DIST_TARGET_DIR", "/app/project_nginx")?),
            log_base_dir: PathBuf::from(env_str("LOG_BASE_DIR", "/app/logs/project_logs")?),
            max_inline_file_size_bytes: env_parse("MAX_INLINE_FILE_SIZE_BYTES", 1_048_576)?,
            upload_max_file_size_bytes: env_parse(
                "UPLOAD_MAX_FILE_SIZE_BYTES",
                MAX_UPLOAD_FILE_SIZE_BYTES,
            )?,
            skill_download_max_bytes: env_parse("SKILL_DOWNLOAD_MAX_BYTES", 104_857_600)?,
            skill_download_connect_timeout_secs: env_parse(
                "SKILL_DOWNLOAD_CONNECT_TIMEOUT_SECS",
                10,
            )?,
            skill_download_timeout_secs: env_parse("SKILL_DOWNLOAD_TIMEOUT_SECS", 60)?,
            skill_download_max_redirects: env_parse("SKILL_DOWNLOAD_MAX_REDIRECTS", 3)?,
            skill_url_max_count: env_parse("SKILL_URL_MAX_COUNT", 20)?,
            skill_url_allow_http: env_bool("SKILL_URL_ALLOW_HTTP", true)?,
            skill_url_allow_private_networks: env_bool("SKILL_URL_ALLOW_PRIVATE_NETWORKS", true)?,
            skill_url_allowed_hosts: env_list("SKILL_URL_ALLOWED_HOSTS", "")?,
            download_max_file_size_bytes: env_parse("DOWNLOAD_MAX_FILE_SIZE_BYTES", 104_857_600)?,
            request_body_max_bytes: request_body_max_bytes(MAX_UPLOAD_FILE_SIZE_BYTES)?,
            log_cache_enabled: env_bool("LOG_CACHE_ENABLED", true)?,
            log_cache_duration_ms: env_parse("LOG_CACHE_DURATION", 180_000)?,
            log_cache_max_entries: env_parse("LOG_CACHE_MAX_ENTRIES", 100)?,
            log_cache_max_file_size_bytes: env_parse("LOG_CACHE_MAX_FILE_SIZE", 2_097_152)?,
            log_read_max_bytes: env_parse("LOG_READ_MAX_BYTES", 64 * 1024 * 1024)?,
            upload_allowed_extensions: env_list("UPLOAD_ALLOWED_EXTENSIONS", ".zip")?,
            attachment_allowed_extensions: default_attachment_extensions(),
            traverse_exclude_dirs: env_list(
                "TRAVERSE_EXCLUDE_DIRS",
                "dist,node_modules,.pnpm-store,__MACOSX,.attachments,.git,.agents,.codex,.opencode,.grok,.pi,.logs",
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
                ".git,.tmp,.claude,.agents,.codex,.opencode,.grok,.pi,.logs,.npmrc,__pycache__,node_modules,dist,pnpm-lock.yaml,yarn.lock,package-lock.json",
            )?,
            git_enabled: env_bool("GIT_ENABLED", false)?,
            git_default_author_name: env_str("GIT_DEFAULT_AUTHOR_NAME", "Nuwax File Server")?,
            git_default_author_email: env_str("GIT_DEFAULT_AUTHOR_EMAIL", "git@nuwax.com")?,
            git_diff_max_file_size_bytes: env_parse(
                "GIT_DIFF_MAX_FILE_SIZE_BYTES",
                16 * 1024 * 1024,
            )?,
            git_diff_max_total_bytes: env_parse("GIT_DIFF_MAX_TOTAL_BYTES", 64 * 1024 * 1024)?,
            git_diff_max_output_bytes: env_parse("GIT_DIFF_MAX_OUTPUT_BYTES", 64 * 1024 * 1024)?,
            git_file_content_max_bytes: env_parse("GIT_FILE_CONTENT_MAX_BYTES", 64 * 1024 * 1024)?,
            init_project_name_react: env_str("INIT_PROJECT_NAME_REACT", "react-vite-template")?,
            init_project_name_vue3: env_str("INIT_PROJECT_NAME_VUE3", "vue3-vite-template")?,
            deployment_mode: env_str("DEPLOYMENT_MODE", "docker-compose")?,
            fast_restart_enabled: env_bool("FAST_RESTART_ENABLED", false)?,
            computer_log_dir: PathBuf::from(env_str(
                "COMPUTER_LOG_DIR",
                "/app/logs/computer_logs",
            )?),
            bash_path: env_str("BASH_PATH", "")?,
            dev_port_range_start: env_parse("DEV_PORT_RANGE_START", 4000)?,
            dev_port_range_end: env_parse("DEV_PORT_RANGE_END", 55000)?,
            dev_port_reserved_start: env_parse("DEV_PORT_RESERVED_START", 8000)?,
            dev_port_reserved_end: env_parse("DEV_PORT_RESERVED_END", 9000)?,
            dev_alive_check_timeout_ms: env_parse("DEV_ALIVE_CHECK_TIMEOUT_MS", 1500)?,
            dev_alive_max_wait_ms: env_parse("DEV_ALIVE_MAX_WAIT_MS", 30000)?,
            dev_alive_poll_interval_ms: env_parse("DEV_ALIVE_POLL_INTERVAL_MS", 300)?,
            dev_stop_check_interval_ms: env_parse("DEV_STOP_CHECK_INTERVAL_MS", 100)?,
            dev_stop_max_attempts: env_parse("DEV_STOP_MAX_ATTEMPTS", 50)?,
            dev_command_timeout_secs: env_parse("DEV_COMMAND_TIMEOUT_SECS", 600)?,
            max_build_concurrency: env_parse("MAX_BUILD_CONCURRENCY", 20)?,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, path) in [
            ("INIT_PROJECT_DIR", &self.init_project_dir),
            ("UPLOAD_PROJECT_DIR", &self.upload_project_dir),
            ("DIST_TARGET_DIR", &self.dist_target_dir),
            ("LOG_BASE_DIR", &self.log_base_dir),
            ("COMPUTER_LOG_DIR", &self.computer_log_dir),
            ("PROJECT_SOURCE_DIR", &self.project_source_dir),
            ("COMPUTER_WORKSPACE_DIR", &self.computer_workspace_dir),
            ("FILE_SERVER_LOG_DIR", &self.service_log_dir),
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
        validate_upload_limit(self.upload_max_file_size_bytes)?;
        validate_request_body_limit(self.request_body_max_bytes)?;
        if self.port == 0 {
            return Err(anyhow!("port must be greater than zero"));
        }
        if self.listen_host.trim().is_empty() {
            return Err(anyhow!("listen_host must not be empty"));
        }
        if self.service_log_retention_days == 0 {
            return Err(anyhow!(
                "FILE_SERVER_LOG_RETENTION_DAYS must be greater than zero"
            ));
        }
        for (name, value) in [
            (
                "GIT_DIFF_MAX_FILE_SIZE_BYTES",
                self.git_diff_max_file_size_bytes,
            ),
            ("GIT_DIFF_MAX_TOTAL_BYTES", self.git_diff_max_total_bytes),
            ("GIT_DIFF_MAX_OUTPUT_BYTES", self.git_diff_max_output_bytes),
            (
                "GIT_FILE_CONTENT_MAX_BYTES",
                self.git_file_content_max_bytes,
            ),
        ] {
            if value == 0 {
                return Err(anyhow!("{name} must be greater than zero"));
            }
        }
        for (name, value) in [
            ("SKILL_DOWNLOAD_MAX_BYTES", self.skill_download_max_bytes),
            (
                "SKILL_DOWNLOAD_CONNECT_TIMEOUT_SECS",
                self.skill_download_connect_timeout_secs,
            ),
            (
                "SKILL_DOWNLOAD_TIMEOUT_SECS",
                self.skill_download_timeout_secs,
            ),
        ] {
            if value == 0 {
                return Err(anyhow!("{name} must be greater than zero"));
            }
        }
        if self.skill_url_max_count == 0 {
            return Err(anyhow!("SKILL_URL_MAX_COUNT must be greater than zero"));
        }
        for (name, value) in [
            (
                "DEV_ALIVE_CHECK_TIMEOUT_MS",
                self.dev_alive_check_timeout_ms,
            ),
            ("DEV_ALIVE_MAX_WAIT_MS", self.dev_alive_max_wait_ms),
            (
                "DEV_ALIVE_POLL_INTERVAL_MS",
                self.dev_alive_poll_interval_ms,
            ),
            (
                "DEV_STOP_CHECK_INTERVAL_MS",
                self.dev_stop_check_interval_ms,
            ),
            ("DEV_COMMAND_TIMEOUT_SECS", self.dev_command_timeout_secs),
            ("LOG_READ_MAX_BYTES", self.log_read_max_bytes),
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
    use super::{
        Config, MAX_UPLOAD_FILE_SIZE_BYTES, parse_byte_size, validate_request_body_limit,
        validate_upload_limit,
    };

    #[test]
    fn parses_legacy_request_body_limit_units() {
        assert_eq!(parse_byte_size("2000mb"), Some(2_097_152_000));
        assert_eq!(parse_byte_size("2 GB"), Some(2_147_483_648));
        assert_eq!(parse_byte_size("1024"), Some(1024));
        assert_eq!(parse_byte_size("invalid"), None);
    }

    #[test]
    fn upload_limit_is_at_most_one_gibibyte() {
        assert!(validate_upload_limit(MAX_UPLOAD_FILE_SIZE_BYTES).is_ok());
        assert!(validate_upload_limit(MAX_UPLOAD_FILE_SIZE_BYTES + 1).is_err());
        assert!(validate_upload_limit(0).is_err());
    }

    #[test]
    fn request_body_limit_is_at_most_one_gibibyte() {
        assert!(validate_request_body_limit(MAX_UPLOAD_FILE_SIZE_BYTES).is_ok());
        assert!(validate_request_body_limit(MAX_UPLOAD_FILE_SIZE_BYTES + 1).is_err());
        assert!(validate_request_body_limit(0).is_err());
    }

    #[test]
    fn partial_yaml_config_uses_defaults() {
        let file = tempfile::Builder::new()
            .suffix(".yaml")
            .tempfile()
            .expect("create config fixture");
        std::fs::write(file.path(), "port: 60123\ngit_enabled: true\n")
            .expect("write config fixture");
        let config = Config::from_file(file.path()).expect("load partial YAML config");
        assert_eq!(config.port, 60_123);
        assert!(config.git_enabled);
        assert_eq!(config.request_body_max_bytes, MAX_UPLOAD_FILE_SIZE_BYTES);
        assert_eq!(config.service_log_retention_days, 7);
    }

    #[test]
    fn config_file_rejects_unknown_fields() {
        let file = tempfile::Builder::new()
            .suffix(".toml")
            .tempfile()
            .expect("create config fixture");
        std::fs::write(file.path(), "unknown_setting = true\n").expect("write config fixture");
        assert!(Config::from_file(file.path()).is_err());
    }
}
