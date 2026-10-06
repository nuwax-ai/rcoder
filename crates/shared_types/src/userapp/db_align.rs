//! Userapp PG 凭据对齐契约（跨 crate，按模块契约约定置于 shared_types）。
//!
//! 业务：开发环境（UserappBuilder 开发容器）与部署环境（Userapp 运行容器）的
//! 容器内 PG 账号密码保持一致——start 部署链带 `pg.username`/`pg.password` 时
//! 自动对齐（app_manager 函数级消费；独立 HTTP 入口 align-credentials 已下线）：
//! 验证（TCP scram）→ 不一致则重置（本地 trust ALTER USER）。
//!
//! 流程单头在 [`align_pg_credentials`]，执行通道（容器 exec / 容器内 file-server
//! execute-command）由宿主以 [`PgCommandRunner`] 注入。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::pg_utils::{
    pg_alter_password_cmd, pg_role_exists_cmd, pg_verify_credentials_cmd, validate_pg_identifier,
};

/// start 部署链 PG 凭据对齐请求体（`pg.username`/`pg.password` 装配而来，
/// app_manager 函数级消费；原独立 HTTP 入口 align-credentials 已下线）。
#[derive(Deserialize, Serialize, Clone, utoipa::ToSchema)]
pub struct AlignCredentialsRequest {
    /// 应用 ID（定位 dev=开发容器 / prod=运行容器）
    pub app_id: String,
    /// PG 账号名（已存在的任意账号；须过 PG 标识符白名单）
    pub username: String,
    /// 目标密码（开发与部署环境对齐后的值）
    pub password: String,
}

impl std::fmt::Debug for AlignCredentialsRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignCredentialsRequest")
            .field("app_id", &self.app_id)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Mutation evidence, independent of whether business services became Ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialMutationEvidence {
    /// This attempt never dispatched a password-changing command.
    NotAttempted,
    /// A changing command was dispatched, but its result is not proven.
    Unknown,
    /// PostgreSQL acknowledged ALTER, but subsequent TCP verification failed.
    AppliedButUnverified,
}

/// 对齐结果。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AlignCredentialsOutcome {
    /// 凭据已一致（验证通过或重置后复验通过）
    pub aligned: bool,
    /// 是否执行了重置（false=传入密码本就与当前一致）
    pub reset_performed: bool,
}

/// 命令执行结果（exit_code + 输出；与 runtime exec 的 ExecResult 对齐）。
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

/// The caller declares whether a command can change persistent data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgCommandMode {
    ReadOnly,
    Write,
}

/// Dispatch evidence must come from the transport, never from shell text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgCommandEvidence {
    NotDispatched,
    DefinitivelyRejected,
    OutcomeUnknown,
}

/// Original downstream failure, independent of write-dispatch evidence.
/// Its operation identifies the child request; a captured parent operation wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgCommandDiagnostic {
    pub code: String,
    pub operation_id: Option<String>,
    pub blocker: Option<crate::UserAppOperationBlocker>,
    pub error_detail: Option<crate::ErrorDetail>,
}
impl PgCommandDiagnostic {
    pub fn into_app_error(
        self,
        code: &str,
        cause: &str,
        stage: &str,
        detail: &str,
        retryable: bool,
    ) -> crate::AppError {
        let protected = matches!(
            code,
            crate::ERR_OPERATION_OUTCOME_UNKNOWN | crate::ERR_RECOVERY_REQUIRED
        );
        let effective = if protected || self.code.is_empty() {
            code
        } else {
            &self.code
        };
        let mut diagnostic = self.error_detail.unwrap_or_else(|| {
            crate::ErrorDetail::new(
                if self.code.is_empty() {
                    cause
                } else {
                    &self.code
                },
                stage,
                detail,
            )
            .with_retryable(retryable)
        });
        diagnostic = diagnostic.localized(crate::current_request_locale());
        diagnostic.retryable &= !protected;
        let mut error =
            crate::AppError::with_message(effective, detail).with_error_detail(diagnostic);
        if let Some(id) = self.operation_id {
            error = error.with_operation_id(id);
        }
        if let Some(blocker) = self.blocker {
            error = error.with_blocker(blocker);
        }
        error
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgCommandError {
    pub code: &'static str,
    pub cause_code: &'static str,
    pub detail: String,
    pub evidence: PgCommandEvidence,
    pub diagnostic: Option<Box<PgCommandDiagnostic>>,
}

impl PgCommandError {
    pub fn new(code: &'static str, detail: impl Into<String>, evidence: PgCommandEvidence) -> Self {
        Self {
            code,
            cause_code: code,
            detail: detail.into(),
            evidence,
            diagnostic: None,
        }
    }
    pub fn with_diagnostic(mut self, diagnostic: PgCommandDiagnostic) -> Self {
        self.diagnostic = Some(Box::new(diagnostic));
        self
    }
    pub fn with_cause_code(mut self, cause_code: &'static str) -> Self {
        self.cause_code = cause_code;
        self
    }
    pub fn transport(mode: PgCommandMode, code: &'static str, detail: impl Into<String>) -> Self {
        let evidence = PgCommandEvidence::OutcomeUnknown;
        Self::new(
            if mode == PgCommandMode::Write {
                crate::ERR_OPERATION_OUTCOME_UNKNOWN
            } else {
                code
            },
            detail,
            evidence,
        )
        .with_cause_code(code)
    }
    /// Convert a credential-bearing exec failure without exposing raw argv,
    /// SQL or transport text. The caller supplies a fixed stage summary;
    /// original identities and dispatch/retry evidence remain authoritative.
    pub fn into_credential_app_error(
        self,
        stage: &'static str,
        safe_summary: &'static str,
    ) -> crate::AppError {
        self.with_credential_summary(safe_summary)
            .into_app_error(stage)
    }

    /// Apply the same credential boundary before retaining a domain error.
    /// A peer diagnostic may contain only a shell-escaped secret fragment.
    pub fn with_credential_summary(mut self, safe_summary: &'static str) -> Self {
        self.detail = safe_summary.into();
        if let Some(origin) = self.diagnostic.as_mut()
            && let Some(detail) = origin.error_detail.as_mut()
        {
            detail.detail = safe_summary.into();
            detail.hint =
                crate::get_error_hint(&detail.reason_code, crate::current_request_locale());
            if matches!(
                self.code,
                crate::ERR_OPERATION_OUTCOME_UNKNOWN | crate::ERR_RECOVERY_REQUIRED
            ) {
                detail.retryable = false;
            }
        }
        self
    }
    pub fn into_app_error(self, stage: &'static str) -> crate::AppError {
        let retryable = matches!(
            self.code,
            crate::ERR_RUNTIME_UNAVAILABLE | crate::ERR_RUNTIME_TIMEOUT
        );
        if let Some(diagnostic) = self.diagnostic {
            return diagnostic.into_app_error(
                self.code,
                self.cause_code,
                stage,
                &self.detail,
                retryable,
            );
        }
        crate::AppError::with_message(self.code, self.detail.clone()).with_error_detail(
            crate::ErrorDetail::new(self.cause_code, stage, self.detail).with_retryable(retryable),
        )
    }
}
impl std::fmt::Display for PgCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}
impl std::error::Error for PgCommandError {}

/// For our generated single-statement psql commands with ON_ERROR_STOP=1:
/// status 3 acknowledges a SQL rejection; transport/fatal/signal exits do not
/// prove rollback. This contract is never applied to arbitrary shell scripts.
pub fn single_statement_write_evidence(exit_code: i64) -> PgCommandEvidence {
    if exit_code == 3 {
        PgCommandEvidence::DefinitivelyRejected
    } else {
        PgCommandEvidence::OutcomeUnknown
    }
}

/// 容器内 shell 命令执行通道（宿主注入）：
/// - prod：app_manager 经 runtime exec（Userapp 运行容器）
/// - dev：rcoder 经开发容器内 file-server `execute-command`（HTTP）
#[async_trait]
pub trait PgCommandRunner: Send + Sync {
    async fn run(
        &self,
        command: &str,
        mode: PgCommandMode,
    ) -> Result<CommandOutcome, PgCommandError>;
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
    Command {
        stage: &'static str,
        code: &'static str,
        cause_code: &'static str,
        detail: String,
        mutation: CredentialMutationEvidence,
        diagnostic: Option<Box<PgCommandDiagnostic>>,
    },
}

impl std::fmt::Display for AlignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(m) => write!(f, "{m}"),
            Self::RoleMissing(m) => write!(f, "{m}"),
            Self::Command { stage, detail, .. } => write!(f, "{stage}: {detail}"),
        }
    }
}

impl AlignError {
    pub fn mutation_evidence(&self) -> CredentialMutationEvidence {
        match self {
            Self::InvalidInput(_) | Self::RoleMissing(_) => {
                CredentialMutationEvidence::NotAttempted
            }
            Self::Command { mutation, .. } => *mutation,
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
    align_pg_credentials_inner(runner, username, password, None).await
}

/// Versioned execution uses a captured administrator, never the business env.
pub async fn align_pg_credentials_with_admin(
    runner: &dyn PgCommandRunner,
    admin: &crate::pg_utils::PgAdministrationTarget,
    username: &str,
    password: &str,
) -> Result<AlignCredentialsOutcome, AlignError> {
    align_pg_credentials_inner(runner, username, password, Some(admin)).await
}

async fn align_pg_credentials_inner(
    runner: &dyn PgCommandRunner,
    username: &str,
    password: &str,
    admin: Option<&crate::pg_utils::PgAdministrationTarget>,
) -> Result<AlignCredentialsOutcome, AlignError> {
    validate_pg_identifier(username).map_err(AlignError::InvalidInput)?;
    if password.is_empty() {
        return Err(AlignError::InvalidInput(
            "password must not be empty".to_string(),
        ));
    }

    // 1. 验证（TCP scram）：exit 0 = 一致，直接返回
    let verify = runner
        .run(
            &pg_verify_credentials_cmd(username, password),
            PgCommandMode::ReadOnly,
        )
        .await
        .map_err(|error| AlignError::Command {
            diagnostic: error.diagnostic,
            code: error.code,
            cause_code: error.cause_code,
            stage: "verify credentials",
            mutation: CredentialMutationEvidence::NotAttempted,
            // Exec transports may echo argv, including shell-escaped passwords.
            // Replacing the raw password cannot reliably redact those encodings.
            detail: "Credential verification transport failed".into(),
        })?;
    if verify.exit_code == 0 {
        return Ok(AlignCredentialsOutcome {
            aligned: true,
            reset_performed: false,
        });
    }

    // 2. 不一致 → 角色存在检查（区分"密码不同"与"账号不存在"，后者明确报错）
    let exists = runner
        .run(
            &match admin {
                Some(admin) => admin
                    .role_exists_command(username)
                    .map_err(AlignError::InvalidInput)?,
                None => pg_role_exists_cmd(username),
            },
            PgCommandMode::ReadOnly,
        )
        .await
        .map_err(|e| AlignError::Command {
            diagnostic: e.diagnostic,
            code: e.code,
            cause_code: e.cause_code,
            stage: "role-exists check",
            mutation: CredentialMutationEvidence::NotAttempted,
            detail: crate::sanitize_error_text(&e.detail),
        })?;
    if exists.exit_code != 0 {
        return Err(AlignError::Command {
            diagnostic: None,
            code: crate::ERR_DATABASE_COMMAND_FAILED,
            cause_code: crate::ERR_DATABASE_COMMAND_FAILED,
            stage: "role-exists check",
            mutation: CredentialMutationEvidence::NotAttempted,
            detail: exists.stderr.trim().to_string(),
        });
    }
    let missing = exists.stdout.trim() != "1";
    if missing && admin.is_none() {
        return Err(AlignError::RoleMissing(format!(
            "PG role `{username}` does not exist; create it first (align only resets passwords)"
        )));
    }

    // Versioned execution can provision a missing role through its captured
    // administrator. The legacy entry point deliberately keeps reset-only semantics.
    let mutation_command = match admin {
        Some(admin) if missing => admin.create_role_command(username, password),
        Some(admin) => admin.alter_password_command(username, password),
        None => Ok(pg_alter_password_cmd(username, password)),
    }
    .map_err(AlignError::InvalidInput)?;
    let alter = runner
        .run(&mutation_command, PgCommandMode::Write)
        .await
        .map_err(|error| AlignError::Command {
            diagnostic: error.diagnostic,
            code: error.code,
            cause_code: error.cause_code,
            stage: "apply credentials",
            mutation: if error.evidence == PgCommandEvidence::OutcomeUnknown {
                CredentialMutationEvidence::Unknown
            } else {
                CredentialMutationEvidence::NotAttempted
            },
            detail: "Credential write completion was not confirmed".into(),
        })?;
    if alter.exit_code != 0 {
        return Err(AlignError::Command {
            diagnostic: None,
            code: crate::ERR_DATABASE_COMMAND_FAILED,
            cause_code: crate::ERR_DATABASE_COMMAND_FAILED,
            stage: "apply credentials",
            mutation: if single_statement_write_evidence(alter.exit_code)
                == PgCommandEvidence::DefinitivelyRejected
            {
                CredentialMutationEvidence::NotAttempted
            } else {
                CredentialMutationEvidence::Unknown
            },
            detail: format!("Credential write exited with status {}", alter.exit_code),
        });
    }

    // 4. 复验（scram 确认生效）
    let reverify = runner
        .run(
            &pg_verify_credentials_cmd(username, password),
            PgCommandMode::ReadOnly,
        )
        .await
        .map_err(|error| AlignError::Command {
            diagnostic: error.diagnostic,
            code: error.code,
            cause_code: error.cause_code,
            stage: "re-verify after reset",
            mutation: CredentialMutationEvidence::AppliedButUnverified,
            detail: "Password update completed but verification transport failed".into(),
        })?;
    if reverify.exit_code != 0 {
        return Err(AlignError::Command {
            diagnostic: None,
            code: crate::ERR_DATABASE_COMMAND_FAILED,
            cause_code: crate::ERR_DATABASE_COMMAND_FAILED,
            stage: "re-verify after reset",
            mutation: CredentialMutationEvidence::AppliedButUnverified,
            detail: format!(
                "Credential verification exited with status {}",
                reverify.exit_code
            ),
        });
    }

    Ok(AlignCredentialsOutcome {
        aligned: true,
        reset_performed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// wire 契约：app_id/username/password 必填——缺字段即拒。
    #[test]
    fn align_request_requires_core_fields() {
        let ok: AlignCredentialsRequest = serde_json::from_value(serde_json::json!({
            "app_id": "app-1", "username": "u", "password": "p",
        }))
        .expect("full body deserializes");
        assert_eq!(ok.app_id, "app-1");
        for missing in [
            serde_json::json!({"username": "u", "password": "p"}),
            serde_json::json!({"app_id": "app-1", "password": "p"}),
            serde_json::json!({"app_id": "app-1", "username": "u"}),
        ] {
            assert!(serde_json::from_value::<AlignCredentialsRequest>(missing).is_err());
        }
    }

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
            if cmd.starts_with("env -u PGHOSTADDR -u PGSERVICE PGPASSWORD=") {
                "verify"
            } else if cmd.contains("pg_roles") {
                "role_exists"
            } else if cmd.contains("ALTER USER") {
                "alter"
            } else {
                "unknown"
            }
        }
    }

    #[async_trait]
    impl PgCommandRunner for ScriptedRunner {
        async fn run(
            &self,
            command: &str,
            mode: PgCommandMode,
        ) -> Result<CommandOutcome, PgCommandError> {
            self.seen.lock().unwrap().push(command.to_string());
            self.results.lock().unwrap().remove(0).map_err(|detail| {
                PgCommandError::transport(mode, crate::ERR_CONTAINER_EXEC_FAILED, detail)
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
    async fn captured_admin_provisions_missing_role_before_tcp_verification() {
        let runner =
            ScriptedRunner::new(vec![ok(2, ""), ok(0, ""), ok(0, "CREATE ROLE"), ok(0, "1")]);
        let admin = crate::pg_utils::PgAdministrationTarget::new(
            "initialadmin".into(),
            "/var/run/postgresql".into(),
        )
        .unwrap();
        let result = align_pg_credentials_with_admin(&runner, &admin, "business", "pa'ss")
            .await
            .unwrap();
        assert!(result.aligned && result.reset_performed);
        let commands = runner.seen.lock().unwrap();
        assert_eq!(commands.len(), 4);
        assert!(commands[2].contains("CREATE ROLE"));
        assert!(commands[2].contains("initialadmin"));
        assert!(!commands[2].contains("$POSTGRES_USER"));
        assert!(commands[3].starts_with("env -u PGHOSTADDR -u PGSERVICE PGPASSWORD="));
    }

    #[tokio::test]
    async fn unknown_role_creation_keeps_mutation_evidence_without_password() {
        let runner = ScriptedRunner::new(vec![
            ok(2, ""),
            ok(0, ""),
            Err("transport echoed privatepassword".into()),
        ]);
        let admin = crate::pg_utils::PgAdministrationTarget::new(
            "initialadmin".into(),
            "/var/run/postgresql".into(),
        )
        .unwrap();
        let error = align_pg_credentials_with_admin(&runner, &admin, "business", "privatepassword")
            .await
            .unwrap_err();
        assert_eq!(
            error.mutation_evidence(),
            CredentialMutationEvidence::Unknown
        );
        assert!(!error.to_string().contains("privatepassword"));
        assert_eq!(runner.seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn already_aligned_skips_reset() {
        let runner = ScriptedRunner::new(vec![ok(0, "1")]);
        let out = align_pg_credentials(&runner, "app", "pw").await.unwrap();
        assert!(out.aligned && !out.reset_performed);
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
        ]);
        let out = align_pg_credentials(&runner, "app", "pw").await.unwrap();
        assert!(out.aligned && out.reset_performed);
        let kinds: Vec<&str> = runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|c| ScriptedRunner::kind_of(c))
            .collect();
        assert_eq!(kinds, vec!["verify", "role_exists", "alter", "verify"]);
    }

    #[tokio::test]
    async fn explicit_administrator_is_used_only_for_management_commands() {
        let runner =
            ScriptedRunner::new(vec![ok(2, ""), ok(0, "1"), ok(0, "ALTER USER"), ok(0, "1")]);
        let admin = crate::pg_utils::PgAdministrationTarget::new(
            "initialadmin".into(),
            "/var/run/postgresql".into(),
        )
        .unwrap();
        let outcome = align_pg_credentials_with_admin(&runner, &admin, "business", "newpassword")
            .await
            .unwrap();
        assert!(outcome.aligned && outcome.reset_performed);
        let commands = runner.seen.lock().unwrap();
        assert_eq!(commands.len(), 4);
        for index in [0, 3] {
            assert!(commands[index].contains("-h 127.0.0.1"));
            assert!(commands[index].contains("-U \"business\""));
        }
        for index in [1, 2] {
            assert!(commands[index].contains("-U 'initialadmin'"));
            assert!(!commands[index].contains("$POSTGRES_USER"));
        }
    }

    #[tokio::test]
    async fn failures_preserve_password_mutation_evidence() {
        for (results, expected) in [
            (
                vec![Err("disconnected before verification".into())],
                CredentialMutationEvidence::NotAttempted,
            ),
            (
                vec![ok(2, ""), Err("role query disconnected".into())],
                CredentialMutationEvidence::NotAttempted,
            ),
            (
                vec![ok(2, ""), ok(0, "1"), Err("ALTER reply lost".into())],
                CredentialMutationEvidence::Unknown,
            ),
            (
                vec![ok(2, ""), ok(0, "1"), ok(1, "")],
                CredentialMutationEvidence::Unknown,
            ),
            (
                vec![
                    ok(2, ""),
                    ok(0, "1"),
                    ok(0, "ALTER USER"),
                    Err("verification disconnected".into()),
                ],
                CredentialMutationEvidence::AppliedButUnverified,
            ),
            (
                vec![ok(2, ""), ok(0, "1"), ok(0, "ALTER USER"), ok(2, "")],
                CredentialMutationEvidence::AppliedButUnverified,
            ),
        ] {
            let runner = ScriptedRunner::new(results);
            let error = align_pg_credentials(&runner, "app", "pw")
                .await
                .unwrap_err();
            assert_eq!(error.mutation_evidence(), expected);
            assert!(runner.results.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn request_debug_never_contains_password() {
        let request = AlignCredentialsRequest {
            app_id: "app1".into(),
            username: "runtimeuser".into(),
            password: "privatepassword".into(),
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("privatepassword"));
        assert!(debug.contains("<redacted>"));
    }

    #[tokio::test]
    async fn credential_command_errors_do_not_expose_escaped_passwords() {
        let password = "s'ecret";
        let leaked_command = "ALTER USER business PASSWORD 's''ecret'; shell 's'\\''ecret'";
        for replies in [
            vec![Err(leaked_command.into())],
            vec![ok(2, ""), ok(0, "1"), Err(leaked_command.into())],
            vec![
                ok(2, ""),
                ok(0, "1"),
                Ok(CommandOutcome {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: leaked_command.into(),
                }),
            ],
            vec![ok(2, ""), ok(0, "1"), ok(0, ""), Err(leaked_command.into())],
        ] {
            let runner = ScriptedRunner::new(replies);
            let error = align_pg_credentials(&runner, "business", password)
                .await
                .unwrap_err();
            let public = format!("{error} {error:?}");
            assert!(
                !public.contains("ecret"),
                "credential fragment leaked: {public}"
            );
        }
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
