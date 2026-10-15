//! Userapp PG 账号/库管理契约（跨 crate，按模块契约约定置于 shared_types）。
//!
//! 业务：Java 调 `POST /api/v1/userapp/db/{app_stage}/reset-password|create-database` 管理
//! userApp dev（UserappBuilder 开发容器）/ prod（运行容器）的容器内 PG——
//! 账号 upsert（存在改密 / 不存在建号，补齐 [`super::db_align`] 只重置不建号的
//! 缺口）与 API 化建库。
//!
//! 流程单头在 [`upsert_pg_user`]/[`create_pg_database`]，执行通道（容器 exec）
//! 由宿主以 [`PgCommandRunner`] 注入（与 db_align 同款抽象）。

use serde::{Deserialize, Serialize};

use crate::pg_utils::{
    pg_alter_password_cmd, pg_create_database_cmd, pg_create_role_cmd, pg_database_exists_cmd,
    pg_role_exists_cmd, validate_pg_identifier,
};

/// `POST /api/v1/userapp/db/{app_stage}/reset-password` 请求体。
#[derive(Deserialize, Serialize, Clone, utoipa::ToSchema)]
pub struct UserappDbResetPasswordRequest {
    /// Original caller identity for retries. Omission creates a new operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Reject requests targeting a replaced application lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_id: Option<String>,
    /// 应用 ID（定位 dev=开发容器 / prod=运行容器）
    pub app_id: String,
    /// 新密码（非空；允许任意字符含特殊符号）
    pub password: String,
    /// 目标账号名（可选，须过 PG 标识符白名单）：
    /// - 缺省：重置 superuser（`$POSTGRES_USER`，SQL CURRENT_USER 语义）
    /// - 指定：账号 upsert——角色已存在则 ALTER USER 改密，不存在则 CREATE ROLE 建号
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// Explicitly reconcile an uncertain password write; never issues ALTER again.
/// The original private input is required to verify the admission fingerprint.
#[derive(Debug, Deserialize, Serialize, Clone, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserappDbPasswordRecoveryRequest {
    pub lifecycle_id: String,
    pub operation_id: String,
    pub expected_revision: i64,
    pub original: UserappDbResetPasswordRequest,
}

impl UserappDbPasswordRecoveryRequest {
    pub fn validate(&self) -> Result<(), String> {
        self.original.validate()?;
        crate::validate_identifier(&self.lifecycle_id, "lifecycle_id")?;
        crate::validate_identifier(&self.operation_id, "operation_id")?;
        if self.expected_revision < 0 {
            return Err("Expected revision must be nonnegative".into());
        }
        if self.original.request_id.is_none() {
            return Err("Recovery requires the original request_id".into());
        }
        if self
            .original
            .lifecycle_id
            .as_ref()
            .is_some_and(|id| id != &self.lifecycle_id)
        {
            return Err("Original and recovery lifecycle differ".into());
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Clone, utoipa::ToSchema)]
pub struct UserappDbPasswordRecoveryResponse {
    pub operation_id: String,
    pub lifecycle_id: String,
    pub revision: i64,
    /// Operation state: Pending, Running, WaitingRetry, RecoveryRequired,
    /// Succeeded, Failed. A successful recovery response is terminal:
    /// Succeeded means verified commit; Failed means confirmed cancellation.
    pub state: crate::UserAppOperationState,
    /// Durable terminal outcome is confirmed; only exact physical lease cleanup remains.
    pub lease_cleanup_pending: bool,
}

/// Reconcile an uncertain explicit deployment password write under its
/// original operation identity. Never reissues the password write; the
/// original private input is required for TCP verification of a commit.
#[derive(Deserialize, Serialize, Clone, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserappDeployPgRecoveryRequest {
    pub app_id: String,
    pub lifecycle_id: String,
    pub operation_id: String,
    pub expected_revision: i64,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for UserappDeployPgRecoveryRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserappDeployPgRecoveryRequest")
            .field("app_id", &self.app_id)
            .field("lifecycle_id", &self.lifecycle_id)
            .field("operation_id", &self.operation_id)
            .field("username", &self.username)
            .field("password", &"<REDACTED>")
            .finish()
    }
}

impl UserappDeployPgRecoveryRequest {
    pub fn validate(&self) -> Result<(), String> {
        crate::validate_identifier(&self.app_id, "app_id")?;
        crate::validate_identifier(&self.lifecycle_id, "lifecycle_id")?;
        crate::validate_identifier(&self.operation_id, "operation_id")?;
        if self.expected_revision < 0 {
            return Err("Expected revision must be nonnegative".into());
        }
        validate_pg_identifier(&self.username)?;
        validate_password(&self.password)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Clone, utoipa::ToSchema)]
pub struct UserappDeployPgRecoveryResponse {
    pub operation_id: String,
    pub lifecycle_id: String,
    pub revision: i64,
    /// Operation state: Pending, Running, WaitingRetry, RecoveryRequired,
    /// Succeeded, Failed. After successful reconciliation the state is terminal
    /// Failed: the deployment itself never durably recorded completion.
    pub state: crate::UserAppOperationState,
    /// Receipt-proven outcome of the original password write:
    /// `captured`, `write_submitted`, `verified`, or `cancelled`.
    pub stage: crate::DatabasePasswordStage,
    /// Durable terminal outcome is confirmed; only exact physical lease cleanup remains.
    pub lease_cleanup_pending: bool,
}

/// `POST /api/v1/userapp/db/{app_stage}/create-database` 请求体。
#[derive(Debug, Deserialize, Serialize, Clone, utoipa::ToSchema)]
pub struct UserappDbCreateDatabaseRequest {
    /// 应用 ID（定位 dev=开发容器 / prod=运行容器）
    pub app_id: String,
    /// 新建数据库名（PG 标识符白名单校验）
    pub database: String,
    /// 库 owner（可选，PG 标识符白名单校验；缺省 = 执行者 superuser）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl std::fmt::Debug for UserappDbResetPasswordRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserappDbResetPasswordRequest")
            .field("app_id", &self.app_id)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

impl UserappDbResetPasswordRequest {
    /// Validate before resolving or waking any physical resource.
    pub fn validate(&self) -> Result<(), String> {
        crate::validate_identifier(&self.app_id, "app_id")?;
        for value in [&self.request_id, &self.lifecycle_id].into_iter().flatten() {
            crate::validate_identifier(value, "request/lifecycle identity")?;
        }
        validate_password(&self.password)?;
        if let Some(username) = &self.username {
            validate_pg_identifier(username)?;
        }
        Ok(())
    }
}

impl UserappDbCreateDatabaseRequest {
    /// Validate before resolving or waking any physical resource.
    pub fn validate(&self) -> Result<(), String> {
        crate::validate_identifier(&self.app_id, "app_id")?;
        validate_pg_identifier(&self.database)?;
        if let Some(owner) = &self.owner {
            validate_pg_identifier(owner)?;
        }
        Ok(())
    }
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.is_empty() || password.contains('\0') {
        return Err("password must be nonempty and contain no NUL bytes".into());
    }
    Ok(())
}

/// PG 凭据（跨环境共用的 wire 形状，字段名恒为 `pg`）：
/// - prod：`POST /api/v1/userapp/{app_id}/start|restart` 的 `pg` 字段——部署后
///   自动对齐（scram 校验，不一致则重置，结果见响应 `pg_aligned`）；
/// - dev：`POST /api/v1/userapp/dev/start|restart` 的 `pg` 字段——注入 dev
///   编排进程 env 的 `POSTGRES_USER`/`POSTGRES_PASSWORD`（覆盖容器默认透传值），
///   save-db-credential 改密后由调用方带上新凭据，避免编排 env 仍是镜像默认
///   `dev` 导致服务连不上库。
///
/// 从 app_manager `models/start.rs` 下沉（dev 链 file-server-userapp 与 prod 链
/// 共用同一契约；serde/ToSchema 形状不变）。
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq, utoipa::ToSchema)]
pub struct StartPgCredential {
    /// PG 账号名（已存在角色；须过 PG 标识符白名单）
    pub username: String,
    /// 目标密码（与开发环境保持一致的值）
    pub password: String,
}

impl std::fmt::Debug for StartPgCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartPgCredential")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("password_provided", &!self.password.is_empty())
            .field("password_length", &self.password.chars().count())
            .finish()
    }
}

/// 账号 upsert 结果（响应 message 区分"已创建"/"已重置"）。
#[derive(Debug, PartialEq, Eq, utoipa::ToSchema)]
pub enum DbUserUpsertOutcome {
    /// 角色原先不存在，已 CREATE ROLE 建号并设置密码
    Created,
    /// 角色已存在，已 ALTER USER 重置密码
    Reset,
}

/// 账号/库管理流程错误（类型化——调用方按 variant 映射 HTTP 错误码）。
#[derive(Debug)]
pub enum DbAdminError {
    /// 调用方输入问题（非法标识符/空密码）→ 400 语义
    InvalidInput(String),
    /// 库已存在 → 409 语义
    AlreadyExists(String),
    /// 容器侧执行失败（通道断/PG 未就绪/SQL 失败）→ 500 语义
    Command {
        stage: &'static str,
        detail: String,
        code: &'static str,
        cause_code: &'static str,
        evidence: super::db_align::PgCommandEvidence,
        diagnostic: Option<Box<super::db_align::PgCommandDiagnostic>>,
    },
}

impl std::fmt::Display for DbAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(m) => write!(f, "{m}"),
            Self::AlreadyExists(m) => write!(f, "{m}"),
            Self::Command { stage, detail, .. } => write!(f, "{stage}: {detail}"),
        }
    }
}

impl std::error::Error for DbAdminError {}

/// 账号 upsert 核心流程（角色存在检查 → 存在 ALTER 改密 / 不存在 CREATE ROLE 建号）。
///
/// 错误信息面向日志与 Java 排障；**不含密码**（只回 username）。
pub async fn upsert_pg_user(
    runner: &dyn super::db_align::PgCommandRunner,
    username: &str,
    password: &str,
) -> Result<DbUserUpsertOutcome, DbAdminError> {
    use super::db_align::{PgCommandEvidence, PgCommandMode};
    validate_pg_identifier(username).map_err(DbAdminError::InvalidInput)?;
    validate_password(password).map_err(DbAdminError::InvalidInput)?;
    let exists = runner
        .run(&pg_role_exists_cmd(username), PgCommandMode::ReadOnly)
        .await
        .map_err(|error| command_error("role-exists check", error))?;
    if exists.exit_code != 0 {
        return Err(DbAdminError::Command {
            stage: "role-exists check",
            detail: crate::sanitize_error_text(exists.stderr.trim()),
            code: crate::ERR_DATABASE_COMMAND_FAILED,
            cause_code: crate::ERR_DATABASE_COMMAND_FAILED,
            diagnostic: None,
            evidence: PgCommandEvidence::DefinitivelyRejected,
        });
    }
    let (command, outcome) = if exists.stdout.trim() == "1" {
        (
            pg_alter_password_cmd(username, password),
            DbUserUpsertOutcome::Reset,
        )
    } else {
        (
            pg_create_role_cmd(username, password),
            DbUserUpsertOutcome::Created,
        )
    };
    let applied = runner
        .run(&command, PgCommandMode::Write)
        .await
        .map_err(|error| {
            // Redact both the summary and the retained peer carrier before it
            // becomes a domain error; the typed write evidence stays intact.
            command_error("apply user upsert", error.with_credential_summary(
                "Password command transport failed; inspect the original operation before retrying",
            ))
        })?;
    if applied.exit_code != 0 {
        let evidence = super::db_align::single_statement_write_evidence(applied.exit_code);
        return Err(DbAdminError::Command {
            stage: "apply user upsert",
            detail: format!(
                "Password command exited with status {}; completion is unconfirmed",
                applied.exit_code
            ),
            code: if evidence == PgCommandEvidence::DefinitivelyRejected {
                crate::ERR_DATABASE_COMMAND_FAILED
            } else {
                crate::ERR_OPERATION_OUTCOME_UNKNOWN
            },
            cause_code: crate::ERR_DATABASE_COMMAND_FAILED,
            diagnostic: None,
            evidence,
        });
    }
    Ok(outcome)
}

fn command_error(stage: &'static str, error: super::db_align::PgCommandError) -> DbAdminError {
    DbAdminError::Command {
        stage,
        code: error.code,
        cause_code: error.cause_code,
        evidence: error.evidence,
        diagnostic: error.diagnostic,
        detail: crate::sanitize_error_text(&error.detail),
    }
}

/// A missing reply from CREATE is an unknown write, never an automatic retry.
pub async fn create_pg_database(
    runner: &dyn super::db_align::PgCommandRunner,
    database: &str,
    owner: Option<&str>,
) -> Result<(), DbAdminError> {
    use super::db_align::{PgCommandEvidence, PgCommandMode};
    validate_pg_identifier(database).map_err(DbAdminError::InvalidInput)?;
    if let Some(owner) = owner {
        validate_pg_identifier(owner).map_err(DbAdminError::InvalidInput)?;
    }
    let exists = runner
        .run(&pg_database_exists_cmd(database), PgCommandMode::ReadOnly)
        .await
        .map_err(|error| command_error("database-exists check", error))?;
    if exists.exit_code != 0 {
        return Err(DbAdminError::Command {
            stage: "database-exists check",
            detail: crate::sanitize_error_text(exists.stderr.trim()),
            code: crate::ERR_DATABASE_COMMAND_FAILED,
            cause_code: crate::ERR_DATABASE_COMMAND_FAILED,
            diagnostic: None,
            evidence: PgCommandEvidence::DefinitivelyRejected,
        });
    }
    if exists.stdout.trim() == "1" {
        return Err(DbAdminError::AlreadyExists(format!(
            "database {database} already exists"
        )));
    }
    let created = runner
        .run(
            &pg_create_database_cmd(database, owner),
            PgCommandMode::Write,
        )
        .await
        .map_err(|error| command_error("create database", error))?;
    if created.exit_code != 0 {
        let evidence = super::db_align::single_statement_write_evidence(created.exit_code);
        let original_error = DbAdminError::Command {
            stage: "create database",
            detail: crate::sanitize_error_text(created.stderr.trim()),
            code: if evidence == PgCommandEvidence::DefinitivelyRejected {
                crate::ERR_DATABASE_COMMAND_FAILED
            } else {
                crate::ERR_OPERATION_OUTCOME_UNKNOWN
            },
            cause_code: crate::ERR_DATABASE_COMMAND_FAILED,
            diagnostic: None,
            evidence,
        };
        if evidence == PgCommandEvidence::OutcomeUnknown {
            // An existence observation cannot prove which CREATE committed.
            // Preserve the original write outcome and do not reissue it.
            return Err(original_error);
        }
        // A definite single-statement SQL rejection permits the existing
        // read-only race check. Its failure never replaces the original SQL result.
        if let Ok(recheck) = runner
            .run(&pg_database_exists_cmd(database), PgCommandMode::ReadOnly)
            .await
            && recheck.exit_code == 0
            && recheck.stdout.trim() == "1"
        {
            return Err(DbAdminError::AlreadyExists(format!(
                "database {database} already exists"
            )));
        }
        return Err(original_error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::db_align::{CommandOutcome, PgCommandRunner};
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn db_admin_requests_use_app_and_stage_without_user_identity() {
        let reset: UserappDbResetPasswordRequest = serde_json::from_value(serde_json::json!({
            "app_id": "app1", "password": "private_reset_marker", "username": "business"
        }))
        .unwrap();
        assert_eq!(reset.app_id, "app1");
        assert!(
            serde_json::to_value(&reset)
                .unwrap()
                .get("user_id")
                .is_none()
        );
        assert!(!format!("{reset:?}").contains("private_reset_marker"));
        let create: UserappDbCreateDatabaseRequest = serde_json::from_value(serde_json::json!({
            "app_id": "app1", "database": "db1"
        }))
        .unwrap();
        assert_eq!(create.app_id, "app1");
        assert!(
            serde_json::to_value(&create)
                .unwrap()
                .get("user_id")
                .is_none()
        );
    }

    #[tokio::test]
    async fn upsert_rejects_nul_password_before_any_command() {
        let runner = ScriptedRunner::new(vec![]);
        assert!(matches!(
            upsert_pg_user(&runner, "biz", "bad\0password").await,
            Err(DbAdminError::InvalidInput(_))
        ));
        assert!(runner.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn db_admin_input_validation_covers_all_resource_fields() {
        let mut reset = UserappDbResetPasswordRequest {
            request_id: None,
            lifecycle_id: None,
            app_id: "app1".into(),
            username: Some("business".into()),
            password: "password".into(),
        };
        assert!(reset.validate().is_ok());
        reset.username = Some("bad-name".into());
        assert!(reset.validate().is_err());
        reset.username = None;
        reset.password = "bad\0password".into();
        assert!(reset.validate().is_err());
        let mut create = UserappDbCreateDatabaseRequest {
            app_id: "app1".into(),
            database: "db1".into(),
            owner: None,
        };
        assert!(create.validate().is_ok());
        create.owner = Some("bad-name".into());
        assert!(create.validate().is_err());
        create.owner = None;
        create.database = "bad-name".into();
        assert!(create.validate().is_err());
    }

    #[tokio::test]
    async fn password_command_errors_never_expose_remote_sql() {
        for result in [
            Err("transport echoed SQL with private_error_marker".into()),
            Ok(CommandOutcome {
                exit_code: 1,
                stdout: String::new(),
                stderr: "SQL error: ALTER USER biz PASSWORD 'private_error_marker'".into(),
            }),
        ] {
            let runner = ScriptedRunner::new(vec![ok(0, "1"), result]);
            let error = upsert_pg_user(&runner, "biz", "private_error_marker")
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                DbAdminError::Command {
                    stage: "apply user upsert",
                    ..
                }
            ));
            assert!(!format!("{error:?} {error}").contains("private_error_marker"));
            assert_eq!(runner.seen.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn lost_create_reply_does_not_reissue_or_report_absence() {
        let runner = ScriptedRunner::new(vec![ok(0, ""), Err("CREATE reply lost".into())]);
        let error = create_pg_database(&runner, "newdb", None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbAdminError::Command {
                code: crate::ERR_OPERATION_OUTCOME_UNKNOWN,
                cause_code: crate::ERR_CONTAINER_EXEC_FAILED,
                evidence: super::super::db_align::PgCommandEvidence::OutcomeUnknown,
                stage: "create database",
                ..
            }
        ));
        assert_eq!(
            runner.seen.lock().unwrap().len(),
            2,
            "only existence read and one CREATE may dispatch"
        );
    }

    #[tokio::test]
    async fn query_failure_never_dispatches_create_database() {
        let runner = ScriptedRunner::new(vec![Err("existence read failed".into())]);
        let error = create_pg_database(&runner, "newdb", None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbAdminError::Command {
                stage: "database-exists check",
                code: crate::ERR_CONTAINER_EXEC_FAILED,
                ..
            }
        ));
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sql_rejection_differs_from_lost_connection_write() {
        for (status, code, evidence) in [
            (
                3,
                crate::ERR_DATABASE_COMMAND_FAILED,
                super::super::db_align::PgCommandEvidence::DefinitivelyRejected,
            ),
            (
                2,
                crate::ERR_OPERATION_OUTCOME_UNKNOWN,
                super::super::db_align::PgCommandEvidence::OutcomeUnknown,
            ),
        ] {
            let runner = ScriptedRunner::new(vec![ok(0, "1"), ok(status, "")]);
            let error = upsert_pg_user(&runner, "biz", "secret").await.unwrap_err();
            assert!(
                matches!(error, DbAdminError::Command { code: actual_code, evidence: actual_evidence, .. } if actual_code == code && actual_evidence == evidence)
            );
            assert_eq!(runner.seen.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn lost_create_connection_is_not_overwritten_by_exists_observation() {
        let runner = ScriptedRunner::new(vec![ok(0, "0"), ok(2, ""), ok(0, "1")]);
        let error = create_pg_database(&runner, "business", None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbAdminError::Command {
                code: crate::ERR_OPERATION_OUTCOME_UNKNOWN,
                evidence: super::super::db_align::PgCommandEvidence::OutcomeUnknown,
                stage: "create database",
                ..
            }
        ));
        assert_eq!(runner.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rejected_create_keeps_original_sql_failure_when_recheck_fails() {
        let runner =
            ScriptedRunner::new(vec![ok(0, "0"), ok(3, ""), Err("read unavailable".into())]);
        let error = create_pg_database(&runner, "business", None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbAdminError::Command {
                code: crate::ERR_DATABASE_COMMAND_FAILED,
                evidence: super::super::db_align::PgCommandEvidence::DefinitivelyRejected,
                stage: "create database",
                ..
            }
        ));
        assert_eq!(runner.seen.lock().unwrap().len(), 3);
    }

    /// 脚本化 runner：按命令内容返回预设结果（与 db_align 的 ScriptedRunner 同款）。
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
            if cmd.contains("pg_roles") {
                "role_exists"
            } else if cmd.contains("ALTER USER") {
                "alter"
            } else if cmd.contains("CREATE ROLE") {
                "create_role"
            } else if cmd.contains("pg_database") {
                "db_exists"
            } else if cmd.contains("CREATE DATABASE") {
                "create_db"
            } else {
                "unknown"
            }
        }
    }

    #[async_trait::async_trait]
    impl PgCommandRunner for ScriptedRunner {
        async fn run(
            &self,
            command: &str,
            mode: crate::PgCommandMode,
        ) -> Result<CommandOutcome, crate::PgCommandError> {
            self.seen.lock().unwrap().push(command.to_string());
            self.results.lock().unwrap().remove(0).map_err(|detail| {
                crate::PgCommandError::transport(mode, crate::ERR_CONTAINER_EXEC_FAILED, detail)
            })
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
    async fn upsert_existing_role_resets() {
        let runner = ScriptedRunner::new(vec![ok(0, "1"), ok(0, "ALTER")]);
        let out = upsert_pg_user(&runner, "biz", "pw").await.unwrap();
        assert_eq!(out, DbUserUpsertOutcome::Reset);
        let kinds: Vec<&str> = runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|c| ScriptedRunner::kind_of(c))
            .collect();
        assert_eq!(kinds, vec!["role_exists", "alter"]);
    }

    #[tokio::test]
    async fn upsert_missing_role_creates() {
        let runner = ScriptedRunner::new(vec![ok(0, ""), ok(0, "CREATE ROLE")]);
        let out = upsert_pg_user(&runner, "newbie", "pw").await.unwrap();
        assert_eq!(out, DbUserUpsertOutcome::Created);
        let kinds: Vec<&str> = runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|c| ScriptedRunner::kind_of(c))
            .collect();
        assert_eq!(kinds, vec!["role_exists", "create_role"]);
    }

    #[tokio::test]
    async fn upsert_rejects_invalid_username_without_running() {
        let runner = ScriptedRunner::new(vec![]);
        let err = upsert_pg_user(&runner, "bad-name", "pw").await.unwrap_err();
        assert!(matches!(err, DbAdminError::InvalidInput(_)));
        assert!(runner.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_rejects_empty_password() {
        let runner = ScriptedRunner::new(vec![]);
        assert!(matches!(
            upsert_pg_user(&runner, "biz", "").await.unwrap_err(),
            DbAdminError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    async fn create_database_happy_path() {
        let runner = ScriptedRunner::new(vec![ok(0, ""), ok(0, "CREATE DATABASE")]);
        create_pg_database(&runner, "mydb", Some("biz"))
            .await
            .unwrap();
        let kinds: Vec<&str> = runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|c| ScriptedRunner::kind_of(c))
            .collect();
        assert_eq!(kinds, vec!["db_exists", "create_db"]);
    }

    #[tokio::test]
    async fn create_database_conflict_detected_before_create() {
        let runner = ScriptedRunner::new(vec![ok(0, "1")]);
        let err = create_pg_database(&runner, "mydb", None).await.unwrap_err();
        assert!(matches!(err, DbAdminError::AlreadyExists(_)));
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_database_race_recheck_reports_conflict() {
        // ON_ERROR_STOP 的明确 SQL 拒绝（exit 3），复检发现已被并发创建。
        // exit 1/2 不能证明原写入结果，另由 Unknown 反例保护。
        let runner = ScriptedRunner::new(vec![ok(0, ""), ok(3, "already exists"), ok(0, "1")]);
        let err = create_pg_database(&runner, "mydb", None).await.unwrap_err();
        assert!(matches!(err, DbAdminError::AlreadyExists(_)));
        assert_eq!(runner.seen.lock().unwrap().len(), 3);
    }
    #[test]
    fn runtime_credentials_debug_is_redacted_through_nested_run_config() {
        let config = crate::userapp::runtime_control::OperationRunConfig {
            pg: Some(StartPgCredential {
                username: "runtime".into(),
                password: "private-password-test-marker".into(),
            }),
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("private-password-test-marker"));
        assert!(debug.contains("[REDACTED]"));
        assert!(debug.contains("password_provided: true"));
        assert!(debug.contains("password_length: 28"));
        let missing = StartPgCredential {
            username: "runtime".into(),
            password: String::new(),
        };
        let debug = format!("{missing:?}");
        assert!(debug.contains("password_provided: false"));
        assert!(debug.contains("password_length: 0"));
        // Redaction changes only diagnostic formatting, not the private wire
        // used to transfer operation configuration to its execution owner.
        assert_eq!(
            serde_json::to_value(&config).unwrap()["pg"]["password"],
            "private-password-test-marker"
        );
    }
}
