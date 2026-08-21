//! 配置加载: 配置文件 (YAML/TOML/JSON) + 环境变量叠加 + 校验。

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use shared_types::AGENT_FILE_SERVER_PORT;
use shared_types::paths::{COMPUTER_WORKSPACE_ROOT, USERAPP_WORKSPACE_ROOT, WORKSPACE_ROOT};

use super::Config;
use super::DeploymentMode;
use super::env::{
    MAX_UPLOAD_FILE_SIZE_BYTES, default_attachment_extensions, env_bool, env_list, env_parse,
    env_str, request_body_max_bytes, validate_request_body_limit, validate_upload_limit,
};

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
        path!(userapp_workspace_dir, "USERAPP_WORKSPACE_DIR");
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
        parse!(deployment_mode, "DEPLOYMENT_MODE");
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
                Err(std::env::VarError::NotPresent) => AGENT_FILE_SERVER_PORT,
                Err(error) => return Err(anyhow!(error)).context("read FILE_SERVER_PORT/PORT"),
            },
            project_source_dir: PathBuf::from(env_str("PROJECT_SOURCE_DIR", WORKSPACE_ROOT)?),
            computer_workspace_dir: PathBuf::from(env_str(
                "COMPUTER_WORKSPACE_DIR",
                COMPUTER_WORKSPACE_ROOT,
            )?),
            userapp_workspace_dir: PathBuf::from(env_str(
                "USERAPP_WORKSPACE_DIR",
                USERAPP_WORKSPACE_ROOT,
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
                super::DEFAULT_TRAVERSE_EXCLUDE_DIRS,
            )?,
            backup_traverse_exclude_files: env_list(
                "BACKUP_TRAVERSE_EXCLUDE_FILES",
                super::DEFAULT_BACKUP_TRAVERSE_EXCLUDE_FILES,
            )?,
            content_traverse_exclude_files: env_list(
                "CONTENT_TRAVERSE_EXCLUDE_FILES",
                super::DEFAULT_CONTENT_TRAVERSE_EXCLUDE_FILES,
            )?,
            inline_image_extensions: env_list(
                "INLINE_IMAGE_EXTENSIONS",
                super::DEFAULT_INLINE_IMAGE_EXTENSIONS,
            )?,
            zip_workspace_exclude: env_list(
                "ZIP_WORKSPACE_EXCLUDE",
                super::DEFAULT_ZIP_WORKSPACE_EXCLUDE,
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
            deployment_mode: env_parse("DEPLOYMENT_MODE", DeploymentMode::default())?,
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
            ("USERAPP_WORKSPACE_DIR", &self.userapp_workspace_dir),
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
}

#[cfg(test)]
mod tests {
    use super::super::env::MAX_UPLOAD_FILE_SIZE_BYTES;
    use super::Config;

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
