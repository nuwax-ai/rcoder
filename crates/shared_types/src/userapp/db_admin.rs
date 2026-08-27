//! UserApp PG 账号/库管理契约（跨 crate，按模块契约约定置于 shared_types）。
//!
//! 业务：Java 调 `POST /api/v1/userapp/db/{env}/reset-password|create-database` 管理
//! userApp dev（UserAppBuilder 开发容器）/ prod（运行容器）的容器内 PG——
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

/// `POST /api/v1/userapp/db/{env}/reset-password` 请求体。
#[derive(Debug, Deserialize, Serialize, Clone, utoipa::ToSchema)]
pub struct UserappDbResetPasswordRequest {
    /// 应用 ID（定位 dev=开发容器 / prod=运行容器）
    pub app_id: String,
    /// 新密码（非空；允许任意字符含特殊符号）
    pub new_password: String,
    /// 目标账号名（可选，须过 PG 标识符白名单）：
    /// - 缺省：重置 superuser（`$POSTGRES_USER`，SQL CURRENT_USER 语义）
    /// - 指定：账号 upsert——角色已存在则 ALTER USER 改密，不存在则 CREATE ROLE 建号
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// `POST /api/v1/userapp/db/{env}/create-database` 请求体。
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
    Command { stage: &'static str, detail: String },
}

impl std::fmt::Display for DbAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(m) => write!(f, "{m}"),
            Self::AlreadyExists(m) => write!(f, "{m}"),
            Self::Command { stage, detail } => write!(f, "{stage}: {detail}"),
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
    validate_pg_identifier(username).map_err(DbAdminError::InvalidInput)?;
    if password.is_empty() {
        return Err(DbAdminError::InvalidInput(
            "password must not be empty".to_string(),
        ));
    }

    // 1. 角色存在检查（`-tAc`：命中输出 1、未命中输出空）
    let exists = runner
        .run(&pg_role_exists_cmd(username))
        .await
        .map_err(|e| DbAdminError::Command {
            stage: "role-exists check",
            detail: e,
        })?;
    if exists.exit_code != 0 {
        return Err(DbAdminError::Command {
            stage: "role-exists check",
            detail: exists.stderr.trim().to_string(),
        });
    }

    // 2. 存在 → ALTER 改密；不存在 → CREATE ROLE 建号（LOGIN + 密码，最小权限）
    let (cmd, outcome) = if exists.stdout.trim() == "1" {
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
    let applied = runner.run(&cmd).await.map_err(|e| DbAdminError::Command {
        stage: "apply user upsert",
        detail: e,
    })?;
    if applied.exit_code != 0 {
        return Err(DbAdminError::Command {
            stage: "apply user upsert",
            detail: applied.stderr.trim().to_string(),
        });
    }
    Ok(outcome)
}

/// 改密成功后的 dbx 预置连接同步（共用钩子：reset-password 两条分支调用）。
///
/// - `Some(username)`：指定账号版——命令条件内建，仅当账号 == 容器内
///   `$POSTGRES_USER`（local-pg 预置连接在用账号）时才重写+重启；重置业务
///   账号不动 local-pg。
/// - `None`：superuser 版——重置目标就是 `$POSTGRES_USER`，恒同步。
///
/// best-effort：失败仅返回 Err（调用方 warn 留痕，不阻断改密结果——密码已
/// 生效）；错误信息**不含密码**。`username` 须已过白名单（`upsert_pg_user`
/// 成功返回即保证）。
pub async fn sync_dbx_after_password_change(
    runner: &dyn super::db_align::PgCommandRunner,
    username: Option<&str>,
    password: &str,
) -> Result<(), String> {
    let cmd = match username {
        Some(u) => crate::userapp::dbx_sync::dbx_sync_cmd_for_user(u, password),
        None => crate::userapp::dbx_sync::dbx_sync_cmd_superuser(password),
    };
    let r = runner.run(&cmd).await?;
    if r.exit_code != 0 {
        return Err(format!("exit {}: {}", r.exit_code, r.stderr.trim()));
    }
    Ok(())
}

/// 建库核心流程（check-then-act：先 `pg_database` 存在性判定，已存在报 409 语义；
/// PG 不支持 CREATE DATABASE IF NOT EXISTS、也不能进事务/DO 块，故不靠失败后
/// 解析 stderr 文本判定——它随 PG 版本/locale 变，不稳定）。
pub async fn create_pg_database(
    runner: &dyn super::db_align::PgCommandRunner,
    database: &str,
    owner: Option<&str>,
) -> Result<(), DbAdminError> {
    validate_pg_identifier(database).map_err(DbAdminError::InvalidInput)?;
    if let Some(o) = owner {
        validate_pg_identifier(o).map_err(DbAdminError::InvalidInput)?;
    }

    let exists = runner
        .run(&pg_database_exists_cmd(database))
        .await
        .map_err(|e| DbAdminError::Command {
            stage: "database-exists check",
            detail: e,
        })?;
    if exists.exit_code != 0 {
        return Err(DbAdminError::Command {
            stage: "database-exists check",
            detail: exists.stderr.trim().to_string(),
        });
    }
    if exists.stdout.trim() == "1" {
        return Err(DbAdminError::AlreadyExists(format!(
            "database {database} already exists"
        )));
    }

    let created = runner
        .run(&pg_create_database_cmd(database, owner))
        .await
        .map_err(|e| DbAdminError::Command {
            stage: "create database",
            detail: e,
        })?;
    if created.exit_code != 0 {
        // 罕见竞态（检查时不存在、CREATE 时已被并发创建）：再查一次精确判定
        let recheck = runner
            .run(&pg_database_exists_cmd(database))
            .await
            .map_err(|e| DbAdminError::Command {
                stage: "database-exists recheck",
                detail: e,
            })?;
        if recheck.exit_code == 0 && recheck.stdout.trim() == "1" {
            return Err(DbAdminError::AlreadyExists(format!(
                "database {database} already exists"
            )));
        }
        return Err(DbAdminError::Command {
            stage: "create database",
            detail: created.stderr.trim().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::db_align::{CommandOutcome, PgCommandRunner};
    use super::*;
    use std::sync::Mutex;

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
            } else if cmd.contains("connections.json") {
                "dbx_sync"
            } else {
                "unknown"
            }
        }
    }

    #[async_trait::async_trait]
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
    async fn dbx_sync_hook_routes_by_username_presence() {
        // superuser 分支 → 无条件命令（无 if [）；指定账号 → 条件内建命令
        let runner = ScriptedRunner::new(vec![ok(0, "")]);
        sync_dbx_after_password_change(&runner, None, "pw")
            .await
            .unwrap();
        let superuser_cmd = runner.seen.lock().unwrap()[0].clone();
        assert!(superuser_cmd.starts_with("printf"), "got: {superuser_cmd}");
        assert!(!superuser_cmd.contains("if ["));

        let runner = ScriptedRunner::new(vec![ok(0, "")]);
        sync_dbx_after_password_change(&runner, Some("app"), "pw")
            .await
            .unwrap();
        let user_cmd = runner.seen.lock().unwrap()[0].clone();
        assert!(
            user_cmd.starts_with("if [ 'app' = \"$POSTGRES_USER\" ]"),
            "got: {user_cmd}"
        );
    }

    #[tokio::test]
    async fn dbx_sync_hook_failure_is_err_without_password() {
        let runner = ScriptedRunner::new(vec![ok(1, "dbx down")]);
        let err = sync_dbx_after_password_change(&runner, None, "secret-pw")
            .await
            .unwrap_err();
        assert!(err.starts_with("exit 1"), "got: {err}");
        assert!(!err.contains("secret-pw"), "错误信息不得含密码");
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
        // CREATE 失败（exit 1）但复检发现已被并发创建 → 409 语义而非 500
        let runner = ScriptedRunner::new(vec![ok(0, ""), ok(1, "already exists"), ok(0, "1")]);
        let err = create_pg_database(&runner, "mydb", None).await.unwrap_err();
        assert!(matches!(err, DbAdminError::AlreadyExists(_)));
    }
}
