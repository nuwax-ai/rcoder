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
    if name.is_empty() || name.len() > 63 {
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

/// PG 标识符转义 — 标识符里 `"` → `""` (配合双引号引用)
pub fn pg_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// PG 就绪等待命令（容器内 `pg_isready` 轮询，`timeout_secs` 预算）——
/// 唤醒/刚建的容器 phase=Running 不等于容器内 PG 已可连（initdb/启动窗口），
/// 改密类 exec 前置执行可避免竞态。exit 0=就绪；超时 exit 1（stderr 有原因）。
pub fn pg_wait_ready_cmd(timeout_secs: usize) -> String {
    format!(
        "for i in $(seq 1 {timeout_secs}); do pg_isready -q >/dev/null 2>&1 && exit 0; sleep 1; done; \
echo 'postgres not ready' >&2; exit 1"
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
        "PGPASSWORD={} psql -h 127.0.0.1 -U {} -d postgres -tAc 'SELECT 1'",
        pg_shell_quote(password),
        pg_quote_ident(username)
    )
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
        assert!(cmd.starts_with("PGPASSWORD='s3cret' psql -h 127.0.0.1"));
        assert!(cmd.contains(r#"-U "app""#));
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
