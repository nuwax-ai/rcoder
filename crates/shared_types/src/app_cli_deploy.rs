//! app-cli 部署相位契约（rcoder ↔ app-cli 单一事实源）。
//!
//! 容器内 app-cli 管理 API `GET /v1/deploy/status` 的 `phase` 字段枚举。
//! 此前 rcoder 侧（app_manager hot_deploy）以裸字符串 `"running"`/`"failed"`
//! 比较——新增相位无编译期防护。收敛为共享枚举后：
//! - app-cli [`DeployStatus`](crate::app_cli_logs) 序列化直接产出本枚举
//!   （wire 值与历史字符串逐字节一致）；
//! - rcoder 侧判据用穷尽 `match`——新增变体时消费点编译错，强制显式决策
//!   该相位算「部署段完成」还是「继续等」。
//!
//! [`FromStr`] 对未知字符串返回 `Err`：滚动升级窗口旧镜像可能返回新版本
//! 不认识的相位，消费方按「继续等」处理（与编译期检查互补，不矛盾）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// app-cli server 状态机相位（wire 值 snake_case，锁死勿漂移）。
///
/// 转移图：`Idle → Deploying → Orchestrating → Running`；任一部署/编排
/// 失败进 `Failed`（error 细节走 status 响应的独立 `error` 字段，枚举
/// 不带负载）。`Failed` 常驻不被 Idle 覆盖，直到下一次部署请求受理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppCliDeployPhase {
    /// 空容器/等待首次部署（基础设施就绪即应答）。
    #[default]
    Idle,
    /// 部署段进行中：下载制品 → sha256 校验 → 解压 → 换 code。
    Deploying,
    /// 部署段完成，编排进行中：等 PG → migrate → 起服务 → pingap → readiness。
    Orchestrating,
    /// 编排完成，supervise 运行中（readiness 视 bridge 探活而定）。
    Running,
    /// 最近一次部署/编排失败（现场保留，可再次部署）。
    Failed,
}

impl AppCliDeployPhase {
    /// wire 字符串（与 serde 序列化值一致；`Display` 委托此处）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            AppCliDeployPhase::Idle => "idle",
            AppCliDeployPhase::Deploying => "deploying",
            AppCliDeployPhase::Orchestrating => "orchestrating",
            AppCliDeployPhase::Running => "running",
            AppCliDeployPhase::Failed => "failed",
        }
    }
}

impl std::fmt::Display for AppCliDeployPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AppCliDeployPhase {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "idle" => Ok(AppCliDeployPhase::Idle),
            "deploying" => Ok(AppCliDeployPhase::Deploying),
            "orchestrating" => Ok(AppCliDeployPhase::Orchestrating),
            "running" => Ok(AppCliDeployPhase::Running),
            "failed" => Ok(AppCliDeployPhase::Failed),
            other => Err(format!("unknown app-cli deploy phase: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 值快照：锁死 snake_case 序列化（Java/rcoder 消费契约）。
    #[test]
    fn wire_values_snapshot() {
        let cases = [
            (AppCliDeployPhase::Idle, "\"idle\""),
            (AppCliDeployPhase::Deploying, "\"deploying\""),
            (AppCliDeployPhase::Orchestrating, "\"orchestrating\""),
            (AppCliDeployPhase::Running, "\"running\""),
            (AppCliDeployPhase::Failed, "\"failed\""),
        ];
        for (phase, wire) in cases {
            assert_eq!(serde_json::to_string(&phase).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<AppCliDeployPhase>(wire).unwrap(),
                phase
            );
            assert_eq!(phase.to_string(), wire.trim_matches('"'));
            assert_eq!(phase.as_str().parse::<AppCliDeployPhase>().unwrap(), phase);
        }
    }

    /// 未知字符串 Err（滚动窗口旧/新镜像互不认识时的容错入口）。
    #[test]
    fn unknown_string_is_err() {
        assert!("migrating".parse::<AppCliDeployPhase>().is_err());
        assert!("".parse::<AppCliDeployPhase>().is_err());
        assert!("Running".parse::<AppCliDeployPhase>().is_err()); // 大小写敏感
    }
}
