//! PostgreSQL 标识符与字面量的校验 / 转义工具
//!
//! PG 标识符规则 (SQL 标准 + PG 扩展):
//! - 长度 1..=63 字节 (与 PostgreSQL `NAMEDATALEN - 1` 一致；超长时显式报错)
//! - 首字符: 字母 (a-zA-Z) 或下划线 `_`
//! - 后续字符: 字母 / 数字 / 下划线
//! - 大小写保留 (加双引号时不折叠为小写)
//!
//! 防御原则: 白名单校验为主 + SQL 转义 (replace) 为纵深防御。两者缺一会增加注入面。

use std::result::Result;

/// PG 标识符校验 — 白名单, 拒绝即报错
///
/// 规则: `[a-zA-Z_][a-zA-Z0-9_]*`, 长度 1..=63 字节。
/// `str::len()` 返回字节数；当前白名单只允许 ASCII，因此也等于字符数。
pub fn validate_pg_identifier(name: &str) -> Result<(), String> {
    if name.is_empty() {
        // 固定文案：避免 format! 展开（Kani 证明路径 / Fail Fast）
        return Err("PG identifier must be 1..=63 bytes, got 0".to_string());
    }
    if name.len() > 63 {
        return Err(format!(
            "PG identifier must be 1..=63 bytes, got {len}",
            len = name.len()
        ));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("PG identifier must not be empty".to_string());
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err("PG identifier must start with letter or '_'".to_string());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("PG identifier: only [a-zA-Z0-9_] allowed after the first char".to_string());
    }
    Ok(())
}

/// PG SQL 字符串字面量转义 — 标准 escape `'` → `''`
///
/// 注意: 仅转义单引号。完整防注入需配合 validate 白名单使用；
/// 不能防御 `\` 在 standard_conforming_strings=off 模式下的歧义, 但 PG 14+ 默认 on。
pub fn pg_escape_literal(value: &str) -> String {
    value.replace('\'', "''")
}

// Explicit E strings remain correct even if standard_conforming_strings is off.
fn pg_explicit_literal(value: &str) -> String {
    format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

fn validate_password_operation(
    context: &crate::UserAppExecutionContext,
    scope: crate::UserAppOperationScope,
    username: &str,
) -> Result<&'static str, String> {
    context.validate_identity(&context.app_id)?;
    validate_pg_identifier(username)?;
    match scope {
        crate::UserAppOperationScope::Dev => Ok("dev"),
        crate::UserAppOperationScope::Prod => Ok("prod"),
        crate::UserAppOperationScope::Application => {
            Err("Database writes require dev or prod scope".into())
        }
    }
}

/// PG 标识符转义 — 标识符里 `"` → `""` (配合双引号引用)
pub fn pg_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// PG 就绪等待命令（容器内轮询，`timeout_secs` 预算）——
/// 唤醒/刚建的容器 phase=Running 不等于容器内 PG 已可连（initdb/启动窗口），
/// 改密类 exec 前置执行可避免竞态。exit 0=就绪；超时 exit 1（stderr 有原因）。
///
/// 就绪口径 = **业务库可连**（`psql -d "$POSTGRES_DB"` 2xx）而非仅 socket 在听：
/// PG init 中间态（initdb 完成 socket 已监听、业务库 createdb 尚未跑完/曾静默
/// 失败）下 `pg_isready` 会误判就绪，随后业务查询 FATAL "database does not
/// exist"。容器 ENV（POSTGRES_USER/POSTGRES_DB）经 sh -c 展开在。
pub fn pg_wait_ready_cmd(timeout_secs: usize) -> String {
    format!(
        "for i in $(seq 1 {timeout_secs}); do \
psql -h /var/run/postgresql -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" -tAc 'select 1' >/dev/null 2>&1 && exit 0; \
sleep 1; done; \
echo \"postgres or business db $POSTGRES_DB not ready\" >&2; exit 1"
    )
}

// ── 容器内 PG 凭据对齐命令构造（userApp dev/prod 双环境共用；经各自执行通道跑 sh -c） ──

/// sh 单引号安全包裹（`'` → `'\''`）——密码等自由文本进 shell 环境变量的标准转义。
pub fn pg_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 凭据验证命令（TCP scram 认证）：exit 0 = 传入密码与该账号当前密码一致。
///
/// 走 `-h 127.0.0.1` 强制 TCP（镜像 initdb `--auth-host=scram-sha-256`），
/// 不落 trust 通道；`username` 须先过 [`validate_pg_identifier`] 白名单。
pub fn pg_verify_credentials_cmd(username: &str, password: &str) -> String {
    format!(
        "env -u PGHOSTADDR -u PGSERVICE PGPASSWORD={} PGCONNECT_TIMEOUT=5 PGOPTIONS='-c statement_timeout=5000' psql -X -w -h 127.0.0.1 -U {} -d postgres -v ON_ERROR_STOP=1 -tAc 'SELECT 1'",
        pg_shell_quote(password),
        pg_quote_ident(username)
    )
}

/// Explicit local administration channel, independent from business POSTGRES_USER.
/// Its username must come from the captured PG initialization identity.
#[derive(Debug, Clone)]
pub struct PgAdministrationTarget {
    username: String,
    socket_directory: String,
}

impl PgAdministrationTarget {
    pub fn new(username: String, socket_directory: String) -> Result<Self, String> {
        validate_pg_identifier(&username)?;
        if !socket_directory.starts_with('/') || socket_directory.contains('\0') {
            return Err("PostgreSQL management socket must be an absolute path".into());
        }
        Ok(Self {
            username,
            socket_directory,
        })
    }

    fn command(&self, sql: &str) -> String {
        self.command_with_options(sql, "-c statement_timeout=5000")
    }

    fn command_with_options(&self, sql: &str, options: &str) -> String {
        format!(
            "env -u PGHOSTADDR -u PGSERVICE PGCONNECT_TIMEOUT=5 PGOPTIONS={} psql -X -w -h {} -U {} -d postgres -v ON_ERROR_STOP=1 -qAtc {}",
            pg_shell_quote(options),
            pg_shell_quote(&self.socket_directory),
            pg_shell_quote(&self.username),
            pg_shell_quote(sql)
        )
    }

    pub fn ready_command(&self) -> String {
        self.command("SELECT 1")
    }

    /// Database bootstrap is asynchronous in the image. Verify the business
    /// database through the management socket before applying credentials.
    /// Neither business authentication nor application readiness is required.
    pub fn business_database_ready_command(&self) -> String {
        format!(
            "test -n \"${{POSTGRES_DB:-}}\" && env -u PGHOSTADDR -u PGSERVICE PGCONNECT_TIMEOUT=5 PGOPTIONS='-c statement_timeout=5000' psql -X -w -h {} -U {} -d \"$POSTGRES_DB\" -v ON_ERROR_STOP=1 -tAc 'SELECT 1'",
            pg_shell_quote(&self.socket_directory),
            pg_shell_quote(&self.username)
        )
    }

    /// Password mutation and its immutable receipt commit in the same PG
    /// transaction. A late duplicate observes the original writer transaction
    /// and cannot overwrite a later operation's password.
    pub fn password_operation_command(
        &self,
        context: &crate::UserAppExecutionContext,
        scope: crate::UserAppOperationScope,
        username: &str,
        password: &str,
        create: bool,
    ) -> Result<String, String> {
        let scope = validate_password_operation(context, scope, username)?;
        if password.is_empty() || password.contains('\0') {
            return Err("Password must be nonempty and contain no NUL".into());
        }
        let mut sql = String::from(include_str!("pg_password_receipt_schema.sql"));
        sql.push_str("\nBEGIN ISOLATION LEVEL READ COMMITTED;\n");
        for (key, value) in [
            ("app_id", context.app_id.as_str()),
            ("lifecycle_id", context.lifecycle_id.as_str()),
            ("operation_id", context.operation_id.as_str()),
            ("fingerprint", context.request_fingerprint.as_str()),
            ("scope", scope),
            ("username", username),
            ("private_password", password),
            ("create_role", if create { "true" } else { "false" }),
        ] {
            sql.push_str(&format!(
                "SET LOCAL rcoder.{key} = {};\n",
                pg_explicit_literal(value)
            ));
        }
        sql.push_str(include_str!("pg_password_receipt.sql"));
        sql.push_str("\nCOMMIT;");
        // Session-local log settings prevent a failed compound statement from
        // echoing its private input to the default PostgreSQL error log. The
        // captured initialization administrator has permission to set these.
        Ok(self.command_with_options(&sql,
            "-c statement_timeout=5000 -c idle_in_transaction_session_timeout=5000 -c log_statement=none -c log_min_error_statement=panic -c log_parameter_max_length_on_error=0"))
    }

    /// A missing/failed query is not evidence of rollback. Recovery may only use
    /// a matching committed row; the table deliberately stores no password.
    pub fn password_operation_receipt_command(
        &self,
        context: &crate::UserAppExecutionContext,
        scope: crate::UserAppOperationScope,
        username: &str,
    ) -> Result<String, String> {
        let scope = validate_password_operation(context, scope, username)?;
        Ok(self.command(&format!(
            "SELECT 1 FROM rcoder_management.password_receipts WHERE app_id={} AND lifecycle_id={} AND scope={} AND operation_id={} AND request_fingerprint={} AND target_username={} AND outcome='committed'",
            pg_explicit_literal(&context.app_id), pg_explicit_literal(&context.lifecycle_id),
            pg_explicit_literal(scope), pg_explicit_literal(&context.operation_id),
            pg_explicit_literal(&context.request_fingerprint), pg_explicit_literal(username),
        )))
    }

    /// Fence a delayed writer using the original identity. Callers must retain
    /// the control-plane lease until the transaction has committed and the
    /// returned outcome has been verified. Empty output is an identity conflict.
    pub fn password_operation_cancel_command(
        &self,
        context: &crate::UserAppExecutionContext,
        scope: crate::UserAppOperationScope,
        username: &str,
    ) -> Result<String, String> {
        let scope = validate_password_operation(context, scope, username)?;
        let mut sql = String::from(include_str!("pg_password_receipt_schema.sql"));
        sql.push_str("\nBEGIN ISOLATION LEVEL READ COMMITTED;\n");
        for (key, value) in [
            ("app_id", context.app_id.as_str()),
            ("lifecycle_id", context.lifecycle_id.as_str()),
            ("operation_id", context.operation_id.as_str()),
            ("fingerprint", context.request_fingerprint.as_str()),
            ("scope", scope),
            ("username", username),
        ] {
            sql.push_str(&format!(
                "SET LOCAL rcoder.{key} = {};\n",
                pg_explicit_literal(value)
            ));
        }
        sql.push_str(include_str!("pg_password_cancel.sql"));
        sql.push_str("\nCOMMIT;");
        Ok(self.command_with_options(
            &sql,
            "-c statement_timeout=5000 -c idle_in_transaction_session_timeout=5000",
        ))
    }

    pub fn role_exists_command(&self, username: &str) -> Result<String, String> {
        validate_pg_identifier(username)?;
        Ok(self.command(&format!(
            "SELECT 1 FROM pg_roles WHERE rolname='{}'",
            pg_escape_literal(username)
        )))
    }

    /// Provision a missing business role through the captured initialization identity.
    /// Callers must hold the lifecycle mutation fence before dispatching this command.
    pub fn create_role_command(&self, username: &str, password: &str) -> Result<String, String> {
        validate_pg_identifier(username)?;
        if password.is_empty() {
            return Err("password must not be empty".into());
        }
        Ok(self.command(&format!(
            "CREATE ROLE {} LOGIN PASSWORD '{}'",
            pg_quote_ident(username),
            pg_escape_literal(password)
        )))
    }

    pub fn alter_password_command(&self, username: &str, password: &str) -> Result<String, String> {
        validate_pg_identifier(username)?;
        if password.is_empty() {
            return Err("password must not be empty".into());
        }
        Ok(self.command(&format!(
            "ALTER USER {} WITH PASSWORD '{}'",
            pg_quote_ident(username),
            pg_escape_literal(password)
        )))
    }
}

/// 角色存在检查命令（本地 trust 免密，`$POSTGRES_USER` 为镜像 ENV）。
/// `username` 须先过 [`validate_pg_identifier`] 白名单（调用方保证）；
/// SQL 参数整体单引号包裹作纵深防御。
pub fn pg_role_exists_cmd(username: &str) -> String {
    let sql = format!(
        "SELECT 1 FROM pg_roles WHERE rolname='{}'",
        pg_escape_literal(username)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -tAc {}",
        pg_shell_quote(&sql)
    )
}

/// 密码重置命令（本地 trust 免密 ALTER USER；任意已存在账号）。
///
/// `-c` 的 SQL 参数整体经 [`pg_shell_quote`] 单引号包裹——密码是自由文本，
/// 不能落 shell 双引号（`$`/反引号/`"` 在双引号内保持活性：注入 + 含特殊字符
/// 的密码先被 shell 改写、复验必失败的密码损坏双重问题）。SQL 串内的 `'` 已由
/// [`pg_escape_literal`] 转为 `''`，在 shell 单引号内安全。
pub fn pg_alter_password_cmd(username: &str, password: &str) -> String {
    let sql = format!(
        "ALTER USER {} WITH PASSWORD '{}'",
        pg_quote_ident(username),
        pg_escape_literal(password)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
        pg_shell_quote(&sql)
    )
}

/// 超户自身密码重置命令（本地 trust；重置目标 = 当前连接用户 = `$POSTGRES_USER`）。
///
/// 用 SQL 的 `CURRENT_USER` 取代把 `"$POSTGRES_USER"` 内嵌进命令行——后者依赖
/// shell 双引号开合的巧合展开（POSTGRES_USER 含空格还会分词），前者由 psql 会话
/// 身份直接解析，无变量展开依赖。
pub fn pg_alter_current_user_password_cmd(password: &str) -> String {
    let sql = format!(
        "ALTER USER CURRENT_USER WITH PASSWORD '{}'",
        pg_escape_literal(password)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
        pg_shell_quote(&sql)
    )
}

/// 建号命令（本地 trust `CREATE ROLE ... LOGIN`；userApp 账号 upsert 的"不存在"分支）。
///
/// `username` 须先过 [`validate_pg_identifier`] 白名单（调用方保证）；
/// 标识符经 [`pg_quote_ident`]、密码字面量经 [`pg_escape_literal`]、SQL 整体经
/// [`pg_shell_quote`] 单引号包裹——三层防线与 [`pg_alter_password_cmd`] 同款。
pub fn pg_create_role_cmd(username: &str, password: &str) -> String {
    let sql = format!(
        "CREATE ROLE {} LOGIN PASSWORD '{}'",
        pg_quote_ident(username),
        pg_escape_literal(password)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
        pg_shell_quote(&sql)
    )
}

/// 库存在性检查命令（本地 trust `-tAc`：命中输出 `1`、未命中输出空——
/// 比 CREATE 失败后解析 stderr 稳定，PG 不支持 CREATE DATABASE IF NOT EXISTS）。
/// `db` 须先过 [`validate_pg_identifier`] 白名单；SQL 参数整体单引号包裹作纵深防御。
pub fn pg_database_exists_cmd(db: &str) -> String {
    let sql = format!(
        "SELECT 1 FROM pg_database WHERE datname='{}'",
        pg_escape_literal(db)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -tAc {}",
        pg_shell_quote(&sql)
    )
}

/// 建库命令（本地 trust `CREATE DATABASE`，可选 OWNER）。
/// `db`/`owner` 须先过 [`validate_pg_identifier`] 白名单（调用方保证）。
pub fn pg_create_database_cmd(db: &str, owner: Option<&str>) -> String {
    let owner_clause = owner
        .map(|o| format!(" OWNER {}", pg_quote_ident(o)))
        .unwrap_or_default();
    let sql = format!("CREATE DATABASE {}{owner_clause}", pg_quote_ident(db));
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
        pg_shell_quote(&sql)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn business_database_readiness_uses_captured_admin_and_business_database() {
        let admin =
            PgAdministrationTarget::new("initialadmin".into(), "/var/run/postgresql".into())
                .unwrap();
        let command = admin.business_database_ready_command();
        assert!(command.contains("-U 'initialadmin'"));
        assert!(command.contains("-d \"$POSTGRES_DB\""));
        assert!(command.contains("test -n \"${POSTGRES_DB:-}\""));
        assert!(!command.contains("$POSTGRES_USER"));
        assert!(!command.contains("127.0.0.1"));
    }

    #[test]
    fn validate_ok() {
        assert!(validate_pg_identifier("my_db").is_ok());
        assert!(validate_pg_identifier("_hidden").is_ok());
        assert!(validate_pg_identifier("Db123").is_ok());
        assert!(validate_pg_identifier(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_pg_identifier("").is_err());
    }

    #[test]
    fn validate_rejects_too_long() {
        assert!(validate_pg_identifier(&"a".repeat(64)).is_err());
    }

    #[test]
    fn validate_rejects_start_digit() {
        assert!(validate_pg_identifier("1db").is_err());
    }

    #[test]
    fn validate_rejects_dash() {
        assert!(validate_pg_identifier("my-db").is_err());
    }

    #[test]
    fn validate_rejects_space() {
        assert!(validate_pg_identifier("my db").is_err());
    }

    #[test]
    fn validate_rejects_injection() {
        assert!(validate_pg_identifier("foo'; DROP TABLE users;--").is_err());
        assert!(validate_pg_identifier("$(whoami)").is_err());
        assert!(validate_pg_identifier("`id`").is_err());
        assert!(validate_pg_identifier("\"").is_err());
    }

    #[test]
    fn escape_literal() {
        assert_eq!(pg_escape_literal("it's"), "it''s");
        assert_eq!(pg_escape_literal("no_quote"), "no_quote");
        assert_eq!(pg_escape_literal("' OR '1'='1"), "'' OR ''1''=''1");
    }

    #[test]
    fn quote_ident() {
        assert_eq!(pg_quote_ident("my db"), "\"my db\"");
        assert_eq!(pg_quote_ident("weird\"name"), "\"weird\"\"name\"");
    }

    #[test]
    fn shell_quote_escapes_single_quote() {
        assert_eq!(pg_shell_quote("plain"), "'plain'");
        assert_eq!(pg_shell_quote("it's"), r"'it'\''s'");
        assert_eq!(pg_shell_quote("a'; rm -rf /"), r"'a'\''; rm -rf /'");
    }

    #[test]
    fn verify_cmd_forces_tcp_scram() {
        let cmd = pg_verify_credentials_cmd("app", "s3cret");
        assert!(
            cmd.starts_with(
                "env -u PGHOSTADDR -u PGSERVICE PGPASSWORD='s3cret' PGCONNECT_TIMEOUT=5"
            )
        );
        assert!(cmd.contains("psql -X -w -h 127.0.0.1"));
        assert!(cmd.contains("statement_timeout=5000"));
        assert!(cmd.contains("ON_ERROR_STOP=1"));
        assert!(cmd.contains(r#"-U "app""#));
    }

    fn receipt_context() -> crate::UserAppExecutionContext {
        crate::UserAppExecutionContext {
            app_id: "receiptapp".into(),
            lifecycle_id: "receiptlife".into(),
            operation_id: "receiptop".into(),
            executor_id: "receiptworker".into(),
            request_fingerprint: "a".repeat(64),
        }
    }

    #[test]
    fn password_receipt_rejects_invalid_input_before_execution() {
        let admin =
            PgAdministrationTarget::new("admin".into(), "/var/run/postgresql".into()).unwrap();
        let context = receipt_context();
        for password in ["", "bad\0password"] {
            assert!(
                admin
                    .password_operation_command(
                        &context,
                        crate::UserAppOperationScope::Prod,
                        "business",
                        password,
                        false
                    )
                    .is_err()
            );
        }
        assert!(
            admin
                .password_operation_command(
                    &context,
                    crate::UserAppOperationScope::Application,
                    "business",
                    "fixture",
                    false
                )
                .is_err()
        );
        assert!(
            admin
                .password_operation_receipt_command(
                    &context,
                    crate::UserAppOperationScope::Prod,
                    "bad-account"
                )
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn password_receipt_shell_preserves_sql_and_private_literal() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let psql = directory.path().join("psql");
        std::fs::write(
            &psql,
            "#!/bin/sh\nfor arg; do last=$arg; done\nprintf '%s' \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&psql, std::fs::Permissions::from_mode(0o700)).unwrap();
        let password = "fixture'\\$rcoder_password_operation$; $(touch unwanted)";
        let admin =
            PgAdministrationTarget::new("admin".into(), "/var/run/postgresql".into()).unwrap();
        let command = admin
            .password_operation_command(
                &receipt_context(),
                crate::UserAppOperationScope::Prod,
                "business",
                password,
                false,
            )
            .unwrap();
        let result = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(directory.path())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", directory.path().display()),
            )
            .output()
            .unwrap();
        assert!(result.status.success());
        let sql = String::from_utf8(result.stdout).unwrap();
        assert!(sql.starts_with(include_str!("pg_password_receipt_schema.sql")));
        assert!(sql.contains("BEGIN ISOLATION LEVEL READ COMMITTED;"));
        assert!(sql.ends_with("COMMIT;"));
        assert!(sql.contains(&format!(
            "SET LOCAL rcoder.private_password = {};",
            pg_explicit_literal(password)
        )));
        assert!(sql.contains(include_str!("pg_password_receipt.sql")));
        assert!(!directory.path().join("unwanted").exists());
        assert!(!include_str!("pg_password_receipt.sql").contains("FOR UPDATE"));
    }

    #[cfg(unix)]
    #[test]
    fn postgres_commands_unset_service_instead_of_selecting_empty_service() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("psql");
        std::fs::write(
            &executable,
            "#!/bin/sh\ntest -z \"${PGSERVICE+x}\" && test -z \"${PGHOSTADDR+x}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let admin =
            PgAdministrationTarget::new("admin".into(), "/var/run/postgresql".into()).unwrap();
        for command in [
            admin.ready_command(),
            admin.business_database_ready_command(),
            pg_verify_credentials_cmd("business", "password"),
        ] {
            let result = std::process::Command::new("sh")
                .args(["-c", &command])
                .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
                .env("POSTGRES_DB", "business")
                .env("PGSERVICE", "unrelated-service")
                .env("PGHOSTADDR", "192.0.2.1")
                .status()
                .unwrap();
            assert!(result.success());
        }
    }

    #[test]
    fn explicit_admin_never_uses_business_identity() {
        let admin =
            PgAdministrationTarget::new("initialadmin".into(), "/var/run/postgresql".into())
                .unwrap();
        for command in [
            admin.ready_command(),
            admin.role_exists_command("business").unwrap(),
            admin.alter_password_command("business", "pa'ss").unwrap(),
        ] {
            assert!(command.contains("-h '/var/run/postgresql' -U 'initialadmin'"));
            assert!(!command.contains("$POSTGRES_USER"));
            assert!(command.contains("-X -w"));
        }
        assert!(admin.alter_password_command("bad-role", "pw").is_err());
        assert!(PgAdministrationTarget::new("admin".into(), "localhost".into()).is_err());
    }

    #[test]
    fn alter_cmd_escapes_password_literal() {
        let cmd = pg_alter_password_cmd("app", "pa'ss");
        // 双层转义正确性：PG 层 ' → ''（SQL 串内），再经 shell 层整体单引号包裹
        // （' → '\''）——期望串用同一构造器合成，避免手算两层叠加
        let sql = format!(
            "ALTER USER {} WITH PASSWORD '{}'",
            pg_quote_ident("app"),
            pg_escape_literal("pa'ss")
        );
        assert!(cmd.contains(&pg_shell_quote(&sql)), "got: {cmd}");
    }

    #[test]
    fn alter_cmd_shell_quotes_sql_argument() {
        // shell 注入面：密码含 $/`/" 时不得在命令行保持活性——SQL 参数须整体单引号包裹。
        // 断言用运行时构造的串，避开 raw 字符串与嵌套引号的定界歧义。
        let password: String = ['a', '$', '`', 'b', '"', 'c'].into_iter().collect();
        let cmd = pg_alter_password_cmd("app", &password);
        let expected_prefix = format!("-c 'ALTER USER {} WITH PASSWORD '", pg_quote_ident("app"));
        assert!(
            cmd.contains(&expected_prefix),
            "SQL 参数未单引号包裹: {cmd}"
        );
        let double_quoted_sql = "-c \"ALTER".to_string();
        assert!(
            !cmd.contains(&double_quoted_sql),
            "不应再有双引号包 SQL: {cmd}"
        );
    }

    #[test]
    fn alter_current_user_cmd_has_no_variable_expansion_dependency() {
        let cmd = pg_alter_current_user_password_cmd("pw");
        assert!(cmd.contains("ALTER USER CURRENT_USER"));
        assert!(cmd.contains(r#"-c 'ALTER"#));
    }

    #[test]
    fn create_role_cmd_shape_and_escaping() {
        let cmd = pg_create_role_cmd("biz_user", "pa'ss");
        // SQL 层：标识符双引号 + 密码 ' → ''；shell 层整体单引号包裹
        let sql = format!(
            "CREATE ROLE {} LOGIN PASSWORD '{}'",
            pg_quote_ident("biz_user"),
            pg_escape_literal("pa'ss")
        );
        assert_eq!(
            cmd,
            format!(
                "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
                pg_shell_quote(&sql)
            )
        );
        // 注入面：密码里的 shell 元字符被整体关在单引号内
        assert!(cmd.contains(&pg_shell_quote(&sql)));
    }

    #[test]
    fn database_exists_cmd_uses_plain_output() {
        let cmd = pg_database_exists_cmd("mydb");
        // -tAc 纯输出（命中 1/未命中空），不靠 stderr 文本判定；
        // SQL 经 shell 层转义（' → '\''），期望串用同款构造器合成避免手算叠加
        let sql = format!(
            "SELECT 1 FROM pg_database WHERE datname='{}'",
            pg_escape_literal("mydb")
        );
        assert!(cmd.contains(" -tAc "));
        assert!(cmd.contains(&pg_shell_quote(&sql)));
    }

    #[test]
    fn create_database_cmd_owner_clause() {
        let bare = pg_create_database_cmd("mydb", None);
        assert!(bare.contains(r#"CREATE DATABASE "mydb""#));
        assert!(!bare.contains("OWNER"));
        let owned = pg_create_database_cmd("mydb", Some("biz_user"));
        assert!(owned.contains(r#"CREATE DATABASE "mydb" OWNER "biz_user""#));
    }
}

/// Kani 有界证明：转义闭合契约见
/// `specs/002-kani-high-value-proofs/contracts/quote-escaping.md`。
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    fn str_from_bytes(bytes: &[u8]) -> Option<&str> {
        let s = std::str::from_utf8(bytes).ok()?;
        if s.as_bytes().contains(&0) {
            return None;
        }
        Some(s)
    }

    /// sh 词法：单引号包裹 + `'\''` 内嵌引号 后，整串必须是一个 well-formed 词
    /// （扫描结束后必须回到 outside，且不出现未配对引号）。
    fn sh_single_quoted_well_formed(s: &str) -> bool {
        let b = s.as_bytes();
        if b.is_empty() {
            return false;
        }
        let mut i = 0usize;
        let mut in_single = false;
        let mut saw_word = false;
        while i < b.len() {
            if in_single {
                if b[i] == b'\'' {
                    in_single = false;
                }
                i += 1;
            } else if b[i] == b'\'' {
                in_single = true;
                saw_word = true;
                i += 1;
            } else if b[i] == b'\\' {
                // outside 单引号的 \X 转义（'\'' 中的 \' 落在这里）
                if i + 1 >= b.len() {
                    return false;
                }
                saw_word = true;
                i += 2;
            } else {
                // 裸字符（pg_shell_quote 产物不应出现，但 well-formed 仍允许）
                saw_word = true;
                i += 1;
            }
        }
        saw_word && !in_single
    }

    #[kani::proof]
    #[kani::unwind(10)]
    fn shell_quote_roundtrip() {
        let raw: [u8; 6] = kani::any();
        let mut i = 0usize;
        while i < raw.len() {
            kani::assume(raw[i] < 128);
            kani::assume(raw[i] != 0);
            i += 1;
        }
        let Some(value) = str_from_bytes(&raw) else {
            return;
        };
        let quoted = pg_shell_quote(value);
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
        assert!(
            sh_single_quoted_well_formed(&quoted),
            "pg_shell_quote output must be one well-formed sh word"
        );
    }

    #[kani::proof]
    #[kani::unwind(10)]
    fn quote_ident_closure() {
        let raw: [u8; 4] = kani::any();
        let mut i = 0usize;
        while i < raw.len() {
            kani::assume(raw[i] < 128);
            kani::assume(raw[i] != 0);
            i += 1;
        }
        let Some(name) = str_from_bytes(&raw) else {
            return;
        };
        let quoted = pg_quote_ident(name);
        let qb = quoted.as_bytes();
        assert!(qb.len() >= 2 && qb[0] == b'"' && qb[qb.len() - 1] == b'"');
        let mut i = 1usize;
        while i + 1 < qb.len() {
            if qb[i] == b'"' {
                // The partner must be inside the identifier, not its closing quote.
                assert!(i + 2 < qb.len() && qb[i + 1] == b'"');
                i += 2;
            } else {
                i += 1;
            }
        }
        assert_eq!(i, qb.len() - 1);
    }

    #[kani::proof]
    #[kani::unwind(10)]
    fn escape_literal_pairs() {
        let raw: [u8; 6] = kani::any();
        let mut i = 0usize;
        while i < raw.len() {
            kani::assume(raw[i] < 128);
            kani::assume(raw[i] != 0);
            i += 1;
        }
        let Some(value) = str_from_bytes(&raw) else {
            return;
        };
        let escaped = pg_escape_literal(value);
        let eb = escaped.as_bytes();
        let mut i = 0usize;
        let mut singles = 0usize;
        while i < eb.len() {
            if eb[i] == b'\'' {
                singles += 1;
            }
            i += 1;
        }
        assert_eq!(singles % 2, 0, "literal quotes must appear in pairs");
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn pg_identifier_rejects_non_whitelist() {
        assert!(validate_pg_identifier("").is_err());
        assert!(validate_pg_identifier("-b").is_err());
        assert!(validate_pg_identifier("2b").is_err());
        assert!(validate_pg_identifier("a.b").is_err());
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn pg_identifier_accepts_alnum() {
        assert!(validate_pg_identifier("a").is_ok());
        assert!(validate_pg_identifier("_").is_ok());
        assert!(validate_pg_identifier("a_b1").is_ok()); // 本函数白名单不含 `-`，与 validate_identifier 不同
    }
}

/// 吞吐探针：无堆分配，用于测量 crate 级 kani 编译/求解成本。
#[cfg(kani)]
mod kani_probe {
    #[kani::proof]
    #[kani::unwind(4)]
    fn empty_identifier_rejected() {
        assert!(super::validate_pg_identifier("").is_err());
    }
}
