//! UserApp 环境维度（dev/prod）——路径段 `{env}` 的统一解析契约。
//!
//! db 三接口（`/api/v1/userapp/db/{env}/...`）与文件/存储八接口
//! （`/api/v1/userapp/{app_id}/{env}/...`）共用同一词表与解析：
//! - `dev`：开发容器（UserAppBuilder）——workspace 开发卷，幂等 ensure 常驻
//! - `prod`：生产运行容器（UserApp）——per-app 运行卷，stopped 语义 + 唤醒
//!
//! env 必填（无缺省值）：平台同时存在两环境容器，隐式缺省必歧义。

/// UserApp 目标环境（路径段 `{env}` 的类型化形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserappEnv {
    /// 开发容器（UserAppBuilder）：开发卷 workspace，幂等 ensure 常驻
    Dev,
    /// 生产运行容器（UserApp）：per-app 运行卷，stopped 语义 + 唤醒
    Prod,
}

impl UserappEnv {
    /// 解析路径段（仅认 `dev`/`prod`；非法返回 None，调用方 400）。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "dev" => Some(Self::Dev),
            "prod" => Some(Self::Prod),
            _ => None,
        }
    }

    /// 路径段原值（日志/报错文案用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Prod => "prod",
        }
    }
}

/// env 路径段非法时的统一报错文案（各 handler 共用，保证 Java 侧文案一致）。
pub fn invalid_env_error(value: &str) -> String {
    format!("path segment `env` must be `dev` or `prod` (got: {value})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_only_dev_and_prod() {
        assert_eq!(UserappEnv::parse("dev"), Some(UserappEnv::Dev));
        assert_eq!(UserappEnv::parse("prod"), Some(UserappEnv::Prod));
        // 严格小写、无空白容忍——env 必填显式，不做宽松匹配
        for bad in ["Dev", "PROD", " development", "prod ", "staging", ""] {
            assert_eq!(
                UserappEnv::parse(bad),
                None,
                "unexpectedly accepted: {bad:?}"
            );
        }
    }

    #[test]
    fn as_str_round_trips() {
        for env in [UserappEnv::Dev, UserappEnv::Prod] {
            assert_eq!(UserappEnv::parse(env.as_str()), Some(env));
        }
        assert_eq!(UserappEnv::Dev.as_str(), "dev");
        assert_eq!(UserappEnv::Prod.as_str(), "prod");
    }

    #[test]
    fn invalid_env_error_carries_offending_value() {
        let msg = invalid_env_error("staging");
        assert!(msg.contains("must be `dev` or `prod`"), "{msg}");
        assert!(msg.contains("staging"), "{msg}");
    }
}
