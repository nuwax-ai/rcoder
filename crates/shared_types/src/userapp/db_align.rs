//! UserApp PG 凭据对齐契约（跨 crate，按模块契约约定置于 shared_types）。
//!
//! 业务：开发环境（UserAppBuilder 开发容器）与部署环境（UserApp 运行容器）的
//! 容器内 PG 账号密码保持一致——Java 调 `POST /api/userapp/db/{dev|prod}/align-credentials`
//! 传入目标凭据，rcoder 验证（TCP scram）→ 不一致则重置（本地 trust ALTER USER）。
//!
//! 流程单头在 [`align_pg_credentials`]，执行通道（容器 exec / 容器内 file-server
//! execute-command）由宿主以 [`PgCommandRunner`] 注入。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::pg_utils::{
    pg_alter_password_cmd, pg_role_exists_cmd, pg_verify_credentials_cmd, validate_pg_identifier,
};

/// `POST /api/userapp/db/{env}/align-credentials` 请求体（dev/prod 两接口一致）。
#[derive(Debug, Deserialize, Serialize, Clone, utoipa::ToSchema)]
pub struct AlignCredentialsRequest {
    /// 应用 ID（定位 dev=开发容器 / prod=运行容器）
    pub app_id: String,
    /// PG 账号名（已存在的任意账号；须过 PG 标识符白名单）
    pub username: String,
    /// 目标密码（开发与部署环境对齐后的值）
    pub password: String,
}

/// 对齐结果。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AlignCredentialsOutcome {
    /// 凭据已一致（验证通过或重置后复验通过）
    pub aligned: bool,
    /// 是否执行了重置（false=传入密码本就与当前一致）
    pub reset_performed: bool,
    /// dbx 预置连接同步命令执行结果（仅重置发生后触发）：
    /// true=执行成功（含"指定账号非 local-pg 在用账号、无事可做"——命令条件
    /// 内建，条件不满足同样 exit 0）；false=执行失败（详情见 dbx_error）；
    /// None=未触发（密码本就一致，未发生重置）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbx_synced: Option<bool>,
    /// dbx 同步失败详情（成功/未触发为 None；**不含密码**）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbx_error: Option<String>,
}

/// 命令执行结果（exit_code + 输出；与 runtime exec 的 ExecResult 对齐）。
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

/// 容器内 shell 命令执行通道（宿主注入）：
/// - prod：app_manager 经 runtime exec（UserApp 运行容器）
/// - dev：rcoder 经开发容器内 file-server `execute-command`（HTTP）
#[async_trait]
pub trait PgCommandRunner: Send + Sync {
    async fn run(&self, command: &str) -> Result<CommandOutcome, String>;
}

/// 凭据对齐流程错误（类型化——调用方按 variant 映射 HTTP 错误码，
/// 不做错误字符串匹配这类脆弱分类）。
#[derive(Debug)]
pub enum AlignError {
    /// 调用方输入问题（非法标识符/空密码）→ 400 语义
    InvalidInput(String),
    /// 目标 PG 角色不存在（对齐只重置密码，不建号）→ 400 语义
    RoleMissing(String),
    /// 容器侧执行失败（通道断/PG 未就绪/SQL 失败）→ 502/500 语义
    Command { stage: &'static str, detail: String },
}

impl std::fmt::Display for AlignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(m) => write!(f, "{m}"),
            Self::RoleMissing(m) => write!(f, "{m}"),
            Self::Command { stage, detail } => write!(f, "{stage}: {detail}"),
        }
    }
}

impl std::error::Error for AlignError {}

/// 凭据对齐核心流程（验证 → 角色存在检查 → 重置 → 复验）。
///
/// 错误信息面向日志与 Java 排障；**不含密码**（只回 username）。
pub async fn align_pg_credentials(
    runner: &dyn PgCommandRunner,
    username: &str,
    password: &str,
) -> Result<AlignCredentialsOutcome, AlignError> {
    validate_pg_identifier(username).map_err(AlignError::InvalidInput)?;
    if password.is_empty() {
        return Err(AlignError::InvalidInput(
            "password must not be empty".to_string(),
        ));
    }

    // 1. 验证（TCP scram）：exit 0 = 一致，直接返回
    let verify = runner
        .run(&pg_verify_credentials_cmd(username, password))
        .await
        .map_err(|e| AlignError::Command {
            stage: "verify credentials",
            detail: e,
        })?;
    if verify.exit_code == 0 {
        return Ok(AlignCredentialsOutcome {
            aligned: true,
            reset_performed: false,
            dbx_synced: None,
            dbx_error: None,
        });
    }

    // 2. 不一致 → 角色存在检查（区分"密码不同"与"账号不存在"，后者明确报错）
    let exists = runner
        .run(&pg_role_exists_cmd(username))
        .await
        .map_err(|e| AlignError::Command {
            stage: "role-exists check",
            detail: e,
        })?;
    if exists.exit_code != 0 {
        return Err(AlignError::Command {
            stage: "role-exists check",
            detail: exists.stderr.trim().to_string(),
        });
    }
    if exists.stdout.trim() != "1" {
        return Err(AlignError::RoleMissing(format!(
            "PG role `{username}` does not exist; create it first (align only resets passwords)"
        )));
    }

    // 3. 重置（trust ALTER USER）
    let alter = runner
        .run(&pg_alter_password_cmd(username, password))
        .await
        .map_err(|e| AlignError::Command {
            stage: "alter password",
            detail: e,
        })?;
    if alter.exit_code != 0 {
        return Err(AlignError::Command {
            stage: "alter password",
            detail: alter.stderr.trim().to_string(),
        });
    }

    // 4. 复验（scram 确认生效）
    let reverify = runner
        .run(&pg_verify_credentials_cmd(username, password))
        .await
        .map_err(|e| AlignError::Command {
            stage: "re-verify after reset",
            detail: e,
        })?;
    if reverify.exit_code != 0 {
        return Err(AlignError::Command {
            stage: "re-verify after reset",
            detail: reverify.stderr.trim().to_string(),
        });
    }

    // 5. dbx 预置连接同步（仅重置发生后）：重写 connections.json + 重启 dbx
    //    （fork dbx 启动按 id upsert 吸收）。命令条件内建——指定账号非
    //    local-pg 在用账号（$POSTGRES_USER）时无事可做 exit 0。
    //    失败不阻断：密码已生效，结果落字段供 Java 感知重试；错误不含密码。
    let (dbx_synced, dbx_error) = match runner
        .run(&crate::userapp::dbx_sync::dbx_sync_cmd_for_user(
            username, password,
        ))
        .await
    {
        Ok(r) if r.exit_code == 0 => (Some(true), None),
        Ok(r) => (
            Some(false),
            Some(format!("exit {}: {}", r.exit_code, r.stderr.trim())),
        ),
        Err(e) => (Some(false), Some(e)),
    };
    Ok(AlignCredentialsOutcome {
        aligned: true,
        reset_performed: true,
        dbx_synced,
        dbx_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 脚本化 runner：按命令内容返回预设 exit_code（验证 pg_verify_credentials_cmd
    /// 生成的前缀识别请求类型）。
    struct ScriptedRunner {
        results: Mutex<Vec<Result<CommandOutcome, String>>>,
        seen: Mutex<Vec<String>>,
    }

    impl ScriptedRunner {
        fn new(results: Vec<Result<CommandOutcome, String>>) -> Self {
            Self {
                results: Mutex::new(results),
                seen: Mutex::new(vec![]),
            }
        }

        fn kind_of(cmd: &str) -> &'static str {
            if cmd.starts_with("PGPASSWORD=") {
                "verify"
            } else if cmd.contains("pg_roles") {
                "role_exists"
            } else if cmd.contains("ALTER USER") {
                "alter"
            } else if cmd.contains("connections.json") {
                "dbx_sync"
            } else {
                "unknown"
            }
        }
    }

    #[async_trait]
    impl PgCommandRunner for ScriptedRunner {
        async fn run(&self, command: &str) -> Result<CommandOutcome, String> {
            self.seen.lock().unwrap().push(command.to_string());
            self.results.lock().unwrap().remove(0)
        }
    }

    fn ok(exit_code: i64, stdout: &str) -> Result<CommandOutcome, String> {
        Ok(CommandOutcome {
            exit_code,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    #[tokio::test]
    async fn already_aligned_skips_reset() {
        let runner = ScriptedRunner::new(vec![ok(0, "1")]);
        let out = align_pg_credentials(&runner, "app", "pw").await.unwrap();
        assert!(out.aligned && !out.reset_performed);
        assert_eq!(out.dbx_synced, None, "未重置不触发 dbx 同步");
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
        assert_eq!(
            ScriptedRunner::kind_of(&runner.seen.lock().unwrap()[0]),
            "verify"
        );
    }

    #[tokio::test]
    async fn mismatch_resets_and_reverifies() {
        let runner = ScriptedRunner::new(vec![
            ok(2, ""),           // verify 失败（密码不一致）
            ok(0, "1"),          // 角色存在
            ok(0, "ALTER USER"), // 重置成功
            ok(0, "1"),          // 复验通过
            ok(0, ""),           // dbx 同步成功
        ]);
        let out = align_pg_credentials(&runner, "app", "pw").await.unwrap();
        assert!(out.aligned && out.reset_performed);
        assert_eq!(out.dbx_synced, Some(true));
        assert!(out.dbx_error.is_none());
        let kinds: Vec<&str> = runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|c| ScriptedRunner::kind_of(c))
            .collect();
        assert_eq!(
            kinds,
            vec!["verify", "role_exists", "alter", "verify", "dbx_sync"]
        );
    }

    #[tokio::test]
    async fn dbx_sync_failure_does_not_block_alignment() {
        let runner = ScriptedRunner::new(vec![
            ok(2, ""),           // verify 失败
            ok(0, "1"),          // 角色存在
            ok(0, "ALTER USER"), // 重置成功
            ok(0, "1"),          // 复验通过
            ok(1, "dbx down"),   // dbx 同步失败
        ]);
        let out = align_pg_credentials(&runner, "app", "pw").await.unwrap();
        assert!(out.aligned && out.reset_performed, "同步失败不阻断对齐");
        assert_eq!(out.dbx_synced, Some(false));
        assert!(
            out.dbx_error.as_deref().unwrap().starts_with("exit 1"),
            "got: {:?}",
            out.dbx_error
        );
    }

    #[tokio::test]
    async fn missing_role_reports_clearly() {
        let runner = ScriptedRunner::new(vec![ok(2, ""), ok(0, "")]); // 角色不存在
        let err = align_pg_credentials(&runner, "nobody", "pw")
            .await
            .unwrap_err();
        assert!(
            matches!(err, AlignError::RoleMissing(_)),
            "expect RoleMissing, got: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_invalid_username_without_running() {
        let runner = ScriptedRunner::new(vec![]);
        let err = align_pg_credentials(&runner, "bad-name", "pw")
            .await
            .unwrap_err();
        assert!(matches!(err, AlignError::InvalidInput(_)));
        assert!(runner.seen.lock().unwrap().is_empty());
    }
}
