//! Userapp 阶段维度（dev/prod）——路径段 `{app_stage}` 的统一解析契约。
//!
//! db 三接口（`/api/v1/userapp/db/{app_stage}/...`）与文件/存储八接口
//! （`/api/v1/userapp/{app_id}/{app_stage}/...`）共用同一词表与解析，
//! 与转发层 `X-App-Stage` header 同一词汇（app_stage）：
//! - `dev`：开发容器（UserappBuilder）——workspace 开发卷，幂等 ensure 常驻
//! - `prod`：生产运行容器（Userapp）——per-app 运行卷，stopped 语义 + 唤醒
//!
//! app_stage 必填（无缺省值）：平台同时存在两环境容器，隐式缺省必歧义。

/// Userapp 目标阶段（路径段 `{app_stage}` 的类型化形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserappStage {
    /// 开发容器（UserappBuilder）：开发卷 workspace，幂等 ensure 常驻
    Dev,
    /// 生产运行容器（Userapp）：per-app 运行卷，stopped 语义 + 唤醒
    Prod,
}

impl UserappStage {
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

/// app_stage 路径段非法时的统一报错文案（各 handler 共用，保证 Java 侧文案一致）。
pub fn invalid_app_stage_error(value: &str) -> String {
    format!("path segment `app_stage` must be `dev` or `prod` (got: {value})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_only_dev_and_prod() {
        assert_eq!(UserappStage::parse("dev"), Some(UserappStage::Dev));
        assert_eq!(UserappStage::parse("prod"), Some(UserappStage::Prod));
        // 严格小写、无空白容忍——app_stage 必填显式，不做宽松匹配
        for bad in ["Dev", "PROD", " development", "prod ", "staging", ""] {
            assert_eq!(
                UserappStage::parse(bad),
                None,
                "unexpectedly accepted: {bad:?}"
            );
        }
    }

    #[test]
    fn as_str_round_trips() {
        for app_stage in [UserappStage::Dev, UserappStage::Prod] {
            assert_eq!(UserappStage::parse(app_stage.as_str()), Some(app_stage));
        }
        assert_eq!(UserappStage::Dev.as_str(), "dev");
        assert_eq!(UserappStage::Prod.as_str(), "prod");
    }

    #[test]
    fn invalid_app_stage_error_carries_offending_value() {
        let msg = invalid_app_stage_error("staging");
        assert!(msg.contains("must be `dev` or `prod`"), "{msg}");
        assert!(msg.contains("staging"), "{msg}");
    }
}
