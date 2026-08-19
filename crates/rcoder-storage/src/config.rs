//! PostgreSQL 连接配置（rcoder config.yml `[storage.postgres]` 段的数据模型）
//!
//! 定义在 rcoder-storage（连接的所有者），rcoder 的 config.rs 经 serde 内嵌本类型
//! 并负责 env 覆盖（`RCODER_PG_*`，优先级 env > config.yml > 默认值）。

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};

/// PG 连接默认端口
pub const DEFAULT_PG_PORT: u16 = 5432;
/// 连接池默认大小（写穿透模式 QPS 低，10 足够）
pub const DEFAULT_PG_MAX_CONNECTIONS: u32 = 10;
/// 连接池默认预热连接数（启动即建，避免冷启动首批查询付建连延迟）
pub const DEFAULT_PG_MIN_CONNECTIONS: u32 = 2;
/// 默认连接超时（秒）
pub const DEFAULT_PG_CONNECT_TIMEOUT_SECS: u64 = 10;
/// 默认语句超时（秒，防拖死请求）
pub const DEFAULT_PG_STATEMENT_TIMEOUT_SECS: u64 = 5;
/// 默认连接最大寿命（秒）。短于 sqlx 默认 30min：CNPG failover 后指向旧 primary 的
/// 长寿命连接会变僵尸，到期（release/recycle 时）关闭重建即自愈——10min 是
/// "failover 恢复上界"与"重建频率"的折中（建连 ms 级，重建开销可忽略）。
pub const DEFAULT_PG_MAX_LIFETIME_SECS: u64 = 600;

/// `[storage.postgres]` 配置段
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PostgresConfig {
    /// 主机名（K8s 内通常是 `<release>-pg.<namespace>.svc.cluster.local`）
    pub host: Option<String>,
    /// 端口（默认 5432）
    pub port: Option<u16>,
    /// 用户名
    pub username: Option<String>,
    /// 密码（k8s 部署建议留空，经 `RCODER_PG_PASSWORD` env + Secret 注入）
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    /// 数据库名（默认 rcoder）
    pub database: Option<String>,
    /// 整串 DSN（可选；设置后优先于离散字段，覆盖特殊字符/SSL 参数等场景）
    pub url: Option<String>,
    /// 连接池大小（默认 10）
    pub max_connections: Option<u32>,
    /// 预热连接数（默认 2；池维持的最少空闲连接，冷启动首批查询免建连）
    pub min_connections: Option<u32>,
    /// 连接超时秒数（默认 10）
    pub connect_timeout_secs: Option<u64>,
    /// 语句超时秒数（默认 5）
    pub statement_timeout_secs: Option<u64>,
    /// 连接最大寿命秒数（默认 600；CNPG failover 僵尸连接自愈上界）
    pub max_lifetime_secs: Option<u64>,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            host: None,
            port: None,
            username: None,
            password: None,
            database: None,
            url: None,
            max_connections: Some(DEFAULT_PG_MAX_CONNECTIONS),
            min_connections: Some(DEFAULT_PG_MIN_CONNECTIONS),
            connect_timeout_secs: Some(DEFAULT_PG_CONNECT_TIMEOUT_SECS),
            statement_timeout_secs: Some(DEFAULT_PG_STATEMENT_TIMEOUT_SECS),
            max_lifetime_secs: Some(DEFAULT_PG_MAX_LIFETIME_SECS),
        }
    }
}

impl PostgresConfig {
    /// 组装 libpq DSN：`postgres://user:pass@host:port/db`。
    ///
    /// `url` 字段设置时直接返回（逃生口：SSL 参数/特殊字符等）。
    /// # Errors
    /// 离散字段缺失时返回错误，消息列出全部缺失项（fail fast，绝不静默降级）。
    pub fn to_dsn(&self) -> Result<String, String> {
        if let Some(url) = self.url.as_ref().filter(|u| !u.trim().is_empty()) {
            return Ok(url.clone());
        }
        let host = non_empty_field(&self.host);
        let username = non_empty_field(&self.username);
        let database = non_empty_field(&self.database);
        let mut missing = Vec::new();
        if host.is_none() {
            missing.push("host");
        }
        if username.is_none() {
            missing.push("username");
        }
        if database.is_none() {
            missing.push("database");
        }
        if !missing.is_empty() {
            return Err(format!(
                "storage.postgres 缺少必填字段: {}（或设置 url 整串 DSN）",
                missing.join(", ")
            ));
        }
        let port = self.port.unwrap_or(DEFAULT_PG_PORT);
        // 密码 percent-encode：DSN 是 URI，特殊字符（@:/ 等）必须转义
        // 缺失项已在上方逐项报错返回；此处防御性兜底（不 panic）
        let (Some(host), Some(username), Some(database)) = (host, username, database) else {
            return Err("storage.postgres 内部错误：字段校验与组装不一致".to_string());
        };
        let auth = match self.password.as_deref().filter(|p| !p.is_empty()) {
            Some(password) => format!("{}:{}@", username, encode_uri_component(password)),
            None => format!("{username}@"),
        };
        Ok(format!("postgres://{auth}{host}:{port}/{database}"))
    }

    /// 连接池大小（带默认值兜底）
    pub fn max_connections(&self) -> u32 {
        self.max_connections.unwrap_or(DEFAULT_PG_MAX_CONNECTIONS)
    }

    /// 预热连接数（带默认值兜底；不超过 max_connections）
    pub fn min_connections(&self) -> u32 {
        self.min_connections
            .unwrap_or(DEFAULT_PG_MIN_CONNECTIONS)
            .min(self.max_connections())
    }

    /// 连接超时（带默认值兜底）
    pub fn connect_timeout_secs(&self) -> u64 {
        self.connect_timeout_secs
            .unwrap_or(DEFAULT_PG_CONNECT_TIMEOUT_SECS)
    }

    /// 语句超时（带默认值兜底）
    pub fn statement_timeout_secs(&self) -> u64 {
        self.statement_timeout_secs
            .unwrap_or(DEFAULT_PG_STATEMENT_TIMEOUT_SECS)
    }

    /// 连接最大寿命（带默认值兜底）
    pub fn max_lifetime_secs(&self) -> u64 {
        self.max_lifetime_secs
            .unwrap_or(DEFAULT_PG_MAX_LIFETIME_SECS)
    }

    /// 脱敏描述（日志用，不含密码）
    pub fn describe(&self) -> String {
        if self.url.is_some() {
            return "<dsn via url>".to_string();
        }
        format!(
            "{}:{}/{} pool={}/{} connect_timeout={}s statement_timeout={}s max_lifetime={}s",
            self.host.as_deref().unwrap_or("?"),
            self.port.unwrap_or(DEFAULT_PG_PORT),
            self.database.as_deref().unwrap_or("?"),
            self.max_connections(),
            self.min_connections(),
            self.connect_timeout_secs(),
            self.statement_timeout_secs(),
            self.max_lifetime_secs(),
        )
    }
}

/// URI 组件编码（保留未保留字符，转义分隔符与高危字符）。
/// NON_ALPHANUMERIC 对密码场景足够保守（会多转义一些安全字符，无害）。
fn encode_uri_component(input: &str) -> String {
    utf8_percent_encode(input, NON_ALPHANUMERIC).to_string()
}

/// 取非空 trim 后的字段值（None/空白 → None）
fn non_empty_field(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsn_from_discrete_fields() {
        let config = PostgresConfig {
            host: Some("rcoder-pg.test.svc".into()),
            port: Some(5433),
            username: Some("rcoder".into()),
            password: Some("secret".into()),
            database: Some("rcoder".into()),
            ..PostgresConfig::default()
        };
        assert_eq!(
            config.to_dsn().unwrap(),
            "postgres://rcoder:secret@rcoder-pg.test.svc:5433/rcoder"
        );
    }

    #[test]
    fn dsn_escapes_password_special_chars() {
        let config = PostgresConfig {
            host: Some("db".into()),
            username: Some("u".into()),
            password: Some("p@ss/w:rd".into()),
            database: Some("d".into()),
            ..PostgresConfig::default()
        };
        let dsn = config.to_dsn().unwrap();
        assert!(
            dsn.starts_with("postgres://u:p%40ss%2Fw%3Ard@db:5432/d"),
            "{dsn}"
        );
    }

    #[test]
    fn dsn_url_overrides_discrete_fields() {
        let config = PostgresConfig {
            url: Some("postgres://x:y@h/db?sslmode=disable".into()),
            host: Some("ignored".into()),
            ..PostgresConfig::default()
        };
        assert_eq!(
            config.to_dsn().unwrap(),
            "postgres://x:y@h/db?sslmode=disable"
        );
    }

    #[test]
    fn missing_fields_listed_in_error() {
        let error = PostgresConfig::default().to_dsn().unwrap_err();
        assert!(error.contains("host"), "{error}");
        assert!(error.contains("username"), "{error}");
        assert!(error.contains("database"), "{error}");
    }

    #[test]
    fn no_password_yields_username_only_auth() {
        let config = PostgresConfig {
            host: Some("db".into()),
            username: Some("u".into()),
            database: Some("d".into()),
            ..PostgresConfig::default()
        };
        assert_eq!(config.to_dsn().unwrap(), "postgres://u@db:5432/d");
    }

    #[test]
    fn describe_hides_password() {
        let config = PostgresConfig {
            host: Some("db".into()),
            username: Some("u".into()),
            password: Some("secret".into()),
            database: Some("d".into()),
            ..PostgresConfig::default()
        };
        let described = config.describe();
        assert!(!described.contains("secret"), "{described}");
    }

    #[test]
    fn password_skipped_in_serialize_output() {
        let config = PostgresConfig {
            password: Some("secret".into()),
            ..PostgresConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("secret"), "{json}");
    }

    /// 池参数兜底：默认值生效；min_connections 不超过 max_connections。
    #[test]
    fn pool_defaults_and_min_capped_by_max() {
        let config = PostgresConfig::default();
        assert_eq!(config.max_connections(), DEFAULT_PG_MAX_CONNECTIONS);
        assert_eq!(config.min_connections(), DEFAULT_PG_MIN_CONNECTIONS);
        assert_eq!(config.max_lifetime_secs(), DEFAULT_PG_MAX_LIFETIME_SECS);

        let config = PostgresConfig {
            // min 故意配大于 max（错误配置）：getter 兜底压到 max
            min_connections: Some(20),
            max_connections: Some(5),
            ..PostgresConfig::default()
        };
        assert_eq!(config.min_connections(), 5);
    }
}
